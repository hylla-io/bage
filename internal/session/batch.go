// File batch.go adds the INTEGRATION NODE of the op spine (ADR-0004 §10.1/§10.3,
// SPEC §10): Session.ApplyBatch applies a HETEROGENEOUS []Op (Edit + Create +
// Delete + Move) as ONE all-or-nothing logical change a host (Hylla) maps to ONE
// graph mutation. This is the whole reason the tagged Op shape exists.
//
// ALL-OR-NOTHING is the hard guarantee: if ANY op fails its anchor
// (region_hash/raw_hash drift, clobber, parse/lint floor) OR fails to apply, the
// ENTIRE batch is rejected and the filesystem is left EXACTLY as before — no op
// half-applied. "Atomic" here is WAL-backed all-or-nothing ON RECOVERY (there is no
// multi-file syscall): a crash mid-commit converges via Recover to fully-before or
// fully-after, never half.
//
// It REUSES the existing per-op machinery rather than reimplementing anchors:
//   - edits go through resolveEdits (region_hash gate) + the format/lint/parse floor
//     (formatLintParse) + edit.SpliceEdits, exactly like Commit;
//   - creates use the non-existence anchor + the same floor over the staged bytes;
//   - deletes/moves use the raw_hash drift gate and capture Originals;
//   - the union of all op paths (+ move dests) is locked in the deadlock-free SORTED
//     order via lockPaths, held across BOTH phases.
//
// The flow is four phases under the union lock: (A) VALIDATE every op's anchor up
// front, reading nothing-destructive and writing nothing — any failure rejects the
// whole batch having written NOTHING; (B) build ONE unified wal.Intent capturing
// every op's undo (edit Originals + resolved Edits, Creates, Deletes + Originals,
// Moves + Originals) and Append it durably; (C) APPLY every op — on any apply
// failure ROLL BACK every already-applied op from that unified intent and return the
// error, leaving the WAL as the Recover backstop; (D) clearWAL on full success.
// Because Recover already converges a unified intent (Originals restore, Creates
// unlink, Moves converge), a crashed batch needs NO batch-specific Recover change.
//
// It is LIBRARY-ONLY: the host drives the batch, so there is no CLI verb (a batch is
// not ergonomic as flags). It is ADDITIVE — the existing []region.Edit
// Prepare/Commit and the single-op CreateFile/DeleteFile/MoveFile paths are
// unchanged.
package session

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hylla-io/bage/internal/atomicwrite"
	"github.com/hylla-io/bage/internal/edit"
	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/locator"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/wal"
)

// BatchResult is the per-op outcome a host (Hylla) reads after a successful
// ApplyBatch, in input order, so it can map each op to its graph effect. Exactly
// one of the inner result fields is populated, selected by Kind: Edit for the edits
// applied to one file, Create for a created (or move-destination) whole-file result,
// Delete for a removed file, Move for a relocation.
type BatchResult struct {
	// Kind tags which op this result describes (OpCreate/OpDelete/OpMove/OpEdit).
	Kind OpKind
	// Edit holds the per-edit results for an OpEdit (one EditResult per region edit
	// in the op). Nil for other kinds.
	Edit []region.EditResult
	// Create holds the whole-file result for an OpCreate. Zero value for other kinds.
	Create region.EditResult
	// Delete holds the removed-path signal for an OpDelete. Zero value otherwise.
	Delete DeleteResult
	// Move holds the relocation signal for an OpMove. Zero value otherwise.
	Move MoveResult
}

// preparedOp is the validated, side-effect-free plan for one op produced by the
// VALIDATE phase and consumed by the APPLY phase, so apply never re-derives an
// anchor or re-reads a snapshot the validate phase already proved.
type preparedOp struct {
	op Op
	// live is the source bytes read during validate (edit/delete/move source).
	live []byte
	// resolved are the byte-range edits an OpEdit will splice into live.
	resolved []locator.FileEdit
	// spliced is the post-splice bytes an OpEdit writes.
	spliced []byte
}

