package locator

import (
	"os"
	"path/filepath"
	"testing"
)

// writeTemp creates a file with the given contents inside t's temp dir and
// returns its path.
func writeTemp(t *testing.T, name, contents string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
		t.Fatalf("write temp %q: %v", path, err)
	}
	return path
}

// readFile reads path or fails the test.
func readFile(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %q: %v", path, err)
	}
	return string(b)
}

func TestTextLocatorApply(t *testing.T) {
	tests := []struct {
		name    string
		initial string
		start   int
		end     int
		newText string
		want    string
		wantErr bool
	}{
		{name: "middle splice", initial: "hello world", start: 6, end: 11, newText: "gophers", want: "hello gophers"},
		{name: "insert at start (empty range)", initial: "world", start: 0, end: 0, newText: "hello ", want: "hello world"},
		{name: "append at end (empty range)", initial: "hello", start: 5, end: 5, newText: " world", want: "hello world"},
		{name: "full replace", initial: "old", start: 0, end: 3, newText: "new", want: "new"},
		{name: "delete range", initial: "abcdef", start: 2, end: 4, newText: "", want: "abef"},
		{name: "end past length", initial: "abc", start: 0, end: 4, newText: "x", wantErr: true},
		{name: "negative start", initial: "abc", start: -1, end: 2, newText: "x", wantErr: true},
		{name: "inverted range", initial: "abc", start: 2, end: 1, newText: "x", wantErr: true},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			path := writeTemp(t, "f.txt", tc.initial)
			loc := &TextLocator{Path_: path, StartByte: tc.start, EndByte: tc.end}

			if loc.Path() != path {
				t.Fatalf("Path() = %q, want %q", loc.Path(), path)
			}

			err := loc.Apply(Edit{NewText: tc.newText})
			if tc.wantErr {
				if err == nil {
					t.Fatalf("Apply: expected error, got nil")
				}
				// File must be untouched on a guarded error.
				if got := readFile(t, path); got != tc.initial {
					t.Fatalf("file mutated on error: got %q, want %q", got, tc.initial)
				}
				return
			}
			if err != nil {
				t.Fatalf("Apply: unexpected error: %v", err)
			}
			if got := readFile(t, path); got != tc.want {
				t.Fatalf("after Apply: got %q, want %q", got, tc.want)
			}
		})
	}
}

func TestTextLocatorRawHashChangesAfterApply(t *testing.T) {
	path := writeTemp(t, "f.txt", "hello world")
	loc := &TextLocator{Path_: path, StartByte: 6, EndByte: 11}

	before := loc.RawHash()
	if before == "" {
		t.Fatalf("RawHash before apply is empty")
	}

	if err := loc.Apply(Edit{NewText: "gophers"}); err != nil {
		t.Fatalf("Apply: %v", err)
	}

	after := loc.RawHash()
	if after == "" {
		t.Fatalf("RawHash after apply is empty")
	}
	if before == after {
		t.Fatalf("RawHash unchanged after content change: %q", before)
	}

	// Stable: identical content hashes equally.
	other := writeTemp(t, "g.txt", "hello gophers")
	otherLoc := &TextLocator{Path_: other}
	if otherLoc.RawHash() != after {
		t.Fatalf("RawHash not stable across identical content: %q vs %q", otherLoc.RawHash(), after)
	}
}

func TestTextLocatorRawHashMissingFile(t *testing.T) {
	loc := &TextLocator{Path_: filepath.Join(t.TempDir(), "nope.txt")}
	if got := loc.RawHash(); got != "" {
		t.Fatalf("RawHash of missing file = %q, want empty", got)
	}
}

