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

// editOp builds an OpEdit over a [start:end) byte region of src in path, anchored
// by the region_hash and replacing the region with newText, mirroring the
// region.Edit the edit path consumes.
func editOp(path, src string, start, end int, newText string) Op {
	e := regionEdit(path, src, start, end, newText)
	return Op{Kind: OpEdit, Path: path, Edit: &e}
}

// TestApplyBatchAllSucceed applies a heterogeneous 4-op batch (create + edit +
// delete + move) as ONE all-or-nothing change and asserts every op landed: the
// created file exists, the edited file carries the new text, the deleted file is
// gone, the moved file relocated unchanged, and the WAL is cleared on success.
func TestApplyBatchAllSucceed(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)
	h := hashing.XXHasher{}

	// Edit target: a Go file whose func a() body is replaced.
	editSrc := goSrc
	editPath := writeFile(t, dir, "edit.go", editSrc)
	estart := strings.Index(editSrc, "func a() {}")
	eend := estart + len("func a() {}")

	// Delete target: gated by its raw_hash.
	delContent := "package del\n\nfunc D() {}\n"
	delPath := writeFile(t, dir, "del.go", delContent)
	delHash := hashing.RawHash(h, []byte(delContent))

	// Move target: relocate src.go -> sub/dst.go.
	moveContent := "package mv\n\nfunc M() {}\n"
	movePath := writeFile(t, dir, "src.go", moveContent)
	moveDest := filepath.Join(dir, "sub", "dst.go")
	moveHash := hashing.RawHash(h, []byte(moveContent))

	// Create target: a brand-new file.
	createPath := filepath.Join(dir, "new.go")
	createContent := "package fresh\n\nfunc F() {}\n"

	ops := []Op{
		{Kind: OpCreate, Path: createPath, Content: createContent},
		editOp(editPath, editSrc, estart, eend, "func a() { return }"),
		{Kind: OpDelete, Path: delPath, ExpectedRawHash: delHash},
		{Kind: OpMove, Path: movePath, To: moveDest, ExpectedRawHash: moveHash},
	}

	results, err := s.ApplyBatch(context.Background(), ops)
	if err != nil {
		t.Fatalf("ApplyBatch: %v", err)
	}
	if len(results) != len(ops) {
		t.Fatalf("got %d results, want %d (one per op)", len(results), len(ops))
	}

	if got := readFile(t, createPath); got != createContent {
		t.Fatalf("create did not land: %q", got)
	}
	if got := readFile(t, editPath); !strings.Contains(got, "func a() { return }") {
		t.Fatalf("edit did not land: %q", got)
	}
	if _, statErr := os.Stat(delPath); !os.IsNotExist(statErr) {
		t.Fatalf("delete left the file behind: %v", statErr)
	}
	if _, statErr := os.Stat(movePath); !os.IsNotExist(statErr) {
		t.Fatalf("move left the source behind: %v", statErr)
	}
	if got := readFile(t, moveDest); got != moveContent {
		t.Fatalf("move dest bytes = %q, want %q", got, moveContent)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("WAL not cleared after successful batch: %#v", intents)
	}
}

