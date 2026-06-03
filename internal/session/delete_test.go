package session

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/wal"
)

// TestDeleteFileRemovesMatching deletes an existing file whose live raw_hash
// matches the op's expected anchor: the file is unlinked and the success result
// carries the path and the confirmed raw_hash so a host can close that node.
func TestDeleteFileRemovesMatching(t *testing.T) {
	dir := t.TempDir()
	content := "package main\n\nfunc main() {}\n"
	path := writeFile(t, dir, "doomed.go", content)
	s := createSession(t)

	rawHash := hashing.RawHash(hashing.XXHasher{}, []byte(content))
	res, err := s.DeleteFile(context.Background(), Op{Kind: OpDelete, Path: path, ExpectedRawHash: rawHash})
	if err != nil {
		t.Fatalf("DeleteFile: %v", err)
	}
	if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
		t.Fatalf("DeleteFile left the file behind: stat err = %v", statErr)
	}
	if res.Path != path {
		t.Fatalf("result path = %q, want %q", res.Path, path)
	}
	if res.RawHash != rawHash {
		t.Fatalf("result raw hash = %q, want %q", res.RawHash, rawHash)
	}
}

// TestDeleteFileRejectsDrift verifies the raw_hash drift gate: when the live
// file no longer hashes to the op's expected raw_hash (a concurrent change the
// caller did not see), the delete HARD-REJECTS with a ConflictError and the
// file is STILL THERE — Båge never discards bytes the caller did not see.
func TestDeleteFileRejectsDrift(t *testing.T) {
	dir := t.TempDir()
	live := "package main\n\nfunc main() { println(1) }\n"
	path := writeFile(t, dir, "drifted.go", live)
	s := createSession(t)

	stale := hashing.RawHash(hashing.XXHasher{}, []byte("package main\n\nfunc main() {}\n"))
	_, err := s.DeleteFile(context.Background(), Op{Kind: OpDelete, Path: path, ExpectedRawHash: stale})
	if err == nil {
		t.Fatalf("DeleteFile on drift = nil error, want reject")
	}
	if !errors.Is(err, ErrConflict) {
		t.Fatalf("error = %v, want ErrConflict", err)
	}
	if got := readFile(t, path); got != live {
		t.Fatalf("drift-rejected delete altered the file: %q, want %q", got, live)
	}
	// A drift reject must NOT have logged a Deletes record naming the file.
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("drift reject left %d WAL intents, want 0", len(intents))
	}
}

// TestDeleteFileRejectsMissing verifies that deleting a path that does not exist
// is rejected (nothing to delete), distinct from a drift reject.
func TestDeleteFileRejectsMissing(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "ghost.go")
	s := createSession(t)

	_, err := s.DeleteFile(context.Background(), Op{Kind: OpDelete, Path: path, ExpectedRawHash: "anything"})
	if err == nil {
		t.Fatalf("DeleteFile on missing path = nil error, want reject")
	}
	if errors.Is(err, ErrConflict) {
		t.Fatalf("missing-file delete = %v, want a not-found reject distinct from drift", err)
	}
}

