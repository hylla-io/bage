// Package session implements Båge's FILE-LEG two-phase, region-anchored,
// concurrency-safe edit protocol (SPEC §8, ADR-0003).
//
// Prepare is OPTIMISTIC: it holds no lock, reads each live file, resolves every
// region-anchored edit against those bytes (rejecting a Conflict/Ambiguous with a
// typed error), preview-splices, formats, lints, and reparses to prove the result
// is valid, then durably records a wal.Intent. Nothing is written to a source file
// during Prepare — its sole on-disk effect is the WAL record.
//
// Commit is the ATOMIC, lossless point. Per file, UNDER A PER-FILE LOCK, it
// RE-READS the live bytes and RE-RESOLVES every edit (resolve-under-lock, so a
// concurrent commit that benignly shifted a region is picked up and the edit lands
// at the current offset, never the stale one), splices, atomic-writes, reparses,
// and computes a region.EditResult. A region whose region_hash no longer matches
// any live node is a *ConflictError and that file is not written. Same-file commits
// serialize on one lock; cross-file commits take different locks and run in parallel.
//
// The cross-store graph+file coordinator lives in HYLLA per ADR-0001; this package
// owns only the FILE leg.
package session

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"sync"

	"github.com/hylla-io/bage/internal/atomicwrite"
	"github.com/hylla-io/bage/internal/edit"
	"github.com/hylla-io/bage/internal/format"
	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/locator"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/wal"
)

// walLogName is the fixed WAL file name within a session's WALDir, kept in sync
// with internal/wal so Commit and Recover can clear the log after settling.
const walLogName = "wal.log"

// ErrConflict is the sentinel wrapped by every ConflictError, so callers can
// match a region conflict with errors.Is without inspecting the path.
var ErrConflict = errors.New("session: edit conflict")

// ConflictError reports that a region-anchored edit could not be resolved against
// the live file: the target's region_hash no longer matches any live node (a
// concurrent edit changed the same region), or it matches more than one (ambiguous
// twins). Either case is a hard reject — Båge never guesses and never misapplies
// (SPEC §8.3, §8.4, ADR-0003). The offending file is left untouched.
type ConflictError struct {
	// Path is the file whose region could not be resolved.
	Path string
	// Reason is the resolve status that triggered the conflict ("conflict" or
	// "ambiguous"), or a wrapped resolver error message.
	Reason string
}

// Error implements error with a path-and-reason-qualified message.
func (e *ConflictError) Error() string {
	return fmt.Sprintf("session: edit conflict for %q: %s", e.Path, e.Reason)
}

// Unwrap returns ErrConflict so errors.Is(err, ErrConflict) matches a ConflictError.
func (e *ConflictError) Unwrap() error { return ErrConflict }

// Session is the configured FILE-LEG edit engine. Formatter and Linter may be
// nil to skip the corresponding step; Parser, Hasher, and WALDir are required.
// Lang is an OPTIONAL per-session override (see the field). A single Session is
// safe for concurrent Prepare/Commit calls: the
// per-file lock map serializes writers to the same file while letting different
// files proceed in parallel (SPEC §8.3).
type Session struct {
	// Parser reparses live and staged bytes (drift relocation + parse assertion).
	Parser parser.ParserPort
	// Hasher computes the raw and normalized hashes recorded into the WAL intent
	// and the post-edit hashes returned in each EditResult.
	Hasher hashing.Hasher
	// Formatter, when non-nil, rewrites staged bytes before linting/parsing.
	Formatter format.Formatter
	// Linter, when non-nil, validates staged bytes; a lint failure blocks Prepare.
	Linter format.Linter
	// Lang, when set (non-LangUnknown), forces that language for every file;
	// when LangUnknown (the zero value) each file's language is auto-detected
	// from its path via parser.LangForPath. It is an optional per-session
	// override, letting an agent IDE edit a mixed-language tree without a single
	// session-wide language.
	Lang parser.Lang
	// WALDir is the directory holding this session's write-ahead log.
	WALDir string

	// metaMu guards locks. Acquire it only to fetch or create a per-file mutex,
	// never while holding a per-file mutex.
	metaMu sync.Mutex
	// locks maps an absolute file path to its writer mutex. Same-file commits
	// serialize on one mutex; cross-file commits take different mutexes.
	locks map[string]*sync.Mutex
}

