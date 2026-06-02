// Package locator addresses byte regions of files and applies splice edits to
// them. A Locator names a file and a byte range; applying an Edit reads the
// live file, replaces the range with new text, and writes the result back
// atomically. RawHash gates byte-offset validity by hashing the live raw bytes.
package locator

import (
	"encoding/hex"
	"fmt"
	"hash/fnv"
	"os"
	"sort"

	"github.com/hylla-io/bage/internal/atomicwrite"
)

// Edit is the replacement text for a single locator's byte range.
type Edit struct {
	// NewText replaces the locator's [StartByte:EndByte] region.
	NewText string
}

// FileEdit is one byte-range replacement within a named file. It is the unit
// the coordinator/LSP path produces; multiple FileEdits per file are applied
// reverse-sorted by StartByte so earlier offsets stay valid.
type FileEdit struct {
	// Path is the absolute or relative path of the file to edit.
	Path string
	// StartByte is the inclusive start of the byte range to replace.
	StartByte int
	// EndByte is the exclusive end of the byte range to replace.
	EndByte int
	// NewText is the text spliced in for [StartByte:EndByte].
	NewText string
}

// Locator addresses an editable byte region of a single file.
type Locator interface {
	// Path returns the file the locator addresses.
	Path() string
	// Apply reads the file, splices e.NewText into the locator's byte range,
	// and writes the result back atomically.
	Apply(e Edit) error
	// RawHash returns the FNV-1a-64 hex digest of the file's live raw bytes,
	// which gates byte-offset validity.
	RawHash() string
}

// TextLocator is a byte-range Locator over a text file.
type TextLocator struct {
	// Path_ is the file the locator addresses.
	Path_ string
	// StartByte is the inclusive start of the addressed byte range.
	StartByte int
	// EndByte is the exclusive end of the addressed byte range.
	EndByte int
}

// Path returns the file the locator addresses.
func (l *TextLocator) Path() string { return l.Path_ }

// Apply reads the file, replaces [StartByte:EndByte] with e.NewText, and writes
// the result back via atomicwrite. It returns an error if the byte range falls
// outside the current file contents.
func (l *TextLocator) Apply(e Edit) error {
	raw, err := os.ReadFile(l.Path_)
	if err != nil {
		return fmt.Errorf("locator: read %q: %w", l.Path_, err)
	}
	out, err := splice(raw, l.StartByte, l.EndByte, e.NewText)
	if err != nil {
		return fmt.Errorf("locator: apply to %q: %w", l.Path_, err)
	}
	if err := atomicwrite.Write(l.Path_, out); err != nil {
		return fmt.Errorf("locator: write %q: %w", l.Path_, err)
	}
	return nil
}

// RawHash returns the FNV-1a-64 hex digest of the file's live raw bytes. A read
// error yields the empty string so callers treat the offsets as invalid.
func (l *TextLocator) RawHash() string {
	raw, err := os.ReadFile(l.Path_)
	if err != nil {
		return ""
	}
	return fnvHex(raw)
}

// ApplyFileEdits groups edits by Path and applies each file's edits in a single
// atomic write. Within a file, edits are applied reverse-sorted by StartByte
// (descending) so that splicing an earlier-offset edit never invalidates the
// byte ranges of later-offset edits not yet applied.
func ApplyFileEdits(edits []FileEdit) error {
	byPath := make(map[string][]FileEdit)
	var order []string
	for _, e := range edits {
		if _, seen := byPath[e.Path]; !seen {
			order = append(order, e.Path)
		}
		byPath[e.Path] = append(byPath[e.Path], e)
	}

	// Pass 1: sort each file's edits (descending by StartByte) and reject any
	// overlap before writing anything, so a rejected batch leaves every file
	// untouched. Reverse-sorted application only stays correct for disjoint
	// ranges (SPEC §4.4); overlapping edits would corrupt silently.
	for _, path := range order {
		group := byPath[path]
		sort.SliceStable(group, func(i, j int) bool {
			return group[i].StartByte > group[j].StartByte
		})
		for i := 0; i+1 < len(group); i++ {
			// group is sorted descending, so group[i+1] has the lower start;
			// they overlap when its EndByte extends past group[i].StartByte.
			if group[i+1].EndByte > group[i].StartByte {
				return fmt.Errorf("locator: overlapping edits in %q: [%d:%d] and [%d:%d]",
					path, group[i+1].StartByte, group[i+1].EndByte, group[i].StartByte, group[i].EndByte)
			}
		}
	}

	// Pass 2: apply each file's (now validated, sorted) edits in one atomic
	// write. Note: this is not transactional across files — a mid-batch read
	// failure leaves earlier files written; cross-file atomicity is the
	// coordinator's responsibility via the WAL.
	for _, path := range order {
		raw, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("locator: read %q: %w", path, err)
		}
		for _, e := range byPath[path] {
			raw, err = splice(raw, e.StartByte, e.EndByte, e.NewText)
			if err != nil {
				return fmt.Errorf("locator: apply to %q: %w", path, err)
			}
		}
		if err := atomicwrite.Write(path, raw); err != nil {
			return fmt.Errorf("locator: write %q: %w", path, err)
		}
	}
	return nil
}

// splice replaces src[start:end] with newText, guarding against out-of-range
// or inverted offsets. It returns a freshly allocated slice and never mutates
// src in place.
func splice(src []byte, start, end int, newText string) ([]byte, error) {
	if start < 0 || end < start || end > len(src) {
		return nil, fmt.Errorf("byte range [%d:%d] out of bounds for length %d", start, end, len(src))
	}
	out := make([]byte, 0, len(src)-(end-start)+len(newText))
	out = append(out, src[:start]...)
	out = append(out, newText...)
	out = append(out, src[end:]...)
	return out, nil
}

// fnvHex returns the FNV-1a-64 digest of b as a lowercase hex string.
func fnvHex(b []byte) string {
	h := fnv.New64a()
	_, _ = h.Write(b)
	return hex.EncodeToString(h.Sum(nil))
}
