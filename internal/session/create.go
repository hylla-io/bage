// File create.go adds the FILE-LIFECYCLE create leg to the session engine
// (ADR-0004, SPEC §10): creating a new file is first-class on the same engine
// philosophy as an edit — WAL-logged, atomic, reject-never-corrupt. Its anchor
// is NON-EXISTENCE: the target must not already exist (no clobber, no overwrite
// escape hatch in this slice), enforced race-safely under the per-file lock with
// O_EXCL create semantics. The staged bytes must clear the same format/lint/parse
// floor edits clear, and the create is WAL-logged so a crash unlinks the
// half-created file (create's undo is unlink). It is ADDITIVE: the existing
// []region.Edit Prepare/Commit/Rollback path is unchanged.
package session

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/wal"
)

// ErrExists is the sentinel returned when CreateFile is asked to create a path
// that already exists. The non-existence anchor is absolute in this slice:
// Båge never clobbers existing content, so callers can match the reject with
// errors.Is(err, ErrExists).
var ErrExists = errors.New("session: target already exists")

// OpKind tags the kind of a file-lifecycle Op. It is shaped to extend to delete
// and move in later slices; this slice implements only OpCreate.
type OpKind int

const (
	// OpCreate is a create-from-non-existence op: bring a new file into being
	// with the given content, rejecting if the path already exists.
	OpCreate OpKind = iota
	// OpDelete is a delete op: unlink an existing file, gated by the expected
	// raw_hash drift anchor, capturing the prior bytes for restore (ADR-0004).
	OpDelete
	// OpMove is a relocate op: anchored-DELETE(Path) + anchored-CREATE(To) as one
	// atomic-on-recovery unit, preserving the source bytes unchanged at the
	// destination (ADR-0004). The source is gated by the raw_hash drift anchor
	// (like delete); the destination is gated by non-existence (like create).
	OpMove
)

// String returns a stable lowercase label for the op kind.
func (k OpKind) String() string {
	switch k {
	case OpCreate:
		return "create"
	case OpDelete:
		return "delete"
	case OpMove:
		return "move"
	default:
		return "unknown"
	}
}

// Op is a tagged file-lifecycle operation (ADR-0004). It currently models
// Create; the tagged shape lets later slices add Delete/Move without changing
// the edit (region.Edit) path. For OpCreate, Path is the file to create and
// Content is its full bytes; Lang optionally forces the parse-floor language,
// otherwise the session auto-detects it from Path (mirroring langFor).
type Op struct {
	// Kind tags which lifecycle operation this is.
	Kind OpKind
	// Path is the target file path. For OpMove it is the SOURCE path being moved.
	Path string
	// To is the destination path for OpMove: the source bytes are relocated here
	// unchanged. Its anchor is non-existence (To must NOT already exist, no
	// clobber). Empty for OpCreate/OpDelete.
	To string
	// Content is the full bytes of the new file (OpCreate).
	Content string
	// Lang optionally forces the language for the parse floor; LangUnknown (the
	// zero value) means auto-detect from Path.
	Lang parser.Lang
	// ExpectedRawHash is the drift gate for OpDelete and the SOURCE of OpMove: the
	// live file must still hash (hashing.RawHash) to this value or the op
	// HARD-REJECTS, never discarding (delete) or relocating (move) bytes the caller
	// did not see. Empty for OpCreate.
	ExpectedRawHash string
}