// ApplyBatch applies a heterogeneous []Op as ONE all-or-nothing change (ADR-0004
// §10.1). It locks the UNION of every op's path (plus each move's destination) in
// the deadlock-free SORTED order, then runs four phases under that lock: VALIDATE
// every anchor (writing nothing), append ONE unified wal.Intent capturing every
// op's undo, APPLY every op (rolling back from that intent on any apply failure),
// and clear the WAL on full success. On any reject NOTHING is half-applied and the
// filesystem is left exactly as before. It returns one BatchResult per op in input
// order. An empty batch, an unknown op kind, or an OpEdit with a nil Edit is a hard
// reject.
func (s *Session) ApplyBatch(ctx context.Context, ops []Op) ([]BatchResult, error) {
	if len(ops) == 0 {
		return nil, fmt.Errorf("session: ApplyBatch requires at least one op")
	}

	unlock := s.lockPaths(batchPaths(ops)...)
	defer unlock()

	// Phase A — VALIDATE every op's anchor up front, writing nothing. Any failure
	// rejects the whole batch before a single durable record exists.
	prepared, intent, err := s.validateBatch(ctx, ops)
	if err != nil {
		return nil, err
	}

	// Phase B — append ONE unified intent so a crash anywhere in APPLY converges via
	// Recover (Originals restore, Creates unlink, Moves converge).
	if err := wal.Append(s.WALDir, intent); err != nil {
		return nil, fmt.Errorf("session: batch append WAL: %w", err)
	}

	// Phase C — APPLY every op. On any apply failure, roll back every already-applied
	// op from the unified intent and return the error (the WAL is preserved as the
	// Recover backstop, so it is NOT cleared).
	results, err := s.applyBatch(prepared)
	if err != nil {
		s.rollbackBatch(intent)
		return nil, err
	}

	// Phase D — full success: clear the WAL.
	if err := clearWAL(s.WALDir); err != nil {
		return nil, fmt.Errorf("session: batch clear WAL: %w", err)
	}
	return results, nil
}

// validateBatch runs the VALIDATE phase: it checks every op's anchor against the
// live filesystem WITHOUT writing anything and builds both the per-op apply plans
// and the single unified wal.Intent capturing every op's undo. Any anchor failure
// (region_hash/raw_hash drift, clobber, parse/lint floor, missing source) returns an
// error so the whole batch rejects before any durable record exists.
func (s *Session) validateBatch(ctx context.Context, ops []Op) ([]preparedOp, wal.Intent, error) {
	// HARD REJECT overlapping op paths up front (ADR-0004 §10.1): a heterogeneous
	// batch must touch each path AT MOST ONCE. If two ops shared a path the apply
	// phase would be undefined — two edits would each resolve against the pristine
	// pre-batch bytes and the second whole-file write would clobber the first, or a
	// destructive op (delete/move-source) would be silently resurrected by a later
	// write on the same path, leaving the filesystem matching NEITHER op's reported
	// result. Rejecting before any durable record keeps the contract: each path is
	// touched once or the whole batch rejects having written nothing.
	if dup, ok := duplicateBatchPath(ops); ok {
		return nil, wal.Intent{}, fmt.Errorf("session: ApplyBatch op path %q appears in more than one op; a batch must touch each path at most once", dup)
	}

	intent := wal.Intent{
		ID:        newIntentID(),
		Batch:     true,
		Originals: make(map[string][]byte),
	}
	prepared := make([]preparedOp, 0, len(ops))

	for _, op := range ops {
		p, err := s.validateOp(ctx, op, &intent)
		if err != nil {
			return nil, wal.Intent{}, err
		}
		prepared = append(prepared, p)
	}
	return prepared, intent, nil
}

// validateOp validates ONE op's anchor and folds its undo into intent, returning
// the side-effect-free apply plan. It dispatches on op.Kind, reusing the existing
// per-op anchor checks (non-existence for create/move-dest, region_hash for edit,
// raw_hash for delete/move-source, plus the format/lint/parse floor).
func (s *Session) validateOp(ctx context.Context, op Op, intent *wal.Intent) (preparedOp, error) {
	switch op.Kind {
	case OpCreate:
		return s.validateCreate(ctx, op, intent)
	case OpDelete:
		return s.validateDelete(op, intent)
	case OpMove:
		return s.validateMove(op, intent)
	case OpEdit:
		return s.validateEdit(ctx, op, intent)
	default:
		return preparedOp{}, fmt.Errorf("session: ApplyBatch unknown op kind %s", op.Kind)
	}
}