// Plan is the result of a successful Prepare. It records the region edits and the
// per-file anchors so Commit can re-validate under lock — Prepare is optimistic,
// Commit is the atomic point (SPEC §8).
type Plan struct {
	// Intent is the WAL intent recorded by Prepare (already persisted): originals
	// for restore-on-failure plus the expected per-file hashes.
	Intent wal.Intent
	// Edits are the region-anchored edits this plan will apply, in input order.
	Edits []region.Edit
	// Anchors maps each file path to the per-file drift gate it was prepared
	// against (SPEC §8.1).
	Anchors map[string]region.FileAnchor
}

// langFor returns the language to use for path: the per-session Lang override
// when set (non-LangUnknown), otherwise the per-file language auto-detected from
// path via parser.LangForPath, which never returns LangUnknown. This lets an
// agent IDE open a mixed-language tree without a single per-session Lang while
// keeping an explicit Session.Lang as a full override for backward compatibility.
func (s *Session) langFor(path string) parser.Lang {
	if s.Lang != parser.LangUnknown {
		return s.Lang
	}
	return parser.LangForPath(path)
}

// fileLock returns the writer mutex for an absolute path, creating it on first
// use under the meta-mutex. The returned mutex is stable for the path's lifetime
// so repeated Commits of the same file always serialize on the same lock.
func (s *Session) fileLock(path string) *sync.Mutex {
	s.metaMu.Lock()
	defer s.metaMu.Unlock()
	if s.locks == nil {
		s.locks = make(map[string]*sync.Mutex)
	}
	mu, ok := s.locks[path]
	if !ok {
		mu = &sync.Mutex{}
		s.locks[path] = mu
	}
	return mu
}

// Prepare optimistically stages every region-anchored edit and records one WAL
// intent. It holds no lock. For each file it reads the live bytes, resolves every
// edit via region.Resolve (a Conflict/Ambiguous resolve yields a *ConflictError
// and nothing is committed), preview-splices the resolved ranges, runs the
// Formatter then the Linter (a lint failure rejects), and reparses to assert the
// result still parses (a parse failure rejects). The originals and expected hashes
// are recorded into a wal.Intent persisted via wal.Append. On success it returns a
// Plan; the only on-disk effect is the WAL record — no source file is written.
func (s *Session) Prepare(ctx context.Context, edits []region.Edit, anchors []region.FileAnchor) (*Plan, error) {
	byFile := groupByFile(edits)
	anchorByPath := indexAnchors(anchors)

	intent := wal.Intent{
		ID:               newIntentID(),
		Edits:            nil,
		Originals:        make(map[string][]byte, len(byFile)),
		ExpectedRawHash:  make(map[string]string, len(byFile)),
		ExpectedNormHash: make(map[string]string, len(byFile)),
	}

	for _, path := range sortedKeys(byFile) {
		fileEdits := byFile[path]
		live, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("session: read live file %q: %w", path, err)
		}

		// Resolve every edit against the optimistic live snapshot. A Conflict or
		// Ambiguous here rejects the whole Prepare before any WAL is written.
		resolved, err := s.resolveEdits(path, live, fileEdits)
		if err != nil {
			return nil, err
		}

		spliced, err := edit.SpliceEdits(live, resolved)
		if err != nil {
			return nil, fmt.Errorf("session: splice %q: %w", path, err)
		}

		if err := s.formatLintParse(ctx, path, spliced); err != nil {
			return nil, err
		}

		intent.Edits = append(intent.Edits, resolved...)
		intent.Originals[path] = live
		intent.ExpectedRawHash[path] = hashing.RawHash(s.Hasher, live)
		intent.ExpectedNormHash[path] = hashing.NormHash(s.Hasher, live)
	}

	if err := wal.Append(s.WALDir, intent); err != nil {
		return nil, fmt.Errorf("session: append WAL: %w", err)
	}

	return &Plan{Intent: intent, Edits: edits, Anchors: anchorByPath}, nil
}

