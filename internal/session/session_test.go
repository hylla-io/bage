package session

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/hylla-io/bage/internal/format"
	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/wal"
)

// writeFile creates a file under dir and returns its path.
func writeFile(t *testing.T, dir, name, contents string) string {
	t.Helper()
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, []byte(contents), 0o644); err != nil {
		t.Fatalf("write %q: %v", p, err)
	}
	return p
}

// readFile reads a file or fails the test.
func readFile(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %q: %v", path, err)
	}
	return string(b)
}

// newSession builds a Session over the real tree-sitter adapter (the resolver
// reparses, so a real CST is required for the relocation path) with Go as the
// language and the given format ports.
func newSession(t *testing.T, f format.Formatter, l format.Linter) *Session {
	t.Helper()
	return &Session{
		Parser:    treesitter.New(),
		Hasher:    hashing.XXHasher{},
		Formatter: f,
		Linter:    l,
		Lang:      parser.LangGo,
		WALDir:    filepath.Join(t.TempDir(), "wal"),
	}
}

// anchorFor builds the per-file FileAnchor over src.
func anchorFor(path, src string) region.FileAnchor {
	h := hashing.XXHasher{}
	return region.FileAnchor{
		Path:     path,
		RawHash:  hashing.RawHash(h, []byte(src)),
		NormHash: hashing.NormHash(h, []byte(src)),
	}
}

// regionEdit anchors a [start:end) byte region of src and replaces it with newText.
func regionEdit(path, src string, start, end int, newText string) region.Edit {
	return region.Edit{
		Region: region.Region{
			Path:       path,
			StartByte:  start,
			EndByte:    end,
			RegionHash: region.HashRegion([]byte(src), start, end),
		},
		NewText: newText,
	}
}

// goSrc is a small valid Go file with three sibling funcs the tests edit.
const goSrc = "package main\n\nfunc a() {}\n\nfunc b() {}\n\nfunc c() {}\n"

func TestPrepareCommitHappyPath(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	// Replace the body of a(): "func a() {}" spans [14:25).
	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	e := regionEdit(path, goSrc, start, end, "func a() { return }")

	plan, err := s.Prepare(context.Background(), []region.Edit{e}, []region.FileAnchor{anchorFor(path, goSrc)})
	if err != nil {
		t.Fatalf("Prepare: %v", err)
	}
	// Source untouched until Commit.
	if got := readFile(t, path); got != goSrc {
		t.Fatalf("source changed before Commit: %q", got)
	}

	results, err := s.Commit(plan)
	if err != nil {
		t.Fatalf("Commit: %v", err)
	}
	want := strings.Replace(goSrc, "func a() {}", "func a() { return }", 1)
	if got := readFile(t, path); got != want {
		t.Fatalf("committed = %q, want %q", got, want)
	}
	if len(results) != 1 {
		t.Fatalf("results = %d, want 1", len(results))
	}
	r := results[0]
	if r.Path != path {
		t.Fatalf("result path = %q, want %q", r.Path, path)
	}
	if got := want[r.ChangedStart:r.ChangedEnd]; got != "func a() { return }" {
		t.Fatalf("changed range = %q, want %q", got, "func a() { return }")
	}
	if r.NewRegionHash != region.HashRegion([]byte(want), r.ChangedStart, r.ChangedEnd) {
		t.Fatalf("NewRegionHash mismatch")
	}
	if r.NewStartLine != 3 || r.NewEndLine != 3 {
		t.Fatalf("line range = [%d:%d], want [3:3]", r.NewStartLine, r.NewEndLine)
	}
}

