package edit

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/locator"
	"github.com/hylla-io/bage/internal/parser"
)

// fakeParser is a tiny ParserPort that records which method was called and
// returns a stub Tree, so tests can assert the incremental-vs-full dispatch
// without depending on cgo or any real grammar.
type fakeParser struct {
	parseCalls       int
	incrementalCalls int
	lastSrc          []byte
	lastOld          *parser.Tree
	lastEdit         parser.InputEdit
	parseErr         error
	incrementalErr   error
}

func (f *fakeParser) Parse(_ context.Context, _ parser.Lang, src []byte) (*parser.Tree, error) {
	f.parseCalls++
	f.lastSrc = src
	if f.parseErr != nil {
		return nil, f.parseErr
	}
	return &parser.Tree{Root: &parser.Node{Kind: "source_file"}, Source: src}, nil
}

func (f *fakeParser) ParseIncremental(_ context.Context, _ parser.Lang, src []byte, old *parser.Tree, edit parser.InputEdit) (*parser.Tree, error) {
	f.incrementalCalls++
	f.lastSrc = src
	f.lastOld = old
	f.lastEdit = edit
	if f.incrementalErr != nil {
		return nil, f.incrementalErr
	}
	return &parser.Tree{Root: &parser.Node{Kind: "source_file"}, Source: src}, nil
}

func (f *fakeParser) ChangedRanges(_, _ *parser.Tree) []parser.ByteRange { return nil }

func TestCheckDrift(t *testing.T) {
	h := hashing.XXHasher{}
	base := []byte("a := 1 \nb := 2\n")        // trailing space on line 1
	wsVariant := []byte("a := 1\nb := 2\n")    // same after normalize
	realVariant := []byte("a := 99\nb := 2\n") // content differs

	expectedRaw := hashing.RawHash(h, base)
	expectedNorm := hashing.NormHash(h, base)

	tests := []struct {
		name string
		live []byte
		want DriftStatus
	}{
		{"raw match -> valid", base, DriftValid},
		{"whitespace-only -> ws", wsVariant, DriftWhitespaceOnly},
		{"content change -> real", realVariant, DriftReal},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := CheckDrift(h, tt.live, expectedRaw, expectedNorm); got != tt.want {
				t.Fatalf("CheckDrift = %v, want %v", got, tt.want)
			}
		})
	}
}

// TestCheckDriftTrailingNewlineBoundary pins the exact boundary where the live
// bytes differ from the grounded bytes by only a trailing LF. A '\n' is content,
// not horizontal whitespace: normalize.Normalize strips trailing [ \t\r] before
// each LF and at EOF but never inserts or removes a trailing LF. So "foo\n" and
// "foo" disagree on BOTH the raw and the normalized hash, and the bare-LF delta
// must classify as DriftReal — never DriftWhitespaceOnly — so the range is
// re-grounded rather than slid blindly across a line-count change.
func TestCheckDriftTrailingNewlineBoundary(t *testing.T) {
	h := hashing.XXHasher{}
	withNL := []byte("foo\n")
	withoutNL := []byte("foo")

	t.Run("grounded with trailing LF, live drops it -> real", func(t *testing.T) {
		expectedRaw := hashing.RawHash(h, withNL)
		expectedNorm := hashing.NormHash(h, withNL)
		if got := CheckDrift(h, withoutNL, expectedRaw, expectedNorm); got != DriftReal {
			t.Fatalf("CheckDrift(drop trailing LF) = %v, want %v", got, DriftReal)
		}
	})

	t.Run("grounded without trailing LF, live adds it -> real", func(t *testing.T) {
		expectedRaw := hashing.RawHash(h, withoutNL)
		expectedNorm := hashing.NormHash(h, withoutNL)
		if got := CheckDrift(h, withNL, expectedRaw, expectedNorm); got != DriftReal {
			t.Fatalf("CheckDrift(add trailing LF) = %v, want %v", got, DriftReal)
		}
	})

	t.Run("trailing horizontal ws after LF stays whitespace-only", func(t *testing.T) {
		// Sibling control: "foo\n" vs "foo\n   " differ raw but share the
		// normalized form (EOF horizontal ws is stripped), so this is the
		// whitespace-only side of the same boundary.
		expectedRaw := hashing.RawHash(h, withNL)
		expectedNorm := hashing.NormHash(h, withNL)
		if got := CheckDrift(h, []byte("foo\n   "), expectedRaw, expectedNorm); got != DriftWhitespaceOnly {
			t.Fatalf("CheckDrift(trailing ws after LF) = %v, want %v", got, DriftWhitespaceOnly)
		}
	})
}