// CreateFile creates a new file from op, returning a whole-file EditResult a
// host can ingest. It rejects unless op.Kind is OpCreate.
//
// Under the target's per-file lock it: (1) runs the format/lint/parse floor over
// the staged bytes (a failure rejects, NOTHING is written); (2) mkdir -p's the
// parent directory; (3) creates the file with O_CREATE|O_EXCL so a pre-existing
// path (or a concurrent create that won the race) HARD-REJECTS with ErrExists
// and never clobbers — this EXCLUSIVELY claims the path and PROVES non-existence
// BEFORE anything durable names it; (4) only now WAL-logs the create (Creates
// names the now-guaranteed-mine path so a crash or rollback unlinks it);
// (5) writes + fsyncs the bytes; (6) clears the WAL. Doing the O_EXCL open
// before the WAL append is the clobber-via-recovery fix: a Creates record can
// only ever name a file THIS op actually brought into being, so Recover's unlink
// can never delete pre-existing content. On any post-open failure the partial
// file is removed AND the WAL is cleared. The O_EXCL create under the lock is the
// non-existence anchor: the existence check and the create are one atomic kernel
// step, so two concurrent creates cannot both win and nothing is overwritten.
func (s *Session) CreateFile(ctx context.Context, op Op) (region.EditResult, error) {
	if op.Kind != OpCreate {
		return region.EditResult{}, fmt.Errorf("session: CreateFile requires a create op, got %s", op.Kind)
	}

	mu := s.fileLock(op.Path)
	mu.Lock()
	defer mu.Unlock()

	staged := []byte(op.Content)

	// Parse floor (and optional format/lint) over the staged bytes BEFORE any
	// on-disk effect, so a non-parsing create writes nothing.
	if err := s.formatLintParseLang(ctx, op.Path, s.createLang(op), staged); err != nil {
		return region.EditResult{}, err
	}

	if err := os.MkdirAll(filepath.Dir(op.Path), 0o755); err != nil {
		return region.EditResult{}, fmt.Errorf("session: create mkdir for %q: %w", op.Path, err)
	}

	// Claim the path FIRST: O_CREATE|O_EXCL atomically proves non-existence and
	// brings the file into being. A pre-existing path rejects HERE with ErrExists
	// before any durable record names it, so a rejected create can never log a
	// Creates intent that later destroys the user's file (clobber-via-recovery
	// fix). This is the non-existence anchor.
	f, err := s.openExclusive(op.Path)
	if err != nil {
		return region.EditResult{}, err
	}

	// The path is now exclusively ours. WAL-log the create so a crash after the
	// file lands but before the WAL clear can unlink it on Recover (create's undo
	// is unlink). Because the file already exists by our hand, the Creates record
	// can only ever name a file this op created.
	intent := wal.Intent{ID: newIntentID(), Creates: []string{op.Path}}
	if err := wal.Append(s.WALDir, intent); err != nil {
		f.Close()
		os.Remove(op.Path)
		return region.EditResult{}, fmt.Errorf("session: create append WAL: %w", err)
	}

	if err := s.writeAndSync(f, op.Path, staged); err != nil {
		// The create did not fully land; remove the file we exclusively created
		// and drop the just-logged record so a failed create leaves nothing.
		os.Remove(op.Path)
		_ = clearWAL(s.WALDir)
		return region.EditResult{}, err
	}

	if err := clearWAL(s.WALDir); err != nil {
		return region.EditResult{}, fmt.Errorf("session: create clear WAL: %w", err)
	}

	return s.createResult(op.Path, staged), nil
}

// openExclusive opens path with O_CREATE|O_EXCL|O_WRONLY so a pre-existing path
// rejects with ErrExists and is never clobbered. O_EXCL makes the
// existence-check-and-create one atomic kernel step under the per-file lock, so
// two concurrent creates cannot both win. The returned file handle is owned by
// the caller, which must Close it (and remove the path on any later failure).
func (s *Session) openExclusive(path string) (*os.File, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o644)
	if err != nil {
		if errors.Is(err, os.ErrExist) {
			return nil, fmt.Errorf("session: create %q: %w", path, ErrExists)
		}
		return nil, fmt.Errorf("session: create open %q: %w", path, err)
	}
	return f, nil
}

// writeAndSync writes data into the already-open, exclusively-created f and
// fsyncs before close so the content is durable. path is used only for error
// context. On any failure the file is left for the caller to remove; the handle
// is always closed.
func (s *Session) writeAndSync(f *os.File, path string, data []byte) error {
	if _, err := f.Write(data); err != nil {
		f.Close()
		return fmt.Errorf("session: create write %q: %w", path, err)
	}
	if err := f.Sync(); err != nil {
		f.Close()
		return fmt.Errorf("session: create fsync %q: %w", path, err)
	}
	if err := f.Close(); err != nil {
		return fmt.Errorf("session: create close %q: %w", path, err)
	}
	return nil
}

// createResult builds the whole-file EditResult for a created file: the changed
// range spans the entire content, the region/file hashes are recomputed over the
// new bytes, and the 1-based line range covers the whole file, so a host can
// ingest the new file (SPEC §8.2 shape reused for create).
func (s *Session) createResult(path string, data []byte) region.EditResult {
	li := region.NewLineIndex(data)
	startLine, _ := li.PositionForByte(0)
	endLine, _ := li.PositionForByte(len(data))
	return region.EditResult{
		Path:            path,
		ChangedStart:    0,
		ChangedEnd:      len(data),
		NewRegionHash:   region.HashRegion(data, 0, len(data)),
		NewFileRawHash:  hashing.RawHash(s.Hasher, data),
		NewFileNormHash: hashing.NormHash(s.Hasher, data),
		NewStartLine:    startLine,
		NewEndLine:      endLine,
	}
}

// createLang resolves the parse-floor language for op: an explicit per-op Lang
// wins; otherwise it defers to langFor(op.Path), which honours a session-wide
// Lang override and finally auto-detects from the path. This mirrors runApply /
// langFor so create resolves grammars identically to edits.
func (s *Session) createLang(op Op) parser.Lang {
	if op.Lang != parser.LangUnknown {
		return op.Lang
	}
	return s.langFor(op.Path)
}