func TestPrepareConflictBlocks(t *testing.T) {
	dir := t.TempDir()
	// The live file's a() body differs from what the edit was anchored against,
	// so the region_hash matches nothing — a hard conflict at Prepare time.
	live := strings.Replace(goSrc, "func a() {}", "func a() { x := 1; _ = x }", 1)
	path := writeFile(t, dir, "main.go", live)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	e := regionEdit(path, goSrc, start, end, "func a() { return }")

	plan, err := s.Prepare(context.Background(), []region.Edit{e}, []region.FileAnchor{anchorFor(path, goSrc)})
	if err == nil {
		t.Fatalf("Prepare: want conflict, got plan %+v", plan)
	}
	var ce *ConflictError
	if !errors.As(err, &ce) {
		t.Fatalf("want *ConflictError, got %T: %v", err, err)
	}
	if !errors.Is(err, ErrConflict) {
		t.Fatalf("want errors.Is ErrConflict, got %v", err)
	}
	if ce.Path != path {
		t.Fatalf("ConflictError.Path = %q, want %q", ce.Path, path)
	}
	if got := readFile(t, path); got != live {
		t.Fatalf("file changed on conflict: %q", got)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL has %d intents on conflict, want 0", len(intents))
	}
}

func TestPrepareLinterFailureBlocks(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	lintErr := errors.New("lint boom")
	s := newSession(t, nil, format.FakeLinter{Err: lintErr})

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	e := regionEdit(path, goSrc, start, end, "func a() { return }")

	if _, err := s.Prepare(context.Background(), []region.Edit{e}, nil); !errors.Is(err, lintErr) {
		t.Fatalf("Prepare: want lint error, got %v", err)
	}
	if got := readFile(t, path); got != goSrc {
		t.Fatalf("file changed on lint failure: %q", got)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL has %d intents on lint failure, want 0", len(intents))
	}
}

// errParser wraps the real tree-sitter adapter, resolving normally (so Resolve's
// CST path still works) but forcing the post-splice parse-assertion in
// formatLintParse to fail on demand. tree-sitter's Go grammar is error-tolerant
// and never returns a Parse error for malformed syntax, so the parse-block path
// (SPEC §8.4) is exercised by injecting the failure rather than feeding bad bytes.
type errParser struct {
	parser.ParserPort
	failOn []byte
	err    error
}

func (p *errParser) Parse(ctx context.Context, lang parser.Lang, src []byte) (*parser.Tree, error) {
	if p.failOn != nil && strings.Contains(string(src), string(p.failOn)) {
		return nil, p.err
	}
	return p.ParserPort.Parse(ctx, lang, src)
}

func TestPrepareParseFailureBlocks(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)
	parseErr := errors.New("parse boom")
	// Fail only when the spliced (post-edit) bytes are parsed for the assertion.
	s.Parser = &errParser{ParserPort: treesitter.New(), failOn: []byte("SENTINEL_BODY"), err: parseErr}

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	e := regionEdit(path, goSrc, start, end, "func a() { SENTINEL_BODY := 1; _ = SENTINEL_BODY }")

	if _, err := s.Prepare(context.Background(), []region.Edit{e}, nil); !errors.Is(err, parseErr) {
		t.Fatalf("Prepare: want parse error, got %v", err)
	}
	if got := readFile(t, path); got != goSrc {
		t.Fatalf("file changed on parse failure: %q", got)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL has %d intents on parse failure, want 0", len(intents))
	}
}

func TestPrepareWritesAndCommitClearsWAL(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func b() {}")
	end := start + len("func b() {}")
	e := regionEdit(path, goSrc, start, end, "func b() { return }")

	plan, err := s.Prepare(context.Background(), []region.Edit{e}, nil)
	if err != nil {
		t.Fatalf("Prepare: %v", err)
	}
	intents, err := wal.Replay(s.WALDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != 1 {
		t.Fatalf("WAL intents = %d, want 1", len(intents))
	}
	if intents[0].ID != plan.Intent.ID {
		t.Fatalf("WAL intent ID = %q, want %q", intents[0].ID, plan.Intent.ID)
	}
	if got := string(intents[0].Originals[path]); got != goSrc {
		t.Fatalf("WAL original = %q, want %q", got, goSrc)
	}
	if _, err := s.Commit(plan); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL not cleared after Commit: %d intents", len(intents))
	}
}