func TestSpliceEdits(t *testing.T) {
	tests := []struct {
		name    string
		src     string
		edits   []locator.FileEdit
		want    string
		wantErr bool
	}{
		{
			name:  "empty edits returns copy",
			src:   "hello",
			edits: nil,
			want:  "hello",
		},
		{
			name:  "single edit",
			src:   "hello world",
			edits: []locator.FileEdit{{StartByte: 6, EndByte: 11, NewText: "there"}},
			want:  "hello there",
		},
		{
			name: "multi edit reverse-sorted disjoint",
			src:  "abcdefgh",
			edits: []locator.FileEdit{
				{StartByte: 0, EndByte: 2, NewText: "XX"}, // given out of order
				{StartByte: 6, EndByte: 8, NewText: "YY"},
			},
			want: "XXcdefYY",
		},
		{
			name: "overlapping rejected",
			src:  "abcdefgh",
			edits: []locator.FileEdit{
				{StartByte: 0, EndByte: 4, NewText: "X"},
				{StartByte: 2, EndByte: 6, NewText: "Y"},
			},
			wantErr: true,
		},
		{
			name:    "out of range rejected",
			src:     "abc",
			edits:   []locator.FileEdit{{StartByte: 2, EndByte: 99, NewText: "Z"}},
			wantErr: true,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := SpliceEdits([]byte(tt.src), tt.edits)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("SpliceEdits: want error, got %q", got)
				}
				return
			}
			if err != nil {
				t.Fatalf("SpliceEdits: unexpected error: %v", err)
			}
			if string(got) != tt.want {
				t.Fatalf("SpliceEdits = %q, want %q", got, tt.want)
			}
		})
	}
}

// TestSpliceEditsAdjacencyBoundary pins the exact overlap predicate. Two ranges
// that merely TOUCH (the earlier edit's EndByte equals the later edit's
// StartByte) are disjoint and must both apply; the rejection fires only when the
// earlier range's EndByte strictly exceeds the later range's StartByte. This is
// the half-open [start:end) boundary: [0:2) and [2:4) share no byte, so they are
// legal, while [0:3) and [2:4) share byte 2 and are rejected.
func TestSpliceEditsAdjacencyBoundary(t *testing.T) {
	tests := []struct {
		name    string
		src     string
		edits   []locator.FileEdit
		want    string
		wantErr bool
	}{
		{
			name: "touching ranges (end==next start) allowed",
			src:  "abcd",
			edits: []locator.FileEdit{
				{StartByte: 0, EndByte: 2, NewText: "X"},
				{StartByte: 2, EndByte: 4, NewText: "Y"},
			},
			want: "XY",
		},
		{
			name: "touching ranges given out of order allowed",
			src:  "abcd",
			edits: []locator.FileEdit{
				{StartByte: 2, EndByte: 4, NewText: "Y"},
				{StartByte: 0, EndByte: 2, NewText: "X"},
			},
			want: "XY",
		},
		{
			name: "touching zero-width insertion at a non-zero-width edit boundary allowed",
			src:  "abcd",
			edits: []locator.FileEdit{
				{StartByte: 0, EndByte: 2, NewText: "X"}, // replaces ab
				{StartByte: 2, EndByte: 2, NewText: "+"}, // pure insert at byte 2
			},
			want: "X+cd",
		},
		{
			name: "one-byte overlap (end > next start) rejected",
			src:  "abcd",
			edits: []locator.FileEdit{
				{StartByte: 0, EndByte: 3, NewText: "X"},
				{StartByte: 2, EndByte: 4, NewText: "Y"},
			},
			wantErr: true,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := SpliceEdits([]byte(tt.src), tt.edits)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("SpliceEdits: want overlap error, got %q", got)
				}
				return
			}
			if err != nil {
				t.Fatalf("SpliceEdits: unexpected error: %v", err)
			}
			if string(got) != tt.want {
				t.Fatalf("SpliceEdits = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestSpliceEditsNoMutation(t *testing.T) {
	src := []byte("abcdef")
	_, err := SpliceEdits(src, []locator.FileEdit{{StartByte: 0, EndByte: 2, NewText: "XX"}})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if string(src) != "abcdef" {
		t.Fatalf("src mutated to %q", src)
	}
}

func TestPointAt(t *testing.T) {
	src := []byte("ab\ncde\nf")
	tests := []struct {
		name    string
		offset  int
		wantRow int
		wantCol int
	}{
		{"start", 0, 0, 0},
		{"mid first line", 1, 0, 1},
		{"after first newline", 3, 1, 0},
		{"mid second line", 5, 1, 2},
		{"after second newline", 7, 2, 0},
		{"eof", 8, 2, 1},
		{"past eof clamps", 999, 2, 1},
		{"negative clamps", -5, 0, 0},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			p := PointAt(src, tt.offset)
			if p.Row != tt.wantRow || p.Col != tt.wantCol {
				t.Fatalf("PointAt(%d) = {%d,%d}, want {%d,%d}", tt.offset, p.Row, p.Col, tt.wantRow, tt.wantCol)
			}
		})
	}
}

