package session

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
	"github.com/hylla-io/bage/internal/wal"
)

// createSession builds a Session with the real tree-sitter adapter and
// auto-detect language (Lang unset) so CreateFile resolves each path's grammar
// from its extension, exercising the langFor auto-detect path.
func createSession(t *testing.T) *Session {
	t.Helper()
	return &Session{
		Parser: treesitter.New(),
		Hasher: hashing.XXHasher{},
		WALDir: filepath.Join(t.TempDir(), "wal"),
	}
}

// TestCreateFileNewFile creates a brand-new file and asserts the bytes landed
// and the EditResult carries the correct whole-file raw/norm hashes and line
// range so a host can ingest the new file.
func TestCreateFileNewFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "new.go")
	content := "package main\n\nfunc main() {}\n"
	s := createSession(t)

	res, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: content})
	if err != nil {
		t.Fatalf("CreateFile: %v", err)
	}
	if got := readFile(t, path); got != content {
		t.Fatalf("file content = %q, want %q", got, content)
	}

	h := hashing.XXHasher{}
	wantRaw := hashing.RawHash(h, []byte(content))
	wantNorm := hashing.NormHash(h, []byte(content))
	if res.NewFileRawHash != wantRaw {
		t.Fatalf("raw hash = %q, want %q", res.NewFileRawHash, wantRaw)
	}
	if res.NewFileNormHash != wantNorm {
		t.Fatalf("norm hash = %q, want %q", res.NewFileNormHash, wantNorm)
	}
	if res.Path != path {
		t.Fatalf("result path = %q, want %q", res.Path, path)
	}
	// Whole-file result: changed range spans the entire content.
	if res.ChangedStart != 0 || res.ChangedEnd != len(content) {
		t.Fatalf("changed range = [%d:%d], want [0:%d]", res.ChangedStart, res.ChangedEnd, len(content))
	}
	if res.NewStartLine != 1 {
		t.Fatalf("start line = %d, want 1", res.NewStartLine)
	}
}

// TestCreateFileRejectsExisting verifies the non-existence anchor: a create
// against a path that already exists is HARD REJECTED and the pre-existing
// content is never clobbered.
func TestCreateFileRejectsExisting(t *testing.T) {
	dir := t.TempDir()
	original := "DO NOT CLOBBER\n"
	path := writeFile(t, dir, "exists.txt", original)
	s := createSession(t)

	_, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: "new content\n"})
	if err == nil {
		t.Fatalf("CreateFile over existing path = nil error, want reject")
	}
	if !errors.Is(err, ErrExists) {
		t.Fatalf("error = %v, want ErrExists", err)
	}
	if got := readFile(t, path); got != original {
		t.Fatalf("existing file clobbered: %q, want %q", got, original)
	}
}

// TestCreateFileRejectsExistingLeavesNoWAL is the CLOBBER-VIA-RECOVERY
// regression (blocker fix): the non-existence anchor must be established BEFORE
// the create is made durable, so a rejected create over a PRE-EXISTING file can
// never leave a WAL record that names that file. If it did, a crash between the
// WAL append and the reject-path clear would let the next Recover unlink the
// user's pre-existing file. This asserts that after a rejected create the WAL
// holds NO intent naming the pre-existing path, and a subsequent Recover (the
// crash-recovery path) PRESERVES the pre-existing content.
func TestCreateFileRejectsExistingLeavesNoWAL(t *testing.T) {
	dir := t.TempDir()
	original := "precious user content — DO NOT DELETE\n"
	path := writeFile(t, dir, "precious.txt", original)
	s := createSession(t)

	_, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: "usurper\n"})
	if !errors.Is(err, ErrExists) {
		t.Fatalf("CreateFile over existing = %v, want ErrExists", err)
	}

	// The rejected create must NOT have logged a Creates record naming the
	// pre-existing file: a durable record here would be a delete-the-user's-file
	// landmine for Recover.
	intents, err := wal.Replay(s.WALDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	for _, in := range intents {
		for _, c := range in.Creates {
			if c == path {
				t.Fatalf("rejected create left a WAL Creates record naming the pre-existing file %q", path)
			}
		}
	}

	// Crash-recovery over whatever the WAL holds must PRESERVE the user's file.
	if err := s.Recover(context.Background(), s.WALDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}
	if got := readFile(t, path); got != original {
		t.Fatalf("Recover destroyed pre-existing file: %q, want %q", got, original)
	}
}