func TestRollbackLeavesSourceUntouchedAndClearsWAL(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	plan, err := s.Prepare(context.Background(), []region.Edit{regionEdit(path, goSrc, start, end, "func a() { return }")}, nil)
	if err != nil {
		t.Fatalf("Prepare: %v", err)
	}
	if err := s.Rollback(plan); err != nil {
		t.Fatalf("Rollback: %v", err)
	}
	if got := readFile(t, path); got != goSrc {
		t.Fatalf("source changed on Rollback: %q", got)
	}
	if plan.Edits != nil {
		t.Fatalf("Rollback did not discard staged edits")
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL not cleared after Rollback: %d intents", len(intents))
	}
}

func TestRecoverRestoresOriginals(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	if _, err := s.Prepare(context.Background(), []region.Edit{regionEdit(path, goSrc, start, end, "func a() { return }")}, nil); err != nil {
		t.Fatalf("Prepare: %v", err)
	}
	// Simulate a crash: live file corrupted, Commit never ran.
	if err := os.WriteFile(path, []byte("GARBAGE\n"), 0o644); err != nil {
		t.Fatalf("simulate crash write: %v", err)
	}
	if err := s.Recover(context.Background(), s.WALDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}
	if got := readFile(t, path); got != goSrc {
		t.Fatalf("Recover restored = %q, want %q", got, goSrc)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL not cleared after Recover: %d intents", len(intents))
	}
}

func TestMultiFilePrepareCommit(t *testing.T) {
	dir := t.TempDir()
	pathA := writeFile(t, dir, "a.go", goSrc)
	pathB := writeFile(t, dir, "b.go", goSrc)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	edits := []region.Edit{
		regionEdit(pathA, goSrc, start, end, "func a() { return }"),
		regionEdit(pathB, goSrc, start, end, "func a() { panic(1) }"),
	}
	plan, err := s.Prepare(context.Background(), edits, nil)
	if err != nil {
		t.Fatalf("Prepare: %v", err)
	}
	if readFile(t, pathA) != goSrc || readFile(t, pathB) != goSrc {
		t.Fatalf("sources changed before Commit")
	}
	results, err := s.Commit(plan)
	if err != nil {
		t.Fatalf("Commit: %v", err)
	}
	if len(results) != 2 {
		t.Fatalf("results = %d, want 2", len(results))
	}
	if got := readFile(t, pathA); !strings.Contains(got, "func a() { return }") {
		t.Fatalf("pathA not committed: %q", got)
	}
	if got := readFile(t, pathB); !strings.Contains(got, "func a() { panic(1) }") {
		t.Fatalf("pathB not committed: %q", got)
	}
}