// TestApplyBatchRejectsOnStaleAnchor proves all-or-nothing on a VALIDATE-phase
// reject: one op's anchor is stale (the delete's raw_hash no longer matches), so
// the WHOLE batch is rejected having written NOTHING — every file is byte-identical
// to before and no WAL record survives.
func TestApplyBatchRejectsOnStaleAnchor(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)
	h := hashing.XXHasher{}

	editSrc := goSrc
	editPath := writeFile(t, dir, "edit.go", editSrc)
	estart := strings.Index(editSrc, "func a() {}")
	eend := estart + len("func a() {}")

	delContent := "package del\n\nfunc D() {}\n"
	delPath := writeFile(t, dir, "del.go", delContent)
	staleHash := hashing.RawHash(h, []byte("package del\n\nfunc DIFFERENT() {}\n"))

	createPath := filepath.Join(dir, "new.go")

	ops := []Op{
		editOp(editPath, editSrc, estart, eend, "func a() { return }"),
		{Kind: OpDelete, Path: delPath, ExpectedRawHash: staleHash}, // stale anchor
		{Kind: OpCreate, Path: createPath, Content: "package fresh\n"},
	}

	_, err := s.ApplyBatch(context.Background(), ops)
	if err == nil {
		t.Fatalf("ApplyBatch with a stale anchor = nil error, want whole-batch reject")
	}
	if !errors.Is(err, ErrConflict) {
		t.Fatalf("error = %v, want ErrConflict (raw_hash drift)", err)
	}

	// Filesystem byte-identical to before: NOTHING was applied.
	if got := readFile(t, editPath); got != editSrc {
		t.Fatalf("rejected batch altered the edit file: %q", got)
	}
	if got := readFile(t, delPath); got != delContent {
		t.Fatalf("rejected batch altered the delete target: %q", got)
	}
	if _, statErr := os.Stat(createPath); !os.IsNotExist(statErr) {
		t.Fatalf("rejected batch created a file: %v", statErr)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("rejected batch left %d WAL intents, want 0", len(intents))
	}
}

// TestApplyBatchRollsBackOnApplyFailure proves all-or-nothing on an APPLY-phase
// failure: validation passes for every op, the unified WAL intent is appended, then
// one op fails to apply mid-batch. Every already-applied op must be ROLLED BACK from
// the unified intent so the filesystem returns to its pre-batch state. The failure
// is forced by making a create's target parent unwritable, so the create's O_EXCL
// open fails AFTER earlier ops (an edit + a delete) already applied.
func TestApplyBatchRollsBackOnApplyFailure(t *testing.T) {
	if os.Getuid() == 0 {
		t.Skip("running as root bypasses directory write permissions")
	}
	dir := t.TempDir()
	s := createSession(t)
	h := hashing.XXHasher{}

	editSrc := goSrc
	editPath := writeFile(t, dir, "edit.go", editSrc)
	estart := strings.Index(editSrc, "func a() {}")
	eend := estart + len("func a() {}")

	delContent := "package del\n\nfunc D() {}\n"
	delPath := writeFile(t, dir, "del.go", delContent)
	delHash := hashing.RawHash(h, []byte(delContent))

	// Create into a read-only directory so the O_EXCL open fails at APPLY time
	// (the directory exists at VALIDATE time so non-existence passes).
	roDir := filepath.Join(dir, "ro")
	if err := os.MkdirAll(roDir, 0o755); err != nil {
		t.Fatalf("mkdir ro: %v", err)
	}
	createPath := filepath.Join(roDir, "new.go")
	if err := os.Chmod(roDir, 0o555); err != nil {
		t.Fatalf("chmod ro: %v", err)
	}
	t.Cleanup(func() { _ = os.Chmod(roDir, 0o755) })

	// Order so edit + delete apply BEFORE the failing create.
	ops := []Op{
		editOp(editPath, editSrc, estart, eend, "func a() { return }"),
		{Kind: OpDelete, Path: delPath, ExpectedRawHash: delHash},
		{Kind: OpCreate, Path: createPath, Content: "package fresh\n"},
	}

	_, err := s.ApplyBatch(context.Background(), ops)
	if err == nil {
		t.Fatalf("ApplyBatch with a failing apply = nil error, want rollback + error")
	}

	// Rollback restored the edit and the delete to their pre-batch bytes.
	if got := readFile(t, editPath); got != editSrc {
		t.Fatalf("rollback did not restore the edited file: %q, want %q", got, editSrc)
	}
	if got := readFile(t, delPath); got != delContent {
		t.Fatalf("rollback did not restore the deleted file: %q, want %q", got, delContent)
	}
}