// TestCreateFileRejectsExistingBeforeWAL proves the ORDERING invariant behind
// the clobber-via-recovery fix: the O_EXCL existence check must run BEFORE the
// WAL append, so a pre-existing target rejects with ErrExists without ever
// reaching wal.Append. It seeds a pre-existing file, then makes the WAL log path
// unwritable by planting a *file* where the WAL log's parent directory must be:
// if (and only if) the engine ever tried to wal.Append for this rejected create,
// MkdirAll/Append would surface that I/O failure instead of the clean ErrExists.
// Observing ErrExists (not a WAL error) proves the append was never attempted.
func TestCreateFileRejectsExistingBeforeWAL(t *testing.T) {
	dir := t.TempDir()
	original := "keep me\n"
	path := writeFile(t, dir, "keep.txt", original)

	// Plant a regular file at the would-be WAL directory path so any attempt to
	// MkdirAll/open the WAL log fails loudly. A correct (open-first) CreateFile
	// never touches the WAL for a pre-existing target, so this trap stays unsprung.
	walAsFile := filepath.Join(dir, "waltrap")
	if err := os.WriteFile(walAsFile, []byte("x"), 0o644); err != nil {
		t.Fatalf("plant wal trap: %v", err)
	}
	s := &Session{
		Parser: treesitter.New(),
		Hasher: hashing.XXHasher{},
		WALDir: filepath.Join(walAsFile, "wal"), // MkdirAll here would fail (parent is a file)
	}

	_, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: "usurper\n"})
	if !errors.Is(err, ErrExists) {
		t.Fatalf("CreateFile over existing = %v, want a clean ErrExists (proves WAL append never attempted)", err)
	}
	if got := readFile(t, path); got != original {
		t.Fatalf("pre-existing file clobbered: %q, want %q", got, original)
	}
}

// TestCreateFileParseFloorRejects verifies the parse floor: when the parser
// rejects the staged bytes, CreateFile rejects and NOTHING is written (no file,
// no leftover, and the just-logged WAL record is cleared). tree-sitter's grammar
// is error-tolerant and never returns a Parse error for malformed syntax, so the
// parse-block path (SPEC §8.4) is exercised by injecting the failure via
// errParser (the same technique TestPrepareParseFailureBlocks uses).
func TestCreateFileParseFloorRejects(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "broken.go")
	content := "package main\n\nfunc SENTINEL_BODY() {}\n"
	s := createSession(t)
	parseErr := errors.New("parse boom")
	s.Parser = &errParser{ParserPort: treesitter.New(), failOn: []byte("SENTINEL_BODY"), err: parseErr}

	_, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: content})
	if !errors.Is(err, parseErr) {
		t.Fatalf("CreateFile parse-floor = %v, want parseErr", err)
	}
	if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
		t.Fatalf("rejected create left a file behind: stat err = %v", statErr)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL has %d intents after rejected create, want 0", len(intents))
	}
}

// TestCreateFileLangAutoDetect proves the language is auto-detected from the
// path when Op carries no explicit lang. A .py path content that parses cleanly
// as Python but would NOT parse as Go is created successfully, confirming the
// session used the path-detected grammar (Python) rather than its zero-value.
func TestCreateFileLangAutoDetect(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auto.py")
	content := "def hello():\n    return 42\n"
	s := createSession(t)

	res, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: content})
	if err != nil {
		t.Fatalf("auto-detected .py create: %v", err)
	}
	if got := readFile(t, path); got != content {
		t.Fatalf("py content = %q, want %q", got, content)
	}
	if res.NewStartLine != 1 {
		t.Fatalf("start line = %d, want 1", res.NewStartLine)
	}
}

// TestCreateFileLangText creates a non-code .txt file: LangText always parses,
// so any byte content is accepted losslessly.
func TestCreateFileLangText(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "note.txt")
	content := "free-form text {[(<not code>)]}\nsecond line\n"
	s := createSession(t)

	res, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: content})
	if err != nil {
		t.Fatalf("CreateFile .txt: %v", err)
	}
	if got := readFile(t, path); got != content {
		t.Fatalf("txt content = %q, want %q", got, content)
	}
	if res.NewEndLine != 3 {
		t.Fatalf("end line = %d, want 3", res.NewEndLine)
	}
}

