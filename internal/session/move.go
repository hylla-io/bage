// File move.go adds the FILE-LIFECYCLE move leg to the session engine (ADR-0004,
// SPEC §10). A move is an anchored-DELETE(source) + anchored-CREATE(destination)
// as ONE atomic-on-recovery unit. The MVP is RELOCATE ONLY: the source bytes are
// preserved at the destination unchanged (no LSP import-fixup in this slice).
//
// It composes the create and delete anchors:
//   - SOURCE anchor = the expected raw_hash drift gate (like delete): the live
//     source must still hash to op.ExpectedRawHash or the move HARD-REJECTS, never
//     relocating bytes the caller did not see. A missing source rejects with
//     ErrNotFound.
//   - DESTINATION anchor = NON-EXISTENCE (like create): the destination must NOT
//     already exist or the move HARD-REJECTS, never clobbering it. This is enforced
//     race-safely with an O_EXCL claim (openExclusive), NOT os.Rename — os.Rename
//     CLOBBERS an existing destination on POSIX, so it cannot be the guard.
//
// Safety priorities, in order: (1) NEVER lose the source bytes; (2) NEVER clobber
// an existing destination; (3) atomic-on-recovery via the WAL; (4) no orphan
// partial destination on crash. Both per-file locks are taken in a DETERMINISTIC
// SORTED order (lockPaths) so a move A->B racing a move B->A can never deadlock.
// It is ADDITIVE — the existing edit, create, and delete paths are unchanged.
package session

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/wal"
)

// MoveResult is the signal a host (Hylla) reads after a move so it can
// re-identify the moved file's nodes: the destination create-result (whole-file
// EditResult over the relocated bytes) plus the source path that was removed.
type MoveResult struct {
	// From is the source path that was removed by the move.
	From string
	// Dest is the whole-file EditResult for the destination (the relocated bytes),
	// shaped like a create result so a host can ingest the destination's nodes.
	Dest region.EditResult
}

// MoveFile relocates op.Path to op.To, preserving the source bytes unchanged, and
// returns a MoveResult a host can ingest. It rejects unless op.Kind is OpMove.
//
// Under BOTH per-file locks (source and destination) acquired in a DETERMINISTIC
// SORTED order so concurrent moves can never deadlock, it: (1) reads the live
// source — a missing source HARD-REJECTS with ErrNotFound and NOTHING moves;
// (2) gates the source on the raw_hash drift anchor — a live mismatch HARD-REJECTS
// with a *ConflictError and NOTHING moves (never relocate bytes the caller did not
// see); (3) mkdir -p's the destination's parent then O_EXCL-creates the
// destination and writes the source bytes + fsync — the O_EXCL claim PROVES the
// destination did not exist (ErrExists if it did, no clobber) and durably lands the
// relocated bytes; (4) durably WAL-logs the move (a Moves record plus
// Originals[src]=bytes) so a crash between here and the unlink converges to
// fully-moved with the source bytes as the backstop; (5) removes the source;
// (6) clears the WAL. On any reject NOTHING moves: the source is intact and no
// destination is left behind.
//
// Ordering note: the destination is claimed+written BEFORE the WAL append (so a
// rejected destination can never log a Moves record that Recover would act on),
// yet the source is NOT unlinked until AFTER the WAL append — so the recoverable
// window (durable Moves record + Originals, destination already written, source
// still present) always has the source bytes safe, satisfying priority (1).
func (s *Session) MoveFile(_ context.Context, op Op) (MoveResult, error) {
	if op.Kind != OpMove {
		return MoveResult{}, fmt.Errorf("session: MoveFile requires a move op, got %s", op.Kind)
	}
	if op.To == "" {
		return MoveResult{}, fmt.Errorf("session: MoveFile requires a destination (op.To)")
	}

	// Lock BOTH paths in sorted order: deadlock-free even when two moves cross
	// (A->B and B->A take {A,B} in the same global order).
	unlock := s.lockPaths(op.Path, op.To)
	defer unlock()

	// Read the live SOURCE first: a missing source is a not-found reject (nothing
	// to move), distinct from a drift reject, and proves nothing moves.
	live, err := os.ReadFile(op.Path)
	if err != nil {
		if os.IsNotExist(err) {
			return MoveResult{}, fmt.Errorf("session: move %q: %w", op.Path, ErrNotFound)
		}
		return MoveResult{}, fmt.Errorf("session: move read %q: %w", op.Path, err)
	}

	// Source drift gate: the live source must still hash to the caller's expected
	// raw_hash. A mismatch means the source changed since the caller observed it,
	// so the move HARD-REJECTS and never relocates bytes the caller did not see.
	liveRaw := hashing.RawHash(s.Hasher, live)
	if liveRaw != op.ExpectedRawHash {
		return MoveResult{}, &ConflictError{Path: op.Path, Reason: "raw_hash drift"}
	}

	if err := os.MkdirAll(filepath.Dir(op.To), 0o755); err != nil {
		return MoveResult{}, fmt.Errorf("session: move mkdir for %q: %w", op.To, err)
	}

	// Claim the DESTINATION with O_CREATE|O_EXCL: this atomically proves the
	// destination did not exist (ErrExists if it did — no clobber, the source stays
	// intact) and brings the destination into being. Doing this BEFORE the WAL
	// append means a rejected destination never logs a Moves record. This is the
	// non-existence anchor.
	dst, err := s.openExclusive(op.To)
	if err != nil {
		return MoveResult{}, err
	}
	if err := s.writeAndSync(dst, op.To, live); err != nil {
		// The destination did not fully land; remove the file we exclusively created
		// so a failed move leaves no orphan destination. The source is untouched.
		os.Remove(op.To)
		return MoveResult{}, err
	}

	// The destination holds the bytes; the source is still present. Durably WAL-log
	// the move with the source bytes in Originals BEFORE the unlink, so a crash in
	// the window between this record and the source unlink converges to fully-moved
	// without ever losing the source content (Originals is the backstop).
	intent := wal.Intent{
		ID:        newIntentID(),
		Moves:     []wal.Move{{From: op.Path, To: op.To}},
		Originals: map[string][]byte{op.Path: live},
	}
	if err := wal.Append(s.WALDir, intent); err != nil {
		// The move is not durably recoverable; back out the orphan destination so a
		// failed move leaves nothing and the source stays intact.
		os.Remove(op.To)
		return MoveResult{}, fmt.Errorf("session: move append WAL: %w", err)
	}

	// The undo/redo record is durable; now remove the source. If it fails, the WAL
	// record is preserved as the recovery backstop (Recover converges to
	// fully-moved), so we do NOT clear it.
	if err := os.Remove(op.Path); err != nil {
		return MoveResult{}, fmt.Errorf("session: move unlink src %q: %w", op.Path, err)
	}

	if err := clearWAL(s.WALDir); err != nil {
		return MoveResult{}, fmt.Errorf("session: move clear WAL: %w", err)
	}

	return MoveResult{From: op.Path, Dest: s.createResult(op.To, live)}, nil
}
