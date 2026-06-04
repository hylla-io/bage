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