// TestApplyBatchRejectsEmpty verifies an empty batch is a no-op reject (nothing to
// apply), distinct from a successful zero-effect run, so a caller never silently
// commits an empty intent.
func TestApplyBatchRejectsEmpty(t *testing.T) {
	s := createSession(t)
	_, err := s.ApplyBatch(context.Background(), nil)
	if err == nil {
		t.Fatalf("ApplyBatch(nil) = nil error, want an empty-batch reject")
	}
}

// TestApplyBatchRejectsEditWithoutEdit verifies an OpEdit whose Edit field is nil
// is a hard reject (a malformed op), so a batch never silently skips an edit.
func TestApplyBatchRejectsEditWithoutEdit(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)
	path := writeFile(t, dir, "x.go", goSrc)
	_, err := s.ApplyBatch(context.Background(), []Op{{Kind: OpEdit, Path: path}})
	if err == nil {
		t.Fatalf("OpEdit with nil Edit = nil error, want a malformed-op reject")
	}
}

// TestRecoverConvergesCrashedBatch proves a single unified intent carrying an edit
// (Originals+Edits), a create (Creates), a delete (Deletes+Originals), and a move
// (Moves+Originals) converges the WHOLE batch on Recover with no batch-specific
// Recover change. It seeds the on-disk crash state a batch leaves after the durable
// intent but before the WAL clear, then asserts Recover converges every op:
// edited/deleted files restored, created file unlinked, move fully converged.
func TestRecoverConvergesCrashedBatch(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")

	// Edit op crash state: file holds the NEW (edited) bytes; Originals restore it.
	editOrig := goSrc
	editPath := writeFile(t, dir, "edit.go", "package main\n\nfunc a() { return }\n")

	// Create op crash state: the created file is on disk; Recover unlinks it.
	createPath := writeFile(t, dir, "new.go", "package fresh\n")

	// Delete op crash state: the file is already gone; Originals restore it.
	delPath := filepath.Join(dir, "del.go")
	delOrig := "package del\n\nfunc D() {}\n"

	// Move op crash state: dest written, source still present; converge to moved.
	moveFrom := writeFile(t, dir, "src.go", "package mv\n")
	moveTo := writeFile(t, dir, "dst.go", "package mv\n")
	moveOrig := "package mv\n"

	intent := wal.Intent{
		ID:        "crash-batch",
		Edits:     nil,
		Creates:   []string{createPath},
		Deletes:   []string{delPath},
		Moves:     []wal.Move{{From: moveFrom, To: moveTo}},
		Originals: map[string][]byte{editPath: []byte(editOrig), delPath: []byte(delOrig), moveFrom: []byte(moveOrig)},
	}
	if err := wal.Append(walDir, intent); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}

	if got := readFile(t, editPath); got != editOrig {
		t.Fatalf("Recover did not restore the edited file: %q, want %q", got, editOrig)
	}
	if _, statErr := os.Stat(createPath); !os.IsNotExist(statErr) {
		t.Fatalf("Recover did not unlink the created file: %v", statErr)
	}
	if got := readFile(t, delPath); got != delOrig {
		t.Fatalf("Recover did not restore the deleted file: %q, want %q", got, delOrig)
	}
	if _, statErr := os.Stat(moveFrom); !os.IsNotExist(statErr) {
		t.Fatalf("Recover did not remove the move source: %v", statErr)
	}
	if got := readFile(t, moveTo); got != moveOrig {
		t.Fatalf("Recover did not converge the move dest: %q, want %q", got, moveOrig)
	}
	if intents, _ := wal.Replay(walDir); len(intents) != 0 {
		t.Fatalf("Recover left WAL records: %#v", intents)
	}
}