// TestMultiFilePartialFailureRestores pins the multi-file atomic-restore invariant
// (SPEC §1.2, §8.4): two files are prepared in ONE Plan; between Prepare and Commit
// file B's target region is mutated on disk so B can no longer resolve under lock.
// Commit must return a *ConflictError, file A must be RESTORED to its exact original
// (never left half-committed), file B must hold its out-of-band mutation, and the WAL
// must be PRESERVED (Replay non-empty) so Recover stays a backstop. The A==original
// assertion holds regardless of which file Commit happens to process first: if A
// commits before B conflicts it is restored; if B conflicts first A is never written.
func TestMultiFilePartialFailureRestores(t *testing.T) {
	dir := t.TempDir()
	// Names chosen so A sorts before B: Commit writes A, then hits B's conflict and
	// must restore A. (The assertion is order-independent regardless.)
	pathA := writeFile(t, dir, "a.go", goSrc)
	pathB := writeFile(t, dir, "b.go", goSrc)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	edits := []region.Edit{
		regionEdit(pathA, goSrc, start, end, "func a() { return }"),
		regionEdit(pathB, goSrc, start, end, "func a() { panic(1) }"),
	}
	plan, err := s.Prepare(context.Background(), edits, nil)
	if err != nil {
		t.Fatalf("Prepare: %v", err)
	}

	// Mutate B's target region OUT OF BAND after Prepare: a()'s own body now differs,
	// so its region_hash matches no live node ⇒ a hard conflict at commit-under-lock.
	mutatedB := strings.Replace(goSrc, "func a() {}", "func a() { x := 9; _ = x }", 1)
	if err := os.WriteFile(pathB, []byte(mutatedB), 0o644); err != nil {
		t.Fatalf("mutate B: %v", err)
	}

	_, err = s.Commit(plan)
	if err == nil {
		t.Fatalf("Commit: want conflict, got nil")
	}
	var ce *ConflictError
	if !errors.As(err, &ce) {
		t.Fatalf("want *ConflictError, got %T: %v", err, err)
	}
	if !errors.Is(err, ErrConflict) {
		t.Fatalf("want errors.Is ErrConflict, got %v", err)
	}

	// File A: restored to (or never moved from) its exact original — never a partial
	// commit. Order-independent: this must hold whichever file Commit touched first.
	if got := readFile(t, pathA); got != goSrc {
		t.Fatalf("file A not restored to original after partial failure:\n%q", got)
	}
	// File B: holds its out-of-band mutation, untouched by the aborted commit.
	if got := readFile(t, pathB); got != mutatedB {
		t.Fatalf("file B = %q, want its mutated content %q", got, mutatedB)
	}
	// WAL preserved: the conflicting Commit must NOT clear the log, so Recover can
	// still converge the files on a later restart.
	intents, rerr := wal.Replay(s.WALDir)
	if rerr != nil {
		t.Fatalf("Replay: %v", rerr)
	}
	if len(intents) == 0 {
		t.Fatalf("WAL cleared on conflicting Commit; want preserved for Recover")
	}
}

// TestConcurrentPrepareSharedWALIntegrity pins WAL append-integrity under
// concurrency: N goroutines each Prepare a DISJOINT edit (one per file) into the
// SAME WALDir at once. The shared wal.log must end up with exactly N cleanly-decoded
// records — no torn or interleaved line — each with a distinct, non-empty intent ID.
// This proves Append's one-fsynced-line-at-a-time write is atomic against concurrent
// appenders. Run under racePkg.
func TestConcurrentPrepareSharedWALIntegrity(t *testing.T) {
	dir := t.TempDir()
	const n = 16
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")

	var wg sync.WaitGroup
	errs := make([]error, n)
	ids := make([]string, n)
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			// Each goroutine edits its OWN file, so the Prepares are fully disjoint
			// and all must succeed; only the shared WAL is contended.
			p := writeFile(t, dir, "f"+string(rune('a'+i))+".go", goSrc)
			e := regionEdit(p, goSrc, start, end, "func a() { return }")
			plan, err := s.Prepare(context.Background(), []region.Edit{e}, nil)
			if err != nil {
				errs[i] = err
				return
			}
			ids[i] = plan.Intent.ID
		}(i)
	}
	wg.Wait()

	for i, err := range errs {
		if err != nil {
			t.Fatalf("Prepare %d: %v", i, err)
		}
	}

	// Replay decodes exactly N records: a torn/interleaved write would either fail to
	// decode (Replay stops early ⇒ < N) or corrupt a line. Every ID is non-empty and
	// distinct, proving no two appends clobbered each other.
	intents, err := wal.Replay(s.WALDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != n {
		t.Fatalf("WAL has %d records, want exactly %d (torn/interleaved append)", len(intents), n)
	}
	seen := make(map[string]struct{}, n)
	for _, in := range intents {
		if in.ID == "" {
			t.Fatalf("decoded a record with empty ID — torn write")
		}
		if _, dup := seen[in.ID]; dup {
			t.Fatalf("duplicate intent ID %q in WAL", in.ID)
		}
		seen[in.ID] = struct{}{}
		// A well-formed record carries its file's original bytes intact.
		if len(in.Originals) != 1 {
			t.Fatalf("record %q has %d originals, want 1 (interleaved record)", in.ID, len(in.Originals))
		}
		for _, orig := range in.Originals {
			if string(orig) != goSrc {
				t.Fatalf("record %q original = %q, want goSrc (torn payload)", in.ID, string(orig))
			}
		}
	}
}