// TestDeleteFileWALCapturesOriginalBeforeUnlink proves the undo ordering: the
// durable WAL record carrying the FULL original bytes must precede the
// destructive unlink, so a crash between them is recoverable. A successful
// delete clears the WAL, so to OBSERVE the captured bytes we force the unlink to
// fail by making the target's PARENT directory unwritable (a directory entry
// cannot be removed without write permission on its parent). The live file is
// still readable, so the drift gate passes and the WAL is appended; the unlink
// then fails, leaving the original-bytes record visible for the assertion.
func TestDeleteFileWALCapturesOriginalBeforeUnlink(t *testing.T) {
	if os.Getuid() == 0 {
		t.Skip("running as root bypasses directory write permissions")
	}
	dir := t.TempDir()
	parent := filepath.Join(dir, "ro")
	if err := os.MkdirAll(parent, 0o755); err != nil {
		t.Fatalf("seed parent: %v", err)
	}
	content := "keep these bytes safe before unlink\n"
	path := writeFile(t, parent, "real.txt", content)
	// Make the parent unwritable so os.Remove(path) fails AFTER the WAL append.
	if err := os.Chmod(parent, 0o555); err != nil {
		t.Fatalf("chmod parent ro: %v", err)
	}
	t.Cleanup(func() { _ = os.Chmod(parent, 0o755) })

	s := createSession(t)
	rawHash := hashing.RawHash(hashing.XXHasher{}, []byte(content))
	_, err := s.DeleteFile(context.Background(), Op{Kind: OpDelete, Path: path, ExpectedRawHash: rawHash})
	if err == nil {
		t.Fatalf("DeleteFile with unwritable parent = nil, want unlink failure")
	}
	intents, rerr := wal.Replay(s.WALDir)
	if rerr != nil {
		t.Fatalf("Replay: %v", rerr)
	}
	if len(intents) != 1 {
		t.Fatalf("WAL has %d intents, want 1 capturing the original before the failed unlink", len(intents))
	}
	in := intents[0]
	found := false
	for _, d := range in.Deletes {
		if d == path {
			found = true
		}
	}
	if !found {
		t.Fatalf("WAL intent did not record %q in Deletes", path)
	}
	if _, ok := in.Originals[path]; !ok {
		t.Fatalf("WAL intent did not capture original bytes for %q before unlink", path)
	}
}

// TestRecoverRestoresDeletedFile simulates a crash AFTER the delete's WAL record
// (carrying the original bytes) was durable but BEFORE the unlink + WAL clear:
// the file is already gone and a Deletes intent names it with its original bytes
// in Originals. Recover must RESTORE the deleted file's original bytes.
func TestRecoverRestoresDeletedFile(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")
	gone := filepath.Join(dir, "gone.go")
	original := []byte("package gone\n\nfunc Restore() {}\n")

	// A WAL record naming the deleted path with its captured original bytes, as
	// DeleteFile persists BEFORE the unlink.
	if err := wal.Append(walDir, wal.Intent{
		ID:        "crash",
		Deletes:   []string{gone},
		Originals: map[string][]byte{gone: original},
	}); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}
	if got := readFile(t, gone); got != string(original) {
		t.Fatalf("Recover did not restore deleted file: %q, want %q", got, string(original))
	}
	intents, err := wal.Replay(walDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != 0 {
		t.Fatalf("Recover left WAL records: %#v", intents)
	}
}

// TestDeleteFileWALClearedOnSuccess verifies a successful delete leaves no
// pending WAL intent (the destructive op settled cleanly).
func TestDeleteFileWALClearedOnSuccess(t *testing.T) {
	dir := t.TempDir()
	content := "delete me\n"
	path := writeFile(t, dir, "tmp.txt", content)
	s := createSession(t)

	rawHash := hashing.RawHash(hashing.XXHasher{}, []byte(content))
	if _, err := s.DeleteFile(context.Background(), Op{Kind: OpDelete, Path: path, ExpectedRawHash: rawHash}); err != nil {
		t.Fatalf("DeleteFile: %v", err)
	}
	intents, err := wal.Replay(s.WALDir)
	if err != nil {
		t.Fatalf("Replay: %v", err)
	}
	if len(intents) != 0 {
		t.Fatalf("WAL not cleared after successful delete: %#v", intents)
	}
}

// TestDeleteFileRejectsNonDeleteKind verifies DeleteFile only accepts OpDelete.
func TestDeleteFileRejectsNonDeleteKind(t *testing.T) {
	s := createSession(t)
	_, err := s.DeleteFile(context.Background(), Op{Kind: OpCreate, Path: "x.go"})
	if err == nil || !strings.Contains(err.Error(), "delete") {
		t.Fatalf("non-delete Op kind = %v, want a delete-kind reject", err)
	}
}