// validateCreate checks the non-existence anchor and runs the format/lint/parse
// floor over the staged bytes, then records the create's undo (unlink) in
// intent.Creates. It writes nothing.
func (s *Session) validateCreate(ctx context.Context, op Op, intent *wal.Intent) (preparedOp, error) {
	if _, err := os.Stat(op.Path); err == nil {
		return preparedOp{}, fmt.Errorf("session: create %q: %w", op.Path, ErrExists)
	} else if !os.IsNotExist(err) {
		return preparedOp{}, fmt.Errorf("session: create stat %q: %w", op.Path, err)
	}
	staged := []byte(op.Content)
	if err := s.formatLintParseLang(ctx, op.Path, s.createLang(op), staged); err != nil {
		return preparedOp{}, err
	}
	intent.Creates = append(intent.Creates, op.Path)
	return preparedOp{op: op, spliced: staged}, nil
}

// validateDelete checks the raw_hash drift anchor and captures the full prior bytes
// in intent.Originals + the path in intent.Deletes (the delete's undo is a content
// restore). It writes nothing.
func (s *Session) validateDelete(op Op, intent *wal.Intent) (preparedOp, error) {
	live, err := readForAnchor(op.Path)
	if err != nil {
		return preparedOp{}, err
	}
	if hashing.RawHash(s.Hasher, live) != op.ExpectedRawHash {
		return preparedOp{}, &ConflictError{Path: op.Path, Reason: "raw_hash drift", kind: KindDrift}
	}
	intent.Deletes = append(intent.Deletes, op.Path)
	intent.Originals[op.Path] = live
	return preparedOp{op: op, live: live}, nil
}

// validateMove checks the SOURCE raw_hash drift anchor and the DESTINATION
// non-existence anchor, then captures the move in intent.Moves + the source bytes in
// intent.Originals (the backstop that guarantees the bytes are never lost). It
// writes nothing.
func (s *Session) validateMove(op Op, intent *wal.Intent) (preparedOp, error) {
	if op.To == "" {
		return preparedOp{}, fmt.Errorf("session: move %q requires a destination (op.To)", op.Path)
	}
	live, err := readForAnchor(op.Path)
	if err != nil {
		return preparedOp{}, err
	}
	if hashing.RawHash(s.Hasher, live) != op.ExpectedRawHash {
		return preparedOp{}, &ConflictError{Path: op.Path, Reason: "raw_hash drift", kind: KindDrift}
	}
	if _, statErr := os.Stat(op.To); statErr == nil {
		return preparedOp{}, fmt.Errorf("session: move dest %q: %w", op.To, ErrExists)
	} else if !os.IsNotExist(statErr) {
		return preparedOp{}, fmt.Errorf("session: move dest stat %q: %w", op.To, statErr)
	}
	intent.Moves = append(intent.Moves, wal.Move{From: op.Path, To: op.To})
	intent.Originals[op.Path] = live
	return preparedOp{op: op, live: live}, nil
}

// validateEdit resolves the op's region against the live bytes (the region_hash
// gate, reusing resolveEdits), preview-splices, runs the format/lint/parse floor,
// and captures the file's original bytes + the resolved edits in the intent (the
// edit's undo is a content restore). It writes nothing.
func (s *Session) validateEdit(ctx context.Context, op Op, intent *wal.Intent) (preparedOp, error) {
	if op.Edit == nil {
		return preparedOp{}, fmt.Errorf("session: ApplyBatch OpEdit for %q has a nil Edit", op.Path)
	}
	path := op.Edit.Region.Path
	live, err := readForAnchor(path)
	if err != nil {
		return preparedOp{}, err
	}
	resolved, err := s.resolveEdits(path, live, []region.Edit{*op.Edit})
	if err != nil {
		return preparedOp{}, err
	}
	spliced, err := edit.SpliceEdits(live, resolved)
	if err != nil {
		return preparedOp{}, fmt.Errorf("session: batch splice %q: %w", path, err)
	}
	if err := s.formatLintParse(ctx, path, spliced); err != nil {
		return preparedOp{}, err
	}
	intent.Originals[path] = live
	return preparedOp{op: op, live: live, resolved: resolved, spliced: spliced}, nil
}