// TestApplyBatchRejectsDuplicateEditPaths proves blocker 1: two OpEdits targeting
// the SAME file in one batch is a HARD REJECT, not a silent clobber. Each edit
// would resolve against the pristine pre-batch bytes and the second whole-file
// write would drop the first; the contract requires a heterogeneous batch touch
// each path at most once, so the batch must reject and leave the file untouched.
func TestApplyBatchRejectsDuplicateEditPaths(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)

	src := goSrc
	path := writeFile(t, dir, "edit.go", src)
	astart := strings.Index(src, "func a() {}")
	aend := astart + len("func a() {}")
	bstart := strings.Index(src, "func b() {}")
	bend := bstart + len("func b() {}")

	ops := []Op{
		editOp(path, src, astart, aend, "func a() { return }"),
		editOp(path, src, bstart, bend, "func b() { return }"),
	}

	_, err := s.ApplyBatch(context.Background(), ops)
	if err == nil {
		t.Fatalf("ApplyBatch with two edits on the same file = nil error, want a duplicate-path reject")
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("rejected duplicate-path batch altered the file: %q, want %q", got, src)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("rejected duplicate-path batch left %d WAL intents, want 0", len(intents))
	}
}

// TestApplyBatchRejectsDeleteThenEditSamePath proves blocker 2: a destructive op
// and a write on the SAME path (here delete then edit) is a HARD REJECT. Applying
// both would leave the file on disk with edited content — matching neither the
// DeleteResult nor the EditResult the host was told. A heterogeneous batch must
// touch each path at most once, so the batch rejects and the file is untouched.
func TestApplyBatchRejectsDeleteThenEditSamePath(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)
	h := hashing.XXHasher{}

	src := goSrc
	path := writeFile(t, dir, "main.go", src)
	rawHash := hashing.RawHash(h, []byte(src))
	astart := strings.Index(src, "func a() {}")
	aend := astart + len("func a() {}")

	ops := []Op{
		{Kind: OpDelete, Path: path, ExpectedRawHash: rawHash},
		editOp(path, src, astart, aend, "func a() { return }"),
	}

	_, err := s.ApplyBatch(context.Background(), ops)
	if err == nil {
		t.Fatalf("ApplyBatch mixing delete+edit on one path = nil error, want a duplicate-path reject")
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("rejected delete+edit batch altered the file: %q, want %q", got, src)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("rejected delete+edit batch left %d WAL intents, want 0", len(intents))
	}
}

// TestApplyBatchRejectsMoveDestCollidesCreate proves blocker 2 across the move
// destination: a create and a move whose destination is the SAME path is a HARD
// REJECT (the union of every op's primary path AND every move To must be unique).
func TestApplyBatchRejectsMoveDestCollidesCreate(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)
	h := hashing.XXHasher{}

	moveContent := "package mv\n\nfunc M() {}\n"
	movePath := writeFile(t, dir, "src.go", moveContent)
	moveHash := hashing.RawHash(h, []byte(moveContent))
	collide := filepath.Join(dir, "dst.go")

	ops := []Op{
		{Kind: OpCreate, Path: collide, Content: "package fresh\n"},
		{Kind: OpMove, Path: movePath, To: collide, ExpectedRawHash: moveHash},
	}

	_, err := s.ApplyBatch(context.Background(), ops)
	if err == nil {
		t.Fatalf("ApplyBatch with a create + move-dest collision = nil error, want a duplicate-path reject")
	}
	if _, statErr := os.Stat(collide); !os.IsNotExist(statErr) {
		t.Fatalf("rejected collision batch created the colliding path: %v", statErr)
	}
	if got := readFile(t, movePath); got != moveContent {
		t.Fatalf("rejected collision batch altered the move source: %q", got)
	}
	if intents, _ := wal.Replay(s.WALDir); len(intents) != 0 {
		t.Fatalf("rejected collision batch left %d WAL intents, want 0", len(intents))
	}
}