// TestHighContentionSameRegion pins the high-contention same-region invariant
// (SPEC §8.3, ADR-0003): N goroutines each Prepare+Commit an edit to the SAME region
// of one file. Exactly one wins per run; the file always holds exactly one coherent
// edit (never a blend or partial); every loser gets errors.Is(err, ErrConflict). The
// scenario runs several iterations to shake out races. MUST pass racePkg.
func TestHighContentionSameRegion(t *testing.T) {
	const n = 10
	for iter := 0; iter < 8; iter++ {
		dir := t.TempDir()
		path := writeFile(t, dir, "main.go", goSrc)
		s := newSession(t, nil, nil)

		start := strings.Index(goSrc, "func a() {}")
		end := start + len("func a() {}")

		// N distinct replacements for the SAME region, all anchored against the
		// untouched original so every Prepare succeeds optimistically; the race is
		// at Commit, where resolve-under-lock lets exactly one win.
		tos := make([]string, n)
		for i := range tos {
			tos[i] = "func a() { _ = " + string(rune('0'+i)) + " }"
		}

		plans := make([]*Plan, n)
		for i := 0; i < n; i++ {
			e := regionEdit(path, goSrc, start, end, tos[i])
			p, err := s.Prepare(context.Background(), []region.Edit{e}, nil)
			if err != nil {
				t.Fatalf("iter %d Prepare %d: %v", iter, i, err)
			}
			plans[i] = p
		}

		var wg sync.WaitGroup
		results := make([]error, n)
		wg.Add(n)
		for i := 0; i < n; i++ {
			go func(i int) { defer wg.Done(); _, results[i] = s.Commit(plans[i]) }(i)
		}
		wg.Wait()

		okCount, conflictCount := 0, 0
		for i, err := range results {
			switch {
			case err == nil:
				okCount++
			case errors.Is(err, ErrConflict):
				conflictCount++
			default:
				t.Fatalf("iter %d goroutine %d: unexpected error: %v", iter, i, err)
			}
		}
		if okCount != 1 {
			t.Fatalf("iter %d: %d commits succeeded, want exactly 1", iter, okCount)
		}
		if conflictCount != n-1 {
			t.Fatalf("iter %d: %d conflicts, want %d (every loser must conflict)", iter, conflictCount, n-1)
		}

		// The file holds exactly ONE of the candidate edits — never a blend/partial.
		final := readFile(t, path)
		matches := 0
		for _, to := range tos {
			if strings.Contains(final, to) {
				matches++
			}
		}
		if matches != 1 {
			t.Fatalf("iter %d: file holds %d candidate edits, want exactly 1 (blend/partial):\n%s", iter, matches, final)
		}
		// b() and c() are untouched: the winning edit landed cleanly on a().
		if !strings.Contains(final, "func b() {}") || !strings.Contains(final, "func c() {}") {
			t.Fatalf("iter %d: sibling funcs damaged:\n%s", iter, final)
		}
	}
}

// TestConcurrentSameFileDisjointAllApply spawns N goroutines, each Prepare+Commit
// a DISJOINT region edit to one shared file. Every edit must land, none lost: the
// resolve-under-lock path re-grounds each benignly-shifted region against the
// current bytes so concurrent commits compose losslessly (SPEC §8.3, ADR-0003).
func TestConcurrentSameFileDisjointAllApply(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	// Three disjoint edits, one per function body. Each is anchored against the
	// ORIGINAL goSrc; concurrent commits shift the later regions, which the
	// resolver relocates by region_hash under the lock.
	type spec struct{ from, to string }
	specs := []spec{
		{"func a() {}", "func a() { return }"},
		{"func b() {}", "func b() { panic(1) }"},
		{"func c() {}", "func c() { _ = 0 }"},
	}

	var wg sync.WaitGroup
	errs := make([]error, len(specs))
	for i, sp := range specs {
		wg.Add(1)
		go func(i int, sp spec) {
			defer wg.Done()
			start := strings.Index(goSrc, sp.from)
			end := start + len(sp.from)
			e := regionEdit(path, goSrc, start, end, sp.to)
			plan, err := s.Prepare(context.Background(), []region.Edit{e}, nil)
			if err != nil {
				errs[i] = err
				return
			}
			if _, err := s.Commit(plan); err != nil {
				errs[i] = err
			}
		}(i, sp)
	}
	wg.Wait()

	for i, err := range errs {
		if err != nil {
			t.Fatalf("goroutine %d: %v", i, err)
		}
	}
	final := readFile(t, path)
	for _, sp := range specs {
		if !strings.Contains(final, sp.to) {
			t.Fatalf("edit %q lost; final file:\n%s", sp.to, final)
		}
	}
}

