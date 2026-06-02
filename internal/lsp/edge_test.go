package lsp

import (
	"strings"
	"testing"

	"go.lsp.dev/protocol"
	"go.lsp.dev/uri"

	"github.com/hylla-io/bage/internal/edit"
)

// TestByteOffsetCRLF proves that a '\r' is treated as an ordinary one-unit,
// one-byte rune on its line: the LSP character walk does not special-case
// carriage returns, so on a CRLF line the '\r' consumes one UTF-16 unit and one
// byte exactly like any ASCII character, and only the '\n' terminates the line.
func TestByteOffsetCRLF(t *testing.T) {
	// "ab\r\ncd" — line 0 is "ab\r" (the '\r' is content, the '\n' terminates).
	src := []byte("ab\r\ncd")
	tests := []struct {
		name string
		pos  protocol.Position
		want int
	}{
		{name: "before cr", pos: pos(0, 2), want: 2},             // after "ab", at '\r'
		{name: "cr counts as one unit", pos: pos(0, 3), want: 3}, // past '\r', at '\n'
		// char index past the line's content clamps to the terminating '\n' (byte 3),
		// never crossing it.
		{name: "past content clamps to nl", pos: pos(0, 99), want: 3},
		// line 1 begins after the '\n' at byte 4.
		{name: "next line start", pos: pos(1, 0), want: 4},
		{name: "next line mid", pos: pos(1, 1), want: 5},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ByteOffset(src, tc.pos)
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.want {
				t.Fatalf("ByteOffset(%q, %v) = %d, want %d", src, tc.pos, got, tc.want)
			}
		})
	}
}

// TestByteOffsetLeadingBOM proves a leading UTF-8 BOM (EF BB BF, U+FEFF) is
// counted as a single UTF-16 code unit occupying three bytes. LSP servers that
// do not strip the BOM report character offsets that include it, so the walk
// must advance one unit across the three BOM bytes.
func TestByteOffsetLeadingBOM(t *testing.T) {
	// BOM + "xy": BOM = 3 bytes / 1 unit, then x(1/1) y(1/1). The BOM (U+FEFF,
	// bytes EF BB BF) is built explicitly because a literal BOM in Go source is
	// an illegal byte-order mark.
	src := append([]byte{0xEF, 0xBB, 0xBF}, []byte("xy")...)
	if len(src) != 5 {
		t.Fatalf("fixture sanity: BOM+xy should be 5 bytes, got %d", len(src))
	}
	tests := []struct {
		name string
		char uint32
		want int
	}{
		{name: "at bom", char: 0, want: 0},
		{name: "after bom (one unit, three bytes)", char: 1, want: 3},
		{name: "after x", char: 2, want: 4},
		{name: "after y", char: 3, want: 5},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ByteOffset(src, pos(0, tc.char))
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.want {
				t.Fatalf("ByteOffset BOM char %d = %d, want %d", tc.char, got, tc.want)
			}
		})
	}
}

// TestByteOffsetAstralBoundary pins the exact boundary between two adjacent
// astral runes. Each emoji is 4 UTF-8 bytes and 2 UTF-16 units, so the only
// rune boundaries are at char 0, 2, and 4. A character budget that lands exactly
// on the seam between the two emoji (char 2) must resolve to the start byte of
// the second emoji (byte 4), neither splitting the first nor over-running into
// the second.
func TestByteOffsetAstralBoundary(t *testing.T) {
	// "😀😁" — U+1F600 then U+1F601, each 4 bytes / 2 units.
	src := []byte("😀😁")
	if len(src) != 8 {
		t.Fatalf("fixture sanity: two emoji should be 8 bytes, got %d", len(src))
	}
	tests := []struct {
		name string
		char uint32
		want int
	}{
		{name: "start", char: 0, want: 0},
		// the exact seam between the two surrogate pairs.
		{name: "boundary between emoji", char: 2, want: 4},
		{name: "after both", char: 4, want: 8},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ByteOffset(src, pos(0, tc.char))
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.want {
				t.Fatalf("ByteOffset astral char %d = %d, want %d", tc.char, got, tc.want)
			}
		})
	}
}

