// File delete.go adds the FILE-LIFECYCLE delete leg to the session engine
// (ADR-0004, SPEC §10). Delete is the DESTRUCTIVE op, so safety is paramount: it
// is WAL-logged + reject-never-corrupt on the same engine philosophy as edits
// and create. Its anchor is the expected raw_hash drift gate — the live file
// must still hash to op.ExpectedRawHash or the delete HARD-REJECTS, never
// discarding bytes the caller did not see (mirroring the FileAnchor drift gate
// the edit path uses). Its undo is a content RESTORE: the FULL prior bytes are
// captured in a durable WAL record BEFORE the unlink, so a crash, rollback, or
// Recover can restore the deleted file. This WAL-original-bytes-FIRST,
// unlink-LAST ordering is the inverse of create's O_EXCL-before-WAL ordering:
// for delete the recoverable window is the one between the durable undo record
// and the destructive unlink. It is ADDITIVE — the existing edit and create
// paths are unchanged.
package session

import (
	"context"
	"errors"
	"fmt"
	"os"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/wal"
)

// ErrNotFound is the sentinel returned when DeleteFile is asked to delete a path
// that does not exist: there is nothing to delete, distinct from a drift reject.
// Callers can match it with errors.Is(err, ErrNotFound).
var ErrNotFound = errors.New("session: target does not exist")

// DeleteResult is the small success signal a host (Hylla) reads after a delete
// to close that file's node versions: the deleted path and the confirmed
// raw_hash the live bytes matched at unlink time.
type DeleteResult struct {
	// Path is the file that was deleted.
	Path string
	// RawHash is the raw content hash the live bytes matched (the satisfied
	// drift anchor), confirming WHICH content was removed.
	RawHash string
}

// DeleteFile deletes op.Path, returning a DeleteResult a host can ingest to
// close that file's node versions. It rejects unless op.Kind is OpDelete.
//
// Under the target's per-file lock it: (1) reads the live bytes — a missing path
// HARD-REJECTS with ErrNotFound (nothing to delete) and NOTHING is unlinked;
// (2) gates the delete on the raw_hash drift anchor — if the live bytes no
// longer hash to op.ExpectedRawHash a concurrent change the caller did not see
// occurred, so the delete HARD-REJECTS with a *ConflictError and NOTHING is
// unlinked (Båge never discards bytes the caller did not see); (3) durably
// WAL-logs the delete with the FULL prior bytes captured in Originals BEFORE any
// destructive effect, so a crash between the record and the unlink is
// recoverable (Recover restores the bytes); (4) only now unlinks the file;
// (5) clears the WAL. The WAL-with-original-bytes-FIRST, unlink-LAST ordering is
// the inverse of create's O_EXCL-before-WAL ordering: a crash in the recoverable
// window leaves a durable record whose Originals restore the deleted file. On
// any reject NOTHING is unlinked.
func (s *Session) DeleteFile(_ context.Context, op Op) (DeleteResult, error) {
	if op.Kind != OpDelete {
		return DeleteResult{}, fmt.Errorf("session: DeleteFile requires a delete op, got %s", op.Kind)
	}

	mu := s.fileLock(op.Path)
	mu.Lock()
	defer mu.Unlock()

	// Read the live bytes FIRST: a missing path is a not-found reject (nothing to
	// delete), distinct from a drift reject, and proves nothing is unlinked on the
	// missing path.
	live, err := os.ReadFile(op.Path)
	if err != nil {
		if os.IsNotExist(err) {
			return DeleteResult{}, fmt.Errorf("session: delete %q: %w", op.Path, ErrNotFound)
		}
		return DeleteResult{}, fmt.Errorf("session: delete read %q: %w", op.Path, err)
	}

	// Drift gate: the live file must still hash to the caller's expected raw_hash.
	// A mismatch means the file changed since the caller observed it, so the
	// delete HARD-REJECTS and never discards bytes the caller did not see.
	liveRaw := hashing.RawHash(s.Hasher, live)
	if liveRaw != op.ExpectedRawHash {
		return DeleteResult{}, &ConflictError{Path: op.Path, Reason: "raw_hash drift", kind: KindDrift}
	}

	// Capture the FULL prior bytes in a durable WAL record BEFORE the unlink, so a
	// crash in the window between this record and the unlink is recoverable: the
	// delete's undo is a content RESTORE from Originals (the inverse of create's
	// unlink-on-recover). The record must precede the destructive step.
	intent := wal.Intent{
		ID:        newIntentID(),
		Deletes:   []string{op.Path},
		Originals: map[string][]byte{op.Path: live},
	}
	if err := wal.Append(s.WALDir, intent); err != nil {
		return DeleteResult{}, fmt.Errorf("session: delete append WAL: %w", err)
	}

	// The undo record is durable; now perform the destructive unlink. If it fails,
	// the WAL record is preserved as the recovery backstop (Recover restores the
	// still-present bytes), so we do NOT clear it.
	if err := os.Remove(op.Path); err != nil {
		return DeleteResult{}, fmt.Errorf("session: delete unlink %q: %w", op.Path, err)
	}

	if err := clearWAL(s.WALDir); err != nil {
		return DeleteResult{}, fmt.Errorf("session: delete clear WAL: %w", err)
	}

	return DeleteResult{Path: op.Path, RawHash: liveRaw}, nil
}