// TestConcurrentSameRegionConflictRejects has two goroutines target the SAME
// region. Whichever commits first wins; the loser must get a *ConflictError when
// it re-resolves under the lock (its anchor no longer matches), and the file must
// not be corrupted — it holds exactly one of the two edits.
func TestConcurrentSameRegionConflictRejects(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")
	e1 := regionEdit(path, goSrc, start, end, "func a() { return }")
	e2 := regionEdit(path, goSrc, start, end, "func a() { panic(1) }")

	// Both Prepare against the same untouched file (optimistic), then race Commit.
	p1, err := s.Prepare(context.Background(), []region.Edit{e1}, nil)
	if err != nil {
		t.Fatalf("Prepare 1: %v", err)
	}
	p2, err := s.Prepare(context.Background(), []region.Edit{e2}, nil)
	if err != nil {
		t.Fatalf("Prepare 2: %v", err)
	}

	var wg sync.WaitGroup
	results := make([]error, 2)
	wg.Add(2)
	go func() { defer wg.Done(); _, results[0] = s.Commit(p1) }()
	go func() { defer wg.Done(); _, results[1] = s.Commit(p2) }()
	wg.Wait()

	okCount, conflictCount := 0, 0
	for _, err := range results {
		switch {
		case err == nil:
			okCount++
		case errors.Is(err, ErrConflict):
			conflictCount++
		default:
			t.Fatalf("unexpected commit error: %v", err)
		}
	}
	if okCount != 1 || conflictCount != 1 {
		t.Fatalf("want exactly one success and one conflict, got ok=%d conflict=%d", okCount, conflictCount)
	}

	// File holds exactly one of the two edits, never a corrupted blend.
	final := readFile(t, path)
	hasReturn := strings.Contains(final, "func a() { return }")
	hasPanic := strings.Contains(final, "func a() { panic(1) }")
	if hasReturn == hasPanic {
		t.Fatalf("file corrupted: return=%v panic=%v\n%s", hasReturn, hasPanic, final)
	}
}

// TestCrossFileParallelCommit edits DIFFERENT files concurrently; all must commit
// with no false serialization or deadlock (different per-file locks, SPEC §8.3).
func TestCrossFileParallelCommit(t *testing.T) {
	dir := t.TempDir()
	const n = 6
	paths := make([]string, n)
	for i := range paths {
		paths[i] = writeFile(t, dir, "f"+string(rune('a'+i))+".go", goSrc)
	}
	s := newSession(t, nil, nil)

	start := strings.Index(goSrc, "func a() {}")
	end := start + len("func a() {}")

	var wg sync.WaitGroup
	errs := make([]error, n)
	for i, p := range paths {
		wg.Add(1)
		go func(i int, p string) {
			defer wg.Done()
			e := regionEdit(p, goSrc, start, end, "func a() { return }")
			plan, err := s.Prepare(context.Background(), []region.Edit{e}, nil)
			if err != nil {
				errs[i] = err
				return
			}
			if _, err := s.Commit(plan); err != nil {
				errs[i] = err
			}
		}(i, p)
	}
	wg.Wait()

	for i, err := range errs {
		if err != nil {
			t.Fatalf("file %d: %v", i, err)
		}
		if got := readFile(t, paths[i]); !strings.Contains(got, "func a() { return }") {
			t.Fatalf("file %d not committed: %q", i, got)
		}
	}
}