// applyBatch runs the APPLY phase: it applies each prepared op in input order,
// returning the per-op BatchResults. On the first apply failure it returns the
// error WITHOUT rolling back (the caller rolls back from the unified intent) so the
// rollback can use the full undo record, not a partial one.
func (s *Session) applyBatch(prepared []preparedOp) ([]BatchResult, error) {
	results := make([]BatchResult, 0, len(prepared))
	for _, p := range prepared {
		res, err := s.applyOp(p)
		if err != nil {
			return nil, err
		}
		results = append(results, res)
	}
	return results, nil
}

// applyOp performs ONE prepared op's on-disk effect (no anchor re-check — VALIDATE
// already proved it under the same held lock) and returns its BatchResult.
func (s *Session) applyOp(p preparedOp) (BatchResult, error) {
	switch p.op.Kind {
	case OpCreate:
		res, err := s.applyCreate(p.op.Path, p.spliced)
		return BatchResult{Kind: OpCreate, Create: res}, err
	case OpDelete:
		if err := os.Remove(p.op.Path); err != nil {
			return BatchResult{}, fmt.Errorf("session: batch delete unlink %q: %w", p.op.Path, err)
		}
		return BatchResult{Kind: OpDelete, Delete: DeleteResult{Path: p.op.Path, RawHash: hashing.RawHash(s.Hasher, p.live)}}, nil
	case OpMove:
		res, err := s.applyMove(p.op.Path, p.op.To, p.live)
		return BatchResult{Kind: OpMove, Move: res}, err
	case OpEdit:
		res, err := s.applyEdit(p.op.Edit.Region.Path, p.live, p.spliced, p.resolved)
		return BatchResult{Kind: OpEdit, Edit: res}, err
	default:
		return BatchResult{}, fmt.Errorf("session: ApplyBatch unknown op kind %s", p.op.Kind)
	}
}

// applyCreate brings the create's file into being with the O_EXCL claim + fsynced
// write (the same primitives as CreateFile) and returns its whole-file result. The
// non-existence anchor was already proven in VALIDATE, but O_EXCL re-proves it under
// the lock so a concurrent create outside the batch cannot be clobbered.
func (s *Session) applyCreate(path string, staged []byte) (region.EditResult, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return region.EditResult{}, fmt.Errorf("session: batch create mkdir for %q: %w", path, err)
	}
	f, err := s.openExclusive(path)
	if err != nil {
		return region.EditResult{}, err
	}
	if err := s.writeAndSync(f, path, staged); err != nil {
		os.Remove(path)
		return region.EditResult{}, err
	}
	return s.createResult(path, staged), nil
}

// applyMove relocates the source bytes to the destination with the O_EXCL claim +
// fsynced write, then unlinks the source. The source bytes are the validated
// snapshot, so the relocate preserves them exactly.
func (s *Session) applyMove(from, to string, live []byte) (MoveResult, error) {
	if err := os.MkdirAll(filepath.Dir(to), 0o755); err != nil {
		return MoveResult{}, fmt.Errorf("session: batch move mkdir for %q: %w", to, err)
	}
	dst, err := s.openExclusive(to)
	if err != nil {
		return MoveResult{}, err
	}
	if err := s.writeAndSync(dst, to, live); err != nil {
		os.Remove(to)
		return MoveResult{}, err
	}
	if err := os.Remove(from); err != nil {
		os.Remove(to)
		return MoveResult{}, fmt.Errorf("session: batch move unlink src %q: %w", from, err)
	}
	return MoveResult{From: from, Dest: s.createResult(to, live)}, nil
}

// applyEdit atomic-writes the pre-spliced bytes for an edit op and computes its
// per-edit EditResults over the new bytes (reusing editResults). The bytes were
// resolved + spliced in VALIDATE under the same held lock, so no re-resolve is
// needed.
func (s *Session) applyEdit(path string, _ []byte, spliced []byte, resolved []locator.FileEdit) ([]region.EditResult, error) {
	if err := atomicwrite.Write(path, spliced); err != nil {
		return nil, fmt.Errorf("session: batch edit write %q: %w", path, err)
	}
	return s.editResults(context.Background(), path, spliced, resolved)
}