// TestCreateFileMkdirParents verifies mkdir -p: a target under a not-yet-existing
// parent directory chain creates the directories then the file.
func TestCreateFileMkdirParents(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "a", "b", "c", "deep.go")
	content := "package deep\n"
	s := createSession(t)

	if _, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: content}); err != nil {
		t.Fatalf("CreateFile deep path: %v", err)
	}
	if got := readFile(t, path); got != content {
		t.Fatalf("deep content = %q, want %q", got, content)
	}
}

// langRecorder wraps a parser and records the Lang passed to its final Parse
// call, so a test can assert which grammar the parse floor used.
type langRecorder struct {
	parser.ParserPort
	last parser.Lang
}

func (p *langRecorder) Parse(ctx context.Context, lang parser.Lang, src []byte) (*parser.Tree, error) {
	p.last = lang
	return p.ParserPort.Parse(ctx, lang, src)
}

// TestCreateFileExplicitLang verifies an explicit Op.Lang overrides path-based
// auto-detect: a .txt path (which would auto-detect to LangText) with Lang=Go
// drives the Go grammar through the parse floor. A langRecorder captures the
// language the floor used, proving the per-op override won over the path.
func TestCreateFileExplicitLang(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "code.txt")
	content := "package main\n\nfunc q() {}\n"
	rec := &langRecorder{ParserPort: treesitter.New()}
	s := createSession(t)
	s.Parser = rec

	op := Op{Kind: OpCreate, Path: path, Content: content, Lang: parser.LangGo}
	if _, err := s.CreateFile(context.Background(), op); err != nil {
		t.Fatalf("CreateFile explicit lang: %v", err)
	}
	if rec.last != parser.LangGo {
		t.Fatalf("parse floor used lang %v, want LangGo (explicit override)", rec.last)
	}
}

// TestCreateFileWALRecordsCreate verifies the create is WAL-logged with its
// path in Creates and that the WAL is cleared on success.
func TestCreateFileWALRecordsCreate(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "logged.go")
	s := createSession(t)

	if _, err := s.CreateFile(context.Background(), Op{Kind: OpCreate, Path: path, Content: "package logged\n"}); err != nil {
		t.Fatalf("CreateFile: %v", err)
	}
	// On full success the WAL log is cleared, leaving no pending intents.
	intents, err := wal.Replay(s.WALDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != 0 {
		t.Fatalf("WAL not cleared after successful create: %#v", intents)
	}
}

// TestRecoverUnlinksHalfCreatedFile simulates a crash AFTER the create's WAL
// record was durable but BEFORE the WAL was cleared (Commit point not reached):
// a stray file sits on disk and a Creates intent names it. Recover must UNLINK
// the half-created file (create's undo is unlink) and clear the WAL.
func TestRecoverUnlinksHalfCreatedFile(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")
	half := filepath.Join(dir, "half.go")
	if err := os.WriteFile(half, []byte("package half\n"), 0o644); err != nil {
		t.Fatalf("seed half file: %v", err)
	}
	// A WAL record naming the half-created path, as CreateFile would persist
	// before the temp->rename + WAL-clear.
	if err := wal.Append(walDir, wal.Intent{ID: "crash", Creates: []string{half}}); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}
	if _, statErr := os.Stat(half); !os.IsNotExist(statErr) {
		t.Fatalf("Recover did not unlink half-created file: stat err = %v", statErr)
	}
	intents, err := wal.Replay(walDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != 0 {
		t.Fatalf("Recover left WAL records: %#v", intents)
	}
}

// TestCreateFileRejectsNonCreateKind verifies the Op abstraction is honestly
// tagged: CreateFile only accepts OpCreate (Delete/Move are later slices).
func TestCreateFileRejectsNonCreateKind(t *testing.T) {
	s := createSession(t)
	_, err := s.CreateFile(context.Background(), Op{Kind: OpKind(99), Path: "x.go", Content: ""})
	if err == nil || !strings.Contains(err.Error(), "create") {
		t.Fatalf("non-create Op kind = %v, want a create-kind reject", err)
	}
}