// Commit is the atomic, lossless point. Per file, under that file's lock, it
// RE-READS the live bytes, RE-RESOLVES every edit against the current content
// (resolve-under-lock — this picks up concurrent commits, so a benignly shifted
// region lands at its current offset and a same-region conflict is rejected),
// splices the resolved ranges, atomic-writes, reparses, and computes an
// EditResult. A *ConflictError on any file aborts the Commit: every file already
// written in this Commit is restored from its original, and the WAL is preserved
// for Recover. On full success the WAL is cleared. Different files take different
// locks, so cross-file commits run in parallel; same-file commits serialize.
func (s *Session) Commit(plan *Plan) ([]region.EditResult, error) {
	byFile := groupByFile(plan.Edits)

	var results []region.EditResult
	written := make([]string, 0, len(byFile))

	for _, path := range sortedKeys(byFile) {
		fileEdits := byFile[path]

		res, err := s.commitFile(path, fileEdits)
		if err != nil {
			// Handled failure: restore every file this Commit already wrote so the
			// source is left untouched (SPEC §1.2, §8.4). The WAL is preserved so
			// Recover remains a backstop if a restore itself fails.
			s.restore(written, plan.Intent.Originals)
			return nil, err
		}
		written = append(written, path)
		results = append(results, res...)
	}

	if err := clearWAL(s.WALDir); err != nil {
		return nil, fmt.Errorf("session: commit clear WAL: %w", err)
	}
	return results, nil
}

// commitFile applies one file's edits under that file's lock and returns one
// EditResult per edit. It is the resolve-under-lock unit: it re-reads the live
// bytes inside the lock so it sees every prior concurrent commit.
func (s *Session) commitFile(path string, edits []region.Edit) ([]region.EditResult, error) {
	mu := s.fileLock(path)
	mu.Lock()
	defer mu.Unlock()

	live, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("session: commit read %q: %w", path, err)
	}

	resolved, err := s.resolveEdits(path, live, edits)
	if err != nil {
		return nil, err
	}

	out, err := edit.SpliceEdits(live, resolved)
	if err != nil {
		return nil, fmt.Errorf("session: commit splice %q: %w", path, err)
	}

	if err := atomicwrite.Write(path, out); err != nil {
		return nil, fmt.Errorf("session: commit write %q: %w", path, err)
	}

	results, err := s.editResults(context.Background(), path, out, resolved)
	if err != nil {
		return nil, err
	}
	return results, nil
}

// resolveEdits resolves every edit against live via region.Resolve, returning the
// byte-range FileEdits to splice. A Conflict or Ambiguous status (or a resolver
// error) becomes a *ConflictError so the file is rejected, never misapplied
// (SPEC §8.3, §8.4). Benign shifts (Shifted) are silently re-grounded to the
// resolved offset.
func (s *Session) resolveEdits(path string, live []byte, edits []region.Edit) ([]locator.FileEdit, error) {
	resolved := make([]locator.FileEdit, 0, len(edits))
	for _, e := range edits {
		start, end, status, rerr := region.Resolve(s.Parser, s.langFor(path), live, e.Region)
		if status == region.Conflict || status == region.Ambiguous || rerr != nil {
			return nil, &ConflictError{Path: path, Reason: status.String()}
		}
		resolved = append(resolved, locator.FileEdit{
			Path:      path,
			StartByte: start,
			EndByte:   end,
			NewText:   e.NewText,
		})
	}
	return resolved, nil
}

