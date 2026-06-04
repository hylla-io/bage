package session

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/wal"
)

// TestMoveFileRelocatesBytes moves an existing file whose live raw_hash matches
// the op's source anchor to a fresh destination: the source is gone, the
// destination holds the EXACT source bytes unchanged, and the result names the
// removed source plus the destination's whole-file hashes so a host can
// re-identify the moved nodes.
func TestMoveFileRelocatesBytes(t *testing.T) {
	dir := t.TempDir()
	content := "package main\n\nfunc main() {}\n"
	from := writeFile(t, dir, "from.go", content)
	to := filepath.Join(dir, "sub", "to.go")
	s := createSession(t)

	rawHash := hashing.RawHash(hashing.XXHasher{}, []byte(content))
	res, err := s.MoveFile(context.Background(), Op{Kind: OpMove, Path: from, To: to, ExpectedRawHash: rawHash})
	if err != nil {
		t.Fatalf("MoveFile: %v", err)
	}
	if _, statErr := os.Stat(from); !os.IsNotExist(statErr) {
		t.Fatalf("move left the source behind: stat err = %v", statErr)
	}
	if got := readFile(t, to); got != content {
		t.Fatalf("destination bytes = %q, want exact source %q", got, content)
	}
	if res.From != from {
		t.Fatalf("result From = %q, want %q", res.From, from)
	}
	if res.Dest.Path != to {
		t.Fatalf("result Dest.Path = %q, want %q", res.Dest.Path, to)
	}
	if res.Dest.NewFileRawHash != rawHash {
		t.Fatalf("dest raw hash = %q, want %q (relocate must preserve bytes)", res.Dest.NewFileRawHash, rawHash)
	}
	// A successful move clears the WAL.
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL not cleared after successful move: %#v", intents)
	}
}

// TestMoveFileRejectsSourceDrift verifies the SOURCE drift gate: when the live
// source no longer hashes to the op's expected raw_hash (a concurrent change the
// caller did not see), the move HARD-REJECTS with a ConflictError and NOTHING
// moves — the source is intact and no destination is created.
func TestMoveFileRejectsSourceDrift(t *testing.T) {
	dir := t.TempDir()
	live := "package main\n\nfunc main() { println(1) }\n"
	from := writeFile(t, dir, "drift.go", live)
	to := filepath.Join(dir, "dest.go")
	s := createSession(t)

	stale := hashing.RawHash(hashing.XXHasher{}, []byte("package main\n\nfunc main() {}\n"))
	_, err := s.MoveFile(context.Background(), Op{Kind: OpMove, Path: from, To: to, ExpectedRawHash: stale})
	if err == nil {
		t.Fatalf("MoveFile on source drift = nil error, want reject")
	}
	if !errors.Is(err, ErrConflict) {
		t.Fatalf("error = %v, want ErrConflict", err)
	}
	if got := readFile(t, from); got != live {
		t.Fatalf("drift-rejected move altered the source: %q, want %q", got, live)
	}
	if _, statErr := os.Stat(to); !os.IsNotExist(statErr) {
		t.Fatalf("drift-rejected move created a destination: stat err = %v", statErr)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("drift reject left %d WAL intents, want 0", len(intents))
	}
}

// TestMoveFileRejectsExistingDest verifies the DESTINATION non-existence anchor:
// when the destination already exists the move HARD-REJECTS with ErrExists, the
// destination is NEVER clobbered, and the source is left intact (priority 2).
func TestMoveFileRejectsExistingDest(t *testing.T) {
	dir := t.TempDir()
	srcContent := "package src\n"
	from := writeFile(t, dir, "src.go", srcContent)
	destContent := "DO NOT CLOBBER\n"
	to := writeFile(t, dir, "dest.txt", destContent)
	s := createSession(t)

	rawHash := hashing.RawHash(hashing.XXHasher{}, []byte(srcContent))
	_, err := s.MoveFile(context.Background(), Op{Kind: OpMove, Path: from, To: to, ExpectedRawHash: rawHash})
	if err == nil {
		t.Fatalf("MoveFile onto existing destination = nil error, want reject")
	}
	if !errors.Is(err, ErrExists) {
		t.Fatalf("error = %v, want ErrExists", err)
	}
	if got := readFile(t, to); got != destContent {
		t.Fatalf("destination clobbered: %q, want %q", got, destContent)
	}
	if got := readFile(t, from); got != srcContent {
		t.Fatalf("rejected move altered the source: %q, want %q", got, srcContent)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("dest-exists reject left %d WAL intents, want 0", len(intents))
	}
}