// TestNoLostUpdateStaleSnapshotReResolves prepares B against an OLD snapshot, then
// A commits first (shifting offsets), then B commits. B must re-resolve under the
// lock and land at the SHIFTED location — not the stale offset — so A's edit is
// preserved and B applies to the correct region (SPEC §8.3 no lost update).
func TestNoLostUpdateStaleSnapshotReResolves(t *testing.T) {
	dir := t.TempDir()
	path := writeFile(t, dir, "main.go", goSrc)
	s := newSession(t, nil, nil)

	// B targets c() and is anchored against the original file.
	cStart := strings.Index(goSrc, "func c() {}")
	cEnd := cStart + len("func c() {}")
	editB := regionEdit(path, goSrc, cStart, cEnd, "func c() { _ = 0 }")
	planB, err := s.Prepare(context.Background(), []region.Edit{editB}, nil)
	if err != nil {
		t.Fatalf("Prepare B: %v", err)
	}

	// A targets a() with a longer replacement, shifting every later offset, and
	// commits FIRST. After this, B's stale byte offset for c() is wrong.
	aStart := strings.Index(goSrc, "func a() {}")
	aEnd := aStart + len("func a() {}")
	editA := regionEdit(path, goSrc, aStart, aEnd, "func a() { x := 1; _ = x; return }")
	planA, err := s.Prepare(context.Background(), []region.Edit{editA}, nil)
	if err != nil {
		t.Fatalf("Prepare A: %v", err)
	}
	if _, err := s.Commit(planA); err != nil {
		t.Fatalf("Commit A: %v", err)
	}

	// Now B commits: its stale offset no longer hash-matches in place, so the
	// resolver reparses the (A-edited) live file and relocates c() by region_hash.
	if _, err := s.Commit(planB); err != nil {
		t.Fatalf("Commit B: %v", err)
	}

	final := readFile(t, path)
	if !strings.Contains(final, "func a() { x := 1; _ = x; return }") {
		t.Fatalf("A's edit lost:\n%s", final)
	}
	if !strings.Contains(final, "func c() { _ = 0 }") {
		t.Fatalf("B did not re-resolve to the shifted location:\n%s", final)
	}
	// B must NOT have corrupted b() (which sat between a and c).
	if !strings.Contains(final, "func b() {}") {
		t.Fatalf("B misapplied at the stale offset and damaged b():\n%s", final)
	}
}

// TestPrepareDriftWhitespaceReResolves pins that a whitespace-only divergence
// between the anchored region bytes and the live file still resolves: the in-place
// hash misses (raw bytes differ) but the CST relocation finds the matching node,
// because tree-sitter nodes for a func body are unaffected by trailing whitespace
// elsewhere. The edit applies rather than rejecting.
func TestPrepareWhitespaceShiftReResolves(t *testing.T) {
	dir := t.TempDir()
	// Live prepends a blank comment line, shifting every func down without
	// changing any func's own bytes.
	live := "// header\n" + goSrc
	path := writeFile(t, dir, "main.go", live)
	s := newSession(t, nil, nil)

	// Anchor c() against the ORIGINAL goSrc offsets (now stale in live).
	cStart := strings.Index(goSrc, "func c() {}")
	cEnd := cStart + len("func c() {}")
	e := regionEdit(path, goSrc, cStart, cEnd, "func c() { _ = 0 }")

	plan, err := s.Prepare(context.Background(), []region.Edit{e}, nil)
	if err != nil {
		t.Fatalf("Prepare: %v", err)
	}
	if _, err := s.Commit(plan); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	final := readFile(t, path)
	if !strings.Contains(final, "func c() { _ = 0 }") {
		t.Fatalf("shifted region did not re-resolve:\n%s", final)
	}
	if !strings.HasPrefix(final, "// header\n") {
		t.Fatalf("header lost, edit misapplied:\n%s", final)
	}
}