// TestPointAtMultiByteUTF8 pins that PointAt counts BYTES, not runes or UTF-16
// code units. The euro sign U+20AC is 3 UTF-8 bytes; after it, Col must advance
// by 3 (one per byte), and a byte offset that lands in the middle of the rune is
// still a well-defined byte column. This is deliberate: parser.Point columns are
// byte columns (matching tree-sitter's byte-oriented points), so PointAt must
// not "round" to rune boundaries.
func TestPointAtMultiByteUTF8(t *testing.T) {
	// "a€b\nc" — bytes: a(1) €(3:bytes 1,2,3) b(1) \n(1) c(1) => len 7.
	src := []byte("a€b\nc")
	if len(src) != 7 {
		t.Fatalf("fixture byte length = %d, want 7", len(src))
	}
	tests := []struct {
		name    string
		offset  int
		wantRow int
		wantCol int
	}{
		{"before euro", 1, 0, 1},
		{"mid euro (2nd byte)", 2, 0, 2},
		{"mid euro (3rd byte)", 3, 0, 3},
		{"after euro at b", 4, 0, 4}, // a=1 + euro=3 bytes => col 4
		{"at newline byte", 5, 0, 5},
		{"start of second line", 6, 1, 0},
		{"eof", 7, 1, 1},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			p := PointAt(src, tt.offset)
			if p.Row != tt.wantRow || p.Col != tt.wantCol {
				t.Fatalf("PointAt(%d) = {%d,%d}, want {%d,%d}", tt.offset, p.Row, p.Col, tt.wantRow, tt.wantCol)
			}
		})
	}
}

// TestPointAtCRLF pins byte-column semantics across CRLF line endings. Only '\n'
// increments Row; the '\r' is an ordinary byte and counts toward Col. So the LF
// closing a CRLF line sits one column further right than it would after a bare
// LF, and Reparse passes byte-true points into ParseIncremental even on
// CRLF-terminated source rather than silently folding \r\n to \n.
func TestPointAtCRLF(t *testing.T) {
	// "ab\r\ncd" — bytes: a,b,\r,\n,c,d => len 6.
	src := []byte("ab\r\ncd")
	if len(src) != 6 {
		t.Fatalf("fixture byte length = %d, want 6", len(src))
	}
	tests := []struct {
		name    string
		offset  int
		wantRow int
		wantCol int
	}{
		{"start", 0, 0, 0},
		{"after a", 1, 0, 1},
		{"at CR", 2, 0, 2},
		{"at LF (CR counted as col)", 3, 0, 3},
		{"start of second line after LF", 4, 1, 0},
		{"after c", 5, 1, 1},
		{"eof", 6, 1, 2},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			p := PointAt(src, tt.offset)
			if p.Row != tt.wantRow || p.Col != tt.wantCol {
				t.Fatalf("PointAt(%d) = {%d,%d}, want {%d,%d}", tt.offset, p.Row, p.Col, tt.wantRow, tt.wantCol)
			}
		})
	}
}