// TestMoveFileRejectsMissingSource verifies that moving a path that does not
// exist is rejected (nothing to move), distinct from a drift reject, and no
// destination is created.
func TestMoveFileRejectsMissingSource(t *testing.T) {
	dir := t.TempDir()
	from := filepath.Join(dir, "ghost.go")
	to := filepath.Join(dir, "dest.go")
	s := createSession(t)

	_, err := s.MoveFile(context.Background(), Op{Kind: OpMove, Path: from, To: to, ExpectedRawHash: "anything"})
	if err == nil {
		t.Fatalf("MoveFile on missing source = nil error, want reject")
	}
	if !errors.Is(err, ErrNotFound) {
		t.Fatalf("error = %v, want ErrNotFound", err)
	}
	if errors.Is(err, ErrConflict) {
		t.Fatalf("missing-source move = %v, want a not-found reject distinct from drift", err)
	}
	if _, statErr := os.Stat(to); !os.IsNotExist(statErr) {
		t.Fatalf("missing-source move created a destination: stat err = %v", statErr)
	}
}

// TestMoveFileWALRecordsMoveBeforeUnlink proves the recoverable-window ordering:
// the durable WAL record carrying the {From,To} move and the FULL source bytes in
// Originals must precede the destructive source unlink, so a crash between them is
// recoverable. A successful move clears the WAL, so to OBSERVE the record we force
// the source unlink to fail by making the SOURCE's parent directory unwritable (a
// directory entry cannot be removed without write permission on its parent). The
// source is still readable so the drift gate passes and the destination is
// written; the source unlink then fails, leaving the move record visible.
func TestMoveFileWALRecordsMoveBeforeUnlink(t *testing.T) {
	if os.Getuid() == 0 {
		t.Skip("running as root bypasses directory write permissions")
	}
	dir := t.TempDir()
	srcParent := filepath.Join(dir, "ro")
	if err := os.MkdirAll(srcParent, 0o755); err != nil {
		t.Fatalf("seed src parent: %v", err)
	}
	content := "keep these source bytes safe before unlink\n"
	from := writeFile(t, srcParent, "real.txt", content)
	to := filepath.Join(dir, "dest.txt")
	// Make the SOURCE parent unwritable so os.Remove(from) fails AFTER the WAL append.
	if err := os.Chmod(srcParent, 0o555); err != nil {
		t.Fatalf("chmod src parent ro: %v", err)
	}
	t.Cleanup(func() { _ = os.Chmod(srcParent, 0o755) })

	s := createSession(t)
	rawHash := hashing.RawHash(hashing.XXHasher{}, []byte(content))
	_, err := s.MoveFile(context.Background(), Op{Kind: OpMove, Path: from, To: to, ExpectedRawHash: rawHash})
	if err == nil {
		t.Fatalf("MoveFile with unwritable source parent = nil, want unlink failure")
	}
	intents, rerr := wal.Replay(s.WALDir)
	if rerr != nil {
		t.Fatalf("Replay: %v", rerr)
	}
	if len(intents) != 1 {
		t.Fatalf("WAL has %d intents, want 1 recording the move before the failed unlink", len(intents))
	}
	in := intents[0]
	if len(in.Moves) != 1 || in.Moves[0].From != from || in.Moves[0].To != to {
		t.Fatalf("WAL intent did not record the move {From:%q,To:%q}: %#v", from, to, in.Moves)
	}
	if got, ok := in.Originals[from]; !ok || string(got) != content {
		t.Fatalf("WAL intent did not capture the source bytes before unlink: ok=%v got=%q", ok, string(got))
	}
}