// rollbackBatch undoes every op recorded in the unified intent on an APPLY-phase
// failure, returning the filesystem to its pre-batch state. It restores every edited
// and deleted file from Originals, unlinks every created file, and reverses every
// move (restore the source from Originals, unlink the destination). Move sources are
// skipped in the generic Originals restore (their bytes belong to the move reversal,
// not an in-place rewrite). Errors are swallowed because the WAL is preserved as the
// Recover backstop — the caller does NOT clear it after a rollback.
func (s *Session) rollbackBatch(intent wal.Intent) {
	moveSrc := make(map[string]struct{}, len(intent.Moves))
	for _, mv := range intent.Moves {
		moveSrc[mv.From] = struct{}{}
	}

	// Restore edited/deleted originals in place.
	for path, original := range intent.Originals {
		if _, isMoveSrc := moveSrc[path]; isMoveSrc {
			continue
		}
		_ = atomicwrite.Write(path, original)
	}

	// Reverse every move: restore the source, remove the destination.
	for _, mv := range intent.Moves {
		if original, ok := intent.Originals[mv.From]; ok {
			_ = atomicwrite.Write(mv.From, original)
		}
		_ = os.Remove(mv.To)
	}

	// Unlink every created file.
	for _, path := range intent.Creates {
		_ = os.Remove(path)
	}
}

// batchPaths returns every path the batch touches — each op's Path plus each move's
// destination — so lockPaths can take the deduplicated, sorted union of locks. This
// is the deadlock-free invariant for a batch: all writers to any file in the batch
// serialize on a single globally-ordered lock set.
func batchPaths(ops []Op) []string {
	paths := make([]string, 0, len(ops))
	for _, op := range ops {
		paths = append(paths, batchOpPath(op))
		if op.Kind == OpMove && op.To != "" {
			paths = append(paths, op.To)
		}
	}
	return paths
}

// duplicateBatchPath reports the FIRST path that appears in more than one op
// across the batch, treating each op's primary path (batchOpPath) AND each move's
// destination (op.To) as a touched path. This is the uniqueness gate behind the
// hard reject in validateBatch: it catches two edits on one file, a delete+edit on
// one path, a move source colliding with another op's path, a move destination
// colliding with a create, and every other overlap, so a heterogeneous batch
// touches each path at most once (ADR-0004 §10.1). It returns ("", false) when
// every touched path is unique.
func duplicateBatchPath(ops []Op) (string, bool) {
	seen := make(map[string]struct{}, len(ops)*2)
	for _, op := range ops {
		if dup, ok := markBatchPath(seen, batchOpPath(op)); ok {
			return dup, true
		}
		if op.Kind == OpMove && op.To != "" {
			if dup, ok := markBatchPath(seen, op.To); ok {
				return dup, true
			}
		}
	}
	return "", false
}

// markBatchPath records path in seen, returning (path, true) if it was already
// present (a duplicate touch) so duplicateBatchPath can short-circuit on the first
// collision. An empty path is ignored (an OpEdit with a nil Edit or an empty
// op.Path is rejected later by the per-op validator with a clearer message).
func markBatchPath(seen map[string]struct{}, path string) (string, bool) {
	if path == "" {
		return "", false
	}
	if _, ok := seen[path]; ok {
		return path, true
	}
	seen[path] = struct{}{}
	return "", false
}

// batchOpPath returns the primary path an op locks: an OpEdit keys off its region's
// path (which may differ from op.Path if a caller left op.Path empty), every other
// kind off op.Path.
func batchOpPath(op Op) string {
	if op.Kind == OpEdit && op.Edit != nil {
		return op.Edit.Region.Path
	}
	return op.Path
}

// readForAnchor reads a file whose live bytes an anchor will gate, mapping a missing
// path to ErrNotFound (nothing to delete/move/edit), distinct from a drift reject.
func readForAnchor(path string) ([]byte, error) {
	live, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, fmt.Errorf("session: %q: %w", path, ErrNotFound)
		}
		return nil, fmt.Errorf("session: read %q: %w", path, err)
	}
	return live, nil
}