// TestWorkspaceEditDualSourceOverlapRejected proves the dual-source hazard does
// not silently misapply. When a server reports the SAME file URI in BOTH
// we.Changes AND we.DocumentChanges, WorkspaceEditToFileEdits faithfully emits
// duplicate FileEdits for the overlapping range (it does not dedupe — that is a
// downstream apply concern). The contract guard is that the splice layer
// (edit.SpliceEdits, mirrored by session/locator.ApplyFileEdits) REJECTS the
// resulting overlap rather than applying it twice. This test asserts both: the
// duplicate edits are produced, and feeding them to SpliceEdits errors.
func TestWorkspaceEditDualSourceOverlapRejected(t *testing.T) {
	fooURI := uri.File("/tmp/dup.go")
	fooPath := fooURI.Filename()
	src := []byte("func old() {}\n")
	read := func(path string) ([]byte, error) {
		if path != fooPath {
			t.Fatalf("unexpected read of %q", path)
		}
		return src, nil
	}

	edits := []protocol.TextEdit{
		{Range: protocol.Range{Start: pos(0, 5), End: pos(0, 8)}, NewText: "new"}, // "old"
	}
	we := protocol.WorkspaceEdit{
		Changes: map[protocol.DocumentURI][]protocol.TextEdit{fooURI: edits},
		DocumentChanges: []protocol.TextDocumentEdit{
			{
				TextDocument: protocol.OptionalVersionedTextDocumentIdentifier{
					TextDocumentIdentifier: protocol.TextDocumentIdentifier{URI: fooURI},
				},
				Edits: edits,
			},
		},
	}

	got, err := WorkspaceEditToFileEdits(we, read)
	if err != nil {
		t.Fatalf("convert: %v", err)
	}
	// Both sources contribute the same range → two identical FileEdits.
	if len(got) != 2 {
		t.Fatalf("expected 2 duplicate edits from dual source, got %d: %+v", len(got), got)
	}
	if got[0] != got[1] {
		t.Fatalf("expected the two edits to be identical, got %+v and %+v", got[0], got[1])
	}

	// The load-bearing guarantee: the splice layer rejects the overlap rather
	// than applying "new" twice (which would corrupt the file).
	if _, err := edit.SpliceEdits(src, got); err == nil {
		t.Fatalf("expected SpliceEdits to reject duplicate/overlapping edits, got nil")
	} else if !strings.Contains(err.Error(), "overlap") {
		t.Fatalf("expected an overlap rejection, got: %v", err)
	}
}

// TestWorkspaceEditURIDecodesSpecialPath proves URI→path decoding round-trips a
// path containing a space and other special characters. uri.File percent-encodes
// on the way in; WorkspaceEditToFileEdits resolves back via DocumentURI.Filename
// and the read function must be invoked with the DECODED filesystem path.
func TestWorkspaceEditURIDecodesSpecialPath(t *testing.T) {
	// A path with a space and a percent-prone character.
	wantPath := "/tmp/my dir/a+b#c.go"
	fileURI := uri.File(wantPath)
	if !strings.Contains(string(fileURI), "%20") {
		t.Fatalf("fixture sanity: expected space to be percent-encoded in URI %q", fileURI)
	}

	src := []byte("var x = 1\n")
	var readPath string
	read := func(path string) ([]byte, error) {
		readPath = path
		return src, nil
	}

	we := protocol.WorkspaceEdit{
		Changes: map[protocol.DocumentURI][]protocol.TextEdit{
			fileURI: {{Range: protocol.Range{Start: pos(0, 4), End: pos(0, 5)}, NewText: "y"}},
		},
	}
	got, err := WorkspaceEditToFileEdits(we, read)
	if err != nil {
		t.Fatalf("convert: %v", err)
	}
	if readPath != wantPath {
		t.Fatalf("read invoked with decoded path %q, want %q", readPath, wantPath)
	}
	if len(got) != 1 || got[0].Path != wantPath {
		t.Fatalf("edit path not decoded: got %+v, want path %q", got, wantPath)
	}
}