func TestReparseFullWhenOldTreeNil(t *testing.T) {
	f := &fakeParser{}
	src := []byte("package main\n")
	edits := []locator.FileEdit{{StartByte: 0, EndByte: 7, NewText: "PACKAGE"}}

	newBytes, tree, err := Reparse(context.Background(), f, parser.LangGo, src, nil, edits)
	if err != nil {
		t.Fatalf("Reparse: %v", err)
	}
	if f.parseCalls != 1 || f.incrementalCalls != 0 {
		t.Fatalf("dispatch: parse=%d incremental=%d, want parse=1 incremental=0", f.parseCalls, f.incrementalCalls)
	}
	if string(newBytes) != "PACKAGE main\n" {
		t.Fatalf("newBytes = %q", newBytes)
	}
	if tree == nil || string(tree.Source) != "PACKAGE main\n" {
		t.Fatalf("tree source = %v", tree)
	}
}

func TestReparseIncrementalWhenOldTreeAndSingleEdit(t *testing.T) {
	f := &fakeParser{}
	src := []byte("ab\ncd\n")
	oldTree := &parser.Tree{Root: &parser.Node{Kind: "source_file"}, Source: src}
	// replace "cd" (bytes 3:5) with "xyz"
	edits := []locator.FileEdit{{StartByte: 3, EndByte: 5, NewText: "xyz"}}

	newBytes, _, err := Reparse(context.Background(), f, parser.LangGo, src, oldTree, edits)
	if err != nil {
		t.Fatalf("Reparse: %v", err)
	}
	if f.incrementalCalls != 1 || f.parseCalls != 0 {
		t.Fatalf("dispatch: parse=%d incremental=%d, want incremental=1 parse=0", f.parseCalls, f.incrementalCalls)
	}
	if f.lastOld != oldTree {
		t.Fatalf("oldTree not forwarded to ParseIncremental")
	}
	if string(newBytes) != "ab\nxyz\n" {
		t.Fatalf("newBytes = %q", newBytes)
	}
	want := parser.InputEdit{
		StartByte:   3,
		OldEndByte:  5,
		NewEndByte:  6,
		StartPoint:  parser.Point{Row: 1, Col: 0},
		OldEndPoint: parser.Point{Row: 1, Col: 2},
		NewEndPoint: parser.Point{Row: 1, Col: 3},
	}
	if f.lastEdit != want {
		t.Fatalf("InputEdit = %+v, want %+v", f.lastEdit, want)
	}
}

func TestReparseFullWhenMultiEditEvenWithOldTree(t *testing.T) {
	f := &fakeParser{}
	src := []byte("abcdefgh")
	oldTree := &parser.Tree{Root: &parser.Node{Kind: "source_file"}, Source: src}
	edits := []locator.FileEdit{
		{StartByte: 0, EndByte: 2, NewText: "XX"},
		{StartByte: 6, EndByte: 8, NewText: "YY"},
	}

	_, _, err := Reparse(context.Background(), f, parser.LangGo, src, oldTree, edits)
	if err != nil {
		t.Fatalf("Reparse: %v", err)
	}
	if f.parseCalls != 1 || f.incrementalCalls != 0 {
		t.Fatalf("multi-edit must use full Parse: parse=%d incremental=%d", f.parseCalls, f.incrementalCalls)
	}
}

func TestReparseSpliceErrorPropagates(t *testing.T) {
	f := &fakeParser{}
	src := []byte("abc")
	edits := []locator.FileEdit{{StartByte: 0, EndByte: 99, NewText: "Z"}}

	_, _, err := Reparse(context.Background(), f, parser.LangGo, src, nil, edits)
	if err == nil {
		t.Fatal("Reparse: want splice error")
	}
	if !strings.Contains(err.Error(), "out of bounds") {
		t.Fatalf("Reparse error = %v, want out-of-bounds", err)
	}
	if f.parseCalls != 0 || f.incrementalCalls != 0 {
		t.Fatal("Reparse must not parse when splice fails")
	}
}

func TestReparseParserErrorPropagates(t *testing.T) {
	sentinel := errors.New("boom")
	f := &fakeParser{parseErr: sentinel}
	_, _, err := Reparse(context.Background(), f, parser.LangGo, []byte("x"), nil,
		[]locator.FileEdit{{StartByte: 0, EndByte: 1, NewText: "y"}})
	if !errors.Is(err, sentinel) {
		t.Fatalf("Reparse error = %v, want wrapped sentinel", err)
	}
}