// formatLintParse runs the Formatter (if set), the Linter (if set; a failure
// rejects), and a reparse over spliced to assert it still parses (a parse failure
// rejects). It returns nil when the staged bytes are valid. It mutates nothing
// on disk — it operates purely on the preview bytes.
func (s *Session) formatLintParse(ctx context.Context, path string, spliced []byte) error {
	return s.formatLintParseLang(ctx, path, s.langFor(path), spliced)
}

// formatLintParseLang is formatLintParse with an EXPLICIT language, so the
// create path can honour a per-op Lang override while reusing the identical
// format → lint → parse-floor sequence. path is used only for error context.
func (s *Session) formatLintParseLang(ctx context.Context, path string, lang parser.Lang, spliced []byte) error {
	if s.Formatter != nil {
		formatted, ferr := s.Formatter.Format(ctx, spliced)
		if ferr != nil {
			return fmt.Errorf("session: format %q: %w", path, ferr)
		}
		spliced = formatted
	}
	if s.Linter != nil {
		if lerr := s.Linter.Lint(ctx, spliced); lerr != nil {
			return fmt.Errorf("session: lint %q: %w", path, lerr)
		}
	}
	_, tree, perr := edit.Reparse(ctx, s.Parser, lang, spliced, nil, nil)
	if perr != nil {
		return fmt.Errorf("session: reparse %q: %w", path, perr)
	}
	tree.Close()
	return nil
}

// editResults builds one region.EditResult per applied edit over the post-write
// file bytes (out): the changed byte range, the recomputed region/file hashes,
// and the new 1-based line range, so Hylla re-ingests only the changed region
// (SPEC §8.2). Ranges are computed against out, which already reflects every
// edit in this file, accounting for the offset shifts splicing introduced.
func (s *Session) editResults(_ context.Context, path string, out []byte, resolved []locator.FileEdit) ([]region.EditResult, error) {
	li := region.NewLineIndex(out)
	rawHash := hashing.RawHash(s.Hasher, out)
	normHash := hashing.NormHash(s.Hasher, out)

	// Splicing is reverse-sorted by offset (edit.SpliceEdits), so the changed
	// range of a given edit shifts only by the net length delta of every edit at
	// a LOWER start offset. Walk in ascending start order accumulating that delta.
	asc := make([]locator.FileEdit, len(resolved))
	copy(asc, resolved)
	sort.SliceStable(asc, func(i, j int) bool { return asc[i].StartByte < asc[j].StartByte })

	results := make([]region.EditResult, 0, len(asc))
	delta := 0
	for _, e := range asc {
		newStart := e.StartByte + delta
		newEnd := newStart + len(e.NewText)
		startLine, _ := li.PositionForByte(newStart)
		endLine, _ := li.PositionForByte(newEnd)
		results = append(results, region.EditResult{
			Path:            path,
			ChangedStart:    newStart,
			ChangedEnd:      newEnd,
			NewRegionHash:   region.HashRegion(out, newStart, newEnd),
			NewFileRawHash:  rawHash,
			NewFileNormHash: normHash,
			NewStartLine:    startLine,
			NewEndLine:      endLine,
		})
		delta += len(e.NewText) - (e.EndByte - e.StartByte)
	}
	return results, nil
}

// restore writes the originals of every path back to disk on a handled Commit
// failure, leaving the source untouched (SPEC §1.2). A restore error is swallowed
// because the WAL is preserved as the recovery backstop.
func (s *Session) restore(written []string, originals map[string][]byte) {
	for _, p := range written {
		if orig, ok := originals[p]; ok {
			_ = atomicwrite.Write(p, orig)
		}
	}
}

// Rollback abandons a prepared Plan. Because Prepare writes nothing live (only the
// WAL record), rollback discards the staged edits and clears the WAL; the source
// files are left untouched (SPEC §1.2 restore-on-handled-failure).
func (s *Session) Rollback(plan *Plan) error {
	plan.Edits = nil
	if err := clearWAL(s.WALDir); err != nil {
		return fmt.Errorf("session: rollback clear WAL: %w", err)
	}
	return nil
}