func TestApplyFileEditsReverseSortedSingleFile(t *testing.T) {
	// "0123456789" — two edits given in forward order; applying the earlier
	// offset first would shift the later range. Reverse-sort keeps both valid.
	path := writeTemp(t, "f.txt", "0123456789")
	edits := []FileEdit{
		{Path: path, StartByte: 2, EndByte: 4, NewText: "AB"},   // -> replace "23"
		{Path: path, StartByte: 6, EndByte: 8, NewText: "CDEF"}, // -> replace "67" (later offset, longer)
	}
	if err := ApplyFileEdits(edits); err != nil {
		t.Fatalf("ApplyFileEdits: %v", err)
	}
	want := "01AB45CDEF89"
	if got := readFile(t, path); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestApplyFileEditsAcrossTwoFiles(t *testing.T) {
	p1 := writeTemp(t, "a.txt", "alpha")
	p2 := writeTemp(t, "b.txt", "beta")
	edits := []FileEdit{
		{Path: p1, StartByte: 0, EndByte: 5, NewText: "ALPHA"},
		{Path: p2, StartByte: 0, EndByte: 4, NewText: "BETA"},
	}
	if err := ApplyFileEdits(edits); err != nil {
		t.Fatalf("ApplyFileEdits: %v", err)
	}
	if got := readFile(t, p1); got != "ALPHA" {
		t.Fatalf("file 1: got %q, want %q", got, "ALPHA")
	}
	if got := readFile(t, p2); got != "BETA" {
		t.Fatalf("file 2: got %q, want %q", got, "BETA")
	}
}

func TestApplyFileEditsOverlapRejected(t *testing.T) {
	path := writeTemp(t, "f.txt", "0123456789")
	edits := []FileEdit{
		{Path: path, StartByte: 2, EndByte: 6, NewText: "X"},
		{Path: path, StartByte: 4, EndByte: 8, NewText: "Y"}, // overlaps [2:6)
	}
	if err := ApplyFileEdits(edits); err == nil {
		t.Fatalf("expected overlap error, got nil")
	}
	// The whole batch is rejected before any write — file must be untouched.
	if got := readFile(t, path); got != "0123456789" {
		t.Fatalf("file mutated on rejected overlap: got %q", got)
	}
}

func TestApplyFileEditsOutOfRange(t *testing.T) {
	path := writeTemp(t, "f.txt", "abc")
	edits := []FileEdit{{Path: path, StartByte: 0, EndByte: 99, NewText: "x"}}
	if err := ApplyFileEdits(edits); err == nil {
		t.Fatalf("expected out-of-range error, got nil")
	}
}

func TestApplyFileEditsMissingFile(t *testing.T) {
	edits := []FileEdit{{Path: filepath.Join(t.TempDir(), "nope.txt"), StartByte: 0, EndByte: 0, NewText: "x"}}
	if err := ApplyFileEdits(edits); err == nil {
		t.Fatalf("expected read error, got nil")
	}
}

// TestApplyFileEditsAdjacentRanges checks that touching but non-overlapping
// ranges ([2:4) and [4:6)) on one file both apply. Adjacency (group[i+1].End ==
// group[i].Start after the descending sort) must NOT be rejected as overlap —
// the boundary is exclusive/inclusive, so the ranges are disjoint.
func TestApplyFileEditsAdjacentRanges(t *testing.T) {
	path := writeTemp(t, "f.txt", "0123456789")
	edits := []FileEdit{
		{Path: path, StartByte: 2, EndByte: 4, NewText: "AB"}, // replace "23"
		{Path: path, StartByte: 4, EndByte: 6, NewText: "CD"}, // replace "45", touches the prior range
	}
	if err := ApplyFileEdits(edits); err != nil {
		t.Fatalf("ApplyFileEdits on adjacent ranges: %v", err)
	}
	want := "01ABCD6789"
	if got := readFile(t, path); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

// TestApplyFileEditsZeroWidthInserts checks pure insertions (Start==End) at the
// start, middle, and end of a file applied in one batch. Zero-width ranges never
// overlap each other, and reverse-sort by StartByte keeps offsets valid.
func TestApplyFileEditsZeroWidthInserts(t *testing.T) {
	tests := []struct {
		name  string
		edits []FileEdit
		want  string
	}{
		{
			name:  "insert at start",
			edits: []FileEdit{{StartByte: 0, EndByte: 0, NewText: ">>"}},
			want:  ">>abcdef",
		},
		{
			name:  "insert in middle",
			edits: []FileEdit{{StartByte: 3, EndByte: 3, NewText: "--"}},
			want:  "abc--def",
		},
		{
			name:  "insert at end",
			edits: []FileEdit{{StartByte: 6, EndByte: 6, NewText: "<<"}},
			want:  "abcdef<<",
		},
		{
			name: "multiple inserts at start, middle, end",
			edits: []FileEdit{
				{StartByte: 0, EndByte: 0, NewText: "["},
				{StartByte: 3, EndByte: 3, NewText: "|"},
				{StartByte: 6, EndByte: 6, NewText: "]"},
			},
			want: "[abc|def]",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			path := writeTemp(t, "f.txt", "abcdef")
			edits := make([]FileEdit, len(tc.edits))
			for i, e := range tc.edits {
				e.Path = path
				edits[i] = e
			}
			if err := ApplyFileEdits(edits); err != nil {
				t.Fatalf("ApplyFileEdits: %v", err)
			}
			if got := readFile(t, path); got != tc.want {
				t.Fatalf("got %q, want %q", got, tc.want)
			}
		})
	}
}

// TestApplyFileEditsMultiByteUTF8 splices a multi-byte UTF-8 file by byte range.
// "héllo wörld" has 2-byte é (bytes 1-2) and ö (bytes 8-9), so the run "wörld"
// occupies bytes [7:13) of a 13-byte string. Byte-range splicing must respect
// the encoded byte layout, not rune indices.
func TestApplyFileEditsMultiByteUTF8(t *testing.T) {
	const initial = "héllo wörld" // 13 bytes: h é(2) l l o ' ' w ö(2) r l d
	if len(initial) != 13 {
		t.Fatalf("fixture byte length = %d, want 13", len(initial))
	}
	path := writeTemp(t, "u.txt", initial)
	// Replace "wörld" (bytes [7:13)) with an ASCII run; the é before it is left
	// intact, proving the byte range did not split the leading multi-byte rune.
	edits := []FileEdit{{Path: path, StartByte: 7, EndByte: 13, NewText: "gophers"}}
	if err := ApplyFileEdits(edits); err != nil {
		t.Fatalf("ApplyFileEdits: %v", err)
	}
	want := "héllo gophers"
	if got := readFile(t, path); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

// TestRawHashStabilityAndFormat checks RawHash is deterministic for identical
// content, distinct for different content, and always a 16-char lowercase hex
// FNV-1a-64 digest (including for empty files).
func TestRawHashStabilityAndFormat(t *testing.T) {
	isHex16 := func(s string) bool {
		if len(s) != 16 {
			return false
		}
		for _, c := range s {
			if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
				return false
			}
		}
		return true
	}

	pa := writeTemp(t, "a.txt", "hello world")
	pb := writeTemp(t, "b.txt", "hello world")
	pc := writeTemp(t, "c.txt", "hello gophers")
	pe := writeTemp(t, "e.txt", "")

	la := &TextLocator{Path_: pa}
	lb := &TextLocator{Path_: pb}
	lc := &TextLocator{Path_: pc}
	le := &TextLocator{Path_: pe}

	ha := la.RawHash()
	if !isHex16(ha) {
		t.Fatalf("RawHash %q is not 16-char lowercase hex", ha)
	}
	// Deterministic for the same locator across calls.
	if again := la.RawHash(); again != ha {
		t.Fatalf("RawHash not deterministic: %q vs %q", ha, again)
	}
	// Stable across files with identical content.
	if hb := lb.RawHash(); hb != ha {
		t.Fatalf("RawHash differs for identical content: %q vs %q", ha, hb)
	}
	// Distinct for different content.
	if hc := lc.RawHash(); hc == ha {
		t.Fatalf("RawHash collides for different content: %q", hc)
	} else if !isHex16(hc) {
		t.Fatalf("RawHash %q is not 16-char lowercase hex", hc)
	}
	// Empty file still yields a well-formed digest (FNV-1a-64 offset basis).
	he := le.RawHash()
	if !isHex16(he) {
		t.Fatalf("empty-file RawHash %q is not 16-char lowercase hex", he)
	}
}