// TestRecoverConvergesCrashedBatchBackward proves blocker 3: a UNIFIED BATCH intent
// (Batch=true) that mixes a move with an edit converges everything BACKWARD to
// fully-before on Recover, never the half-state where the non-move ops roll back
// but the move completes forward. It seeds a TRULY mid-apply crash state — the
// edit's NEW bytes on disk and the move HALF-DONE (dest written, source still
// present) — then asserts Recover lands fully-before: edit restored, move source
// restored, move dest removed.
func TestRecoverConvergesCrashedBatchBackward(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")

	// Edit op crash state: file holds the NEW (edited) bytes; Originals restore it.
	editOrig := goSrc
	editPath := writeFile(t, dir, "edit.go", "package main\n\nfunc a() { return }\n")

	// Move op MID-APPLY crash state: dest already written, source STILL present
	// (the unlink had not happened). Backward convergence must restore the source
	// and remove the dest.
	moveOrig := "package mv\n\nfunc M() {}\n"
	moveFrom := writeFile(t, dir, "src.go", moveOrig)
	moveTo := writeFile(t, dir, "dst.go", moveOrig)

	intent := wal.Intent{
		ID:        "crash-batch-backward",
		Batch:     true,
		Moves:     []wal.Move{{From: moveFrom, To: moveTo}},
		Originals: map[string][]byte{editPath: []byte(editOrig), moveFrom: []byte(moveOrig)},
	}
	if err := wal.Append(walDir, intent); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}

	if got := readFile(t, editPath); got != editOrig {
		t.Fatalf("Recover did not restore the edited file: %q, want %q", got, editOrig)
	}
	if got := readFile(t, moveFrom); got != moveOrig {
		t.Fatalf("Recover did not restore the move source (backward): %q, want %q", got, moveOrig)
	}
	if _, statErr := os.Stat(moveTo); !os.IsNotExist(statErr) {
		t.Fatalf("Recover did not remove the move dest (backward): %v", statErr)
	}
	if intents, _ := wal.Replay(walDir); len(intents) != 0 {
		t.Fatalf("Recover left WAL records: %#v", intents)
	}
}

// TestRecoverConvergesSingleMoveForward guards the additive contract: a single-op
// move intent (Batch=false, the shape MoveFile writes) still converges FORWARD to
// fully-moved, so the batch backward-convergence change does NOT regress MoveFile's
// own recovery semantics.
func TestRecoverConvergesSingleMoveForward(t *testing.T) {
	dir := t.TempDir()
	walDir := filepath.Join(dir, "wal")

	moveOrig := "package mv\n"
	moveFrom := writeFile(t, dir, "src.go", moveOrig)
	moveTo := writeFile(t, dir, "dst.go", moveOrig)

	intent := wal.Intent{
		ID:        "single-move",
		Moves:     []wal.Move{{From: moveFrom, To: moveTo}},
		Originals: map[string][]byte{moveFrom: []byte(moveOrig)},
	}
	if err := wal.Append(walDir, intent); err != nil {
		t.Fatalf("Append: %v", err)
	}

	s := createSession(t)
	if err := s.Recover(context.Background(), walDir); err != nil {
		t.Fatalf("Recover: %v", err)
	}

	if _, statErr := os.Stat(moveFrom); !os.IsNotExist(statErr) {
		t.Fatalf("single-op move Recover did not remove the source (forward): %v", statErr)
	}
	if got := readFile(t, moveTo); got != moveOrig {
		t.Fatalf("single-op move Recover did not converge the dest (forward): %q, want %q", got, moveOrig)
	}
}

// TestApplyBatchResultKindsMatchOps verifies the per-op results carry the op kind
// in input order so a host can map each result back to its op.
func TestApplyBatchResultKindsMatchOps(t *testing.T) {
	dir := t.TempDir()
	s := createSession(t)
	createPath := filepath.Join(dir, "a.go")
	ops := []Op{{Kind: OpCreate, Path: createPath, Content: "package a\n"}}
	results, err := s.ApplyBatch(context.Background(), ops)
	if err != nil {
		t.Fatalf("ApplyBatch: %v", err)
	}
	if len(results) != 1 || results[0].Kind != OpCreate {
		t.Fatalf("result kinds = %#v, want one OpCreate", results)
	}
	if results[0].Create.Path != createPath {
		t.Fatalf("create result path = %q, want %q", results[0].Create.Path, createPath)
	}
}