// Recover is the restart path: it replays the WAL in dir and, for each intent,
// restores every affected file from Intent.Originals via atomicwrite.Write,
// converging the files back to their pre-Prepare state (files-as-truth, SPEC
// §1.2 converge-on-crash). It then clears the WAL. A crash between Prepare and
// Commit leaves a WAL record whose Originals undo the half-done edit; a crash
// after Commit cleared the WAL leaves nothing to replay.
//
// The same Originals loop is the delete undo: a DELETE intent (ADR-0004) records
// every deleted path's FULL prior bytes in Originals BEFORE the unlink, so a
// crash in the window between that durable record and the unlink is recovered by
// writing those bytes back — restoring the deleted file. A delete that already
// landed (file gone, bytes captured) is re-created with its original content; a
// delete that never reached the unlink leaves the file present and the restore
// is a harmless rewrite of identical bytes. (Intent.Deletes marks the lifecycle
// op; the restore mechanism is Originals, shared with the edit-undo path.)
func (s *Session) Recover(_ context.Context, dir string) error {
	intents, err := wal.Replay(dir)
	if err != nil {
		return fmt.Errorf("session: recover replay: %w", err)
	}
	for _, in := range intents {
		for path, original := range in.Originals {
			if werr := atomicwrite.Write(path, original); werr != nil {
				return fmt.Errorf("session: recover restore %q: %w", path, werr)
			}
		}
		// A create intent's undo is UNLINK: a crash between the durable WAL
		// record and the WAL clear leaves a half-created file on disk, so
		// removing each Creates path converges back to non-existence (ADR-0004,
		// files-as-truth). A path already gone (the create never landed) is not
		// an error.
		for _, path := range in.Creates {
			if rerr := os.Remove(path); rerr != nil && !os.IsNotExist(rerr) {
				return fmt.Errorf("session: recover unlink %q: %w", path, rerr)
			}
		}
	}
	if err := clearWAL(dir); err != nil {
		return fmt.Errorf("session: recover clear WAL: %w", err)
	}
	return nil
}

// groupByFile buckets edits by their region's path, preserving input order within
// each bucket so multi-edit files splice deterministically.
func groupByFile(edits []region.Edit) map[string][]region.Edit {
	byFile := make(map[string][]region.Edit)
	for _, e := range edits {
		byFile[e.Region.Path] = append(byFile[e.Region.Path], e)
	}
	return byFile
}

// indexAnchors maps anchors by their path for Plan storage.
func indexAnchors(anchors []region.FileAnchor) map[string]region.FileAnchor {
	out := make(map[string]region.FileAnchor, len(anchors))
	for _, a := range anchors {
		out[a.Path] = a
	}
	return out
}

// sortedKeys returns the map keys in ascending order, so per-file iteration is
// deterministic across Prepare and Commit (stable WAL ordering, reproducible tests).
func sortedKeys(m map[string][]region.Edit) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

// clearWAL removes the WAL log file in dir so a settled session leaves no pending
// intents. A missing log (already empty) is not an error.
func clearWAL(dir string) error {
	path := filepath.Join(dir, walLogName)
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("session: remove WAL %q: %w", path, err)
	}
	return nil
}

// newIntentID returns a process-unique intent identifier from the PID and a
// monotonic counter, so two intents prepared in one process never collide without
// a third-party UUID dependency. The counter is incremented atomically so
// concurrent Prepare calls get distinct IDs.
func newIntentID() string {
	n := nextIntentSeq()
	return fmt.Sprintf("intent-%d-%d", os.Getpid(), n)
}

// intentSeq is the per-process counter feeding newIntentID, guarded by intentMu
// so concurrent Prepare callers never observe the same value.
var (
	intentSeq uint64
	intentMu  sync.Mutex
)

// nextIntentSeq atomically increments and returns the intent counter.
func nextIntentSeq() uint64 {
	intentMu.Lock()
	defer intentMu.Unlock()
	intentSeq++
	return intentSeq
}