// TestRecoverConvergesCrashedMove simulates a crash AFTER the move's WAL record
// (carrying the source bytes + {From,To}) was durable but BEFORE the source unlink
// + WAL clear: both the source and the destination are present on disk and a Moves
// intent names them. Recover must converge to FULLY-MOVED — the destination holds
// the source bytes and the source is gone — WITHOUT losing the source content.
func TestRecoverConvergesCrashedMove(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")
	from := filepath.Join(dir, "from.go")
	to := filepath.Join(dir, "to.go")
	content := []byte("package moved\n\nfunc Moved() {}\n")

	// Crash state: the destination was written and the source still present, as
	// MoveFile leaves it after the durable WAL append but before the source unlink.
	if err := os.WriteFile(from, content, 0o644); err != nil {
		t.Fatalf("seed source: %v", err)
	}
	if err := os.WriteFile(to, content, 0o644); err != nil {
		t.Fatalf("seed dest: %v", err)
	}
	if err := wal.Append(walDir, wal.Intent{
		ID:        "crash",
		Moves:     []wal.Move{{From: from, To: to}},
		Originals: map[string][]byte{from: content},
	}); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}
	if _, statErr := os.Stat(from); !os.IsNotExist(statErr) {
		t.Fatalf("Recover did not remove the move source: stat err = %v", statErr)
	}
	if got := readFile(t, to); got != string(content) {
		t.Fatalf("Recover lost/altered the moved bytes at dest: %q, want %q", got, string(content))
	}
	intents, err := wal.Replay(walDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != 0 {
		t.Fatalf("Recover left WAL records: %#v", intents)
	}
}

// TestRecoverConvergesMoveBeforeDestWritten simulates the earliest crash point: the
// move's WAL record is durable but the destination was NOT yet written (only the
// source exists). Recover must still converge to fully-moved using Originals as the
// backstop — the destination is created with the source bytes and the source
// removed — so the source bytes are never lost.
func TestRecoverConvergesMoveBeforeDestWritten(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")
	from := filepath.Join(dir, "from.go")
	to := filepath.Join(dir, "to.go")
	content := []byte("package early\n")

	if err := os.WriteFile(from, content, 0o644); err != nil {
		t.Fatalf("seed source: %v", err)
	}
	// Destination intentionally NOT created (crash before dest write).
	if err := wal.Append(walDir, wal.Intent{
		ID:        "crash-early",
		Moves:     []wal.Move{{From: from, To: to}},
		Originals: map[string][]byte{from: content},
	}); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}
	if got := readFile(t, to); got != string(content) {
		t.Fatalf("Recover did not converge dest from Originals backstop: %q, want %q", got, string(content))
	}
	if _, statErr := os.Stat(from); !os.IsNotExist(statErr) {
		t.Fatalf("Recover did not remove the source: stat err = %v", statErr)
	}
}

// TestMoveFileLockOrderingDeadlockFree drives two crossing moves concurrently
// (A->B and B->A over the same pair of paths) to prove the deterministic SORTED
// lock acquisition is deadlock-free. Both moves cannot succeed (whichever loses
// the destination-existence race rejects), but the test asserts only that BOTH
// calls RETURN (no deadlock) within the goroutines, plus that the source bytes are
// never lost. A hang would fail the -race/-timeout gate.
func TestMoveFileLockOrderingDeadlockFree(t *testing.T) {
	dir := t.TempDir()
	a := writeFile(t, dir, "a.txt", "alpha\n")
	b := writeFile(t, dir, "b.txt", "bravo\n")
	s := createSession(t)

	aHash := hashing.RawHash(hashing.XXHasher{}, []byte("alpha\n"))
	bHash := hashing.RawHash(hashing.XXHasher{}, []byte("bravo\n"))

	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		_, _ = s.MoveFile(context.Background(), Op{Kind: OpMove, Path: a, To: b, ExpectedRawHash: aHash})
	}()
	go func() {
		defer wg.Done()
		_, _ = s.MoveFile(context.Background(), Op{Kind: OpMove, Path: b, To: a, ExpectedRawHash: bHash})
	}()
	wg.Wait() // a deadlock would hang here and trip the test timeout

	// No source bytes were lost: every byte that existed (alpha and bravo) is still
	// somewhere on disk across {a,b}, regardless of which move(s) won.
	seen := map[string]bool{}
	for _, p := range []string{a, b} {
		if data, err := os.ReadFile(p); err == nil {
			seen[string(data)] = true
		}
	}
	if !seen["alpha\n"] {
		t.Fatalf("alpha bytes lost across crossing moves: %v", seen)
	}
	if !seen["bravo\n"] {
		t.Fatalf("bravo bytes lost across crossing moves: %v", seen)
	}
}

// TestMoveFileRejectsNonMoveKind verifies MoveFile only accepts OpMove.
func TestMoveFileRejectsNonMoveKind(t *testing.T) {
	s := createSession(t)
	_, err := s.MoveFile(context.Background(), Op{Kind: OpCreate, Path: "x.go", To: "y.go"})
	if err == nil || !strings.Contains(err.Error(), "move") {
		t.Fatalf("non-move Op kind = %v, want a move-kind reject", err)
	}
}
