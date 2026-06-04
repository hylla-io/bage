// Package wal is a durable, file-based write-ahead log of edit intents. Each
// Append fsyncs one JSON-encoded record (one line) to <dir>/wal.log; Replay
// reads every record back in order. There is no SQLite or external store: the
// log is a plain append-only file so a crash mid-edit can be reconciled on
// restart by replaying the recorded intents.
package wal

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hylla-io/bage/internal/locator"
)

// logName is the fixed file name of the WAL within its directory.
const logName = "wal.log"

// Intent is one durably-recorded edit intent. It captures the edits to apply,
// the original bytes of each affected file (for restore-on-failure), and the
// expected raw and normalized content hashes per file (for drift detection)
// so a recovering process has everything needed to reapply or roll back.
//
// The Creates field is an ADDITIVE extension for file-lifecycle create ops
// (ADR-0004): it records the paths a create intent is bringing into existence
// so a crash or rollback can UNLINK each half-created file (create's undo is
// unlink, not a content restore). It is the zero value (nil) for edit-only
// intents, and because the field carries `omitempty` an old edit-only record
// written before Creates existed still unmarshals cleanly (the absent key
// decodes to nil), so Replay over a mixed log keeps working.
type Intent struct {
	// ID uniquely identifies this intent.
	ID string `json:"id"`
	// Edits are the byte-range replacements this intent will apply.
	Edits []locator.FileEdit `json:"edits"`
	// Originals maps each affected file path to its pre-edit raw bytes.
	Originals map[string][]byte `json:"originals"`
	// ExpectedRawHash maps each file path to its expected raw content hash.
	ExpectedRawHash map[string]string `json:"expected_raw_hash"`
	// ExpectedNormHash maps each file path to its expected normalized hash.
	ExpectedNormHash map[string]string `json:"expected_norm_hash"`
	// Creates lists the paths this intent is creating from non-existence. On
	// crash recovery or rollback each path is unlinked, undoing a half-created
	// file. Nil for edit-only intents (ADR-0004).
	Creates []string `json:"creates,omitempty"`
	// Deletes lists the paths this intent is removing. It is the inverse of
	// Creates: a delete's undo is a content RESTORE, so each deleted path's FULL
	// prior bytes are captured in Originals before the unlink, and a crash or
	// rollback restores them from there on recovery (ADR-0004). Nil for non-delete
	// intents; the `omitempty` tag keeps older records (written before Deletes
	// existed) decoding cleanly so Replay over a mixed log keeps working.
	Deletes []string `json:"deletes,omitempty"`
	// Moves lists the {From,To} relocations this intent is performing. A move is a
	// DELETE(From)+CREATE(To) as one atomic-on-recovery unit (ADR-0004): the source
	// bytes are captured in Originals[From] BEFORE the destination is claimed and
	// the source unlinked, so a crash converges to fully-moved or fully-original
	// and the source bytes are never lost. Nil for non-move intents; the
	// `omitempty` tag keeps older records (written before Moves existed) decoding
	// cleanly so Replay over a mixed log keeps working.
	Moves []Move `json:"moves,omitempty"`
}

// Move is one source->destination relocation recorded in an Intent. From is the
// source path being removed; To is the destination path being created with the
// source's bytes (captured in Intent.Originals[From]). Recover uses the pair to
// converge a crashed move (ADR-0004).
type Move struct {
	// From is the source path the move removes.
	From string `json:"from"`
	// To is the destination path the move creates with the source bytes.
	To string `json:"to"`
}

// Append durably records one intent. It creates dir if needed, then opens
// <dir>/wal.log in append mode, writes the JSON encoding of in as a single
// line, and fsyncs the file before returning so the record survives a crash.
func Append(dir string, in Intent) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("wal: create dir %q: %w", dir, err)
	}

	line, err := json.Marshal(in)
	if err != nil {
		return fmt.Errorf("wal: marshal intent %q: %w", in.ID, err)
	}

	path := filepath.Join(dir, logName)
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return fmt.Errorf("wal: open %q: %w", path, err)
	}
	defer f.Close()

	line = append(line, '\n')
	if _, err := f.Write(line); err != nil {
		return fmt.Errorf("wal: write %q: %w", path, err)
	}
	if err := f.Sync(); err != nil {
		return fmt.Errorf("wal: fsync %q: %w", path, err)
	}

	// Fsync the PARENT DIRECTORY too. Syncing the file content alone does not
	// make the directory entry durable: when wal.log was just created, a
	// power-loss crash could lose the directory entry and thus the whole record,
	// even though f.Sync returned — leaving a caller that already acted on
	// Append's success (e.g. delete, which unlinks the target right after) with
	// UNRECOVERABLE bytes. Syncing the directory closes that window, so the
	// WAL-before-unlink ordering is a true crash guarantee for delete, and the
	// same hardening strengthens the create and edit paths that share Append.
	if err := syncDir(dir); err != nil {
		return fmt.Errorf("wal: fsync dir %q: %w", dir, err)
	}
	return nil
}

// syncDir fsyncs the directory at dir so a newly-created or renamed entry within
// it (here wal.log) becomes durable, not just the file's content. It opens the
// directory read-only, fsyncs it, and closes it, wrapping any failure with %w so
// the caller never silently treats an undurable directory as committed.
func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return fmt.Errorf("wal: open dir %q: %w", dir, err)
	}
	if err := d.Sync(); err != nil {
		d.Close()
		return fmt.Errorf("wal: sync dir %q: %w", dir, err)
	}
	if err := d.Close(); err != nil {
		return fmt.Errorf("wal: close dir %q: %w", dir, err)
	}
	return nil
}

// Replay reads every recorded intent from <dir>/wal.log in append order. A
// missing directory or log file is treated as an empty log: it returns an
// empty slice and no error.
func Replay(dir string) ([]Intent, error) {
	path := filepath.Join(dir, logName)
	f, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return []Intent{}, nil
		}
		return nil, fmt.Errorf("wal: open %q: %w", path, err)
	}
	defer f.Close()

	out := []Intent{}
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
	for sc.Scan() {
		line := sc.Bytes()
		if len(line) == 0 {
			continue
		}
		var in Intent
		if err := json.Unmarshal(line, &in); err != nil {
			// A record that fails to decode is treated as a torn trailing
			// write: the process crashed mid-Append before this record was
			// fully written/fsynced. Stop and return every cleanly-committed
			// record before it, so crash recovery preserves all good intents
			// (SPEC §1.2). Append writes one fsynced line at a time, so only
			// the final record can be torn.
			break
		}
		out = append(out, in)
	}
	if err := sc.Err(); err != nil {
		return nil, fmt.Errorf("wal: scan %q: %w", path, err)
	}
	return out, nil
}
