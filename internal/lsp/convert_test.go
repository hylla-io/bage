package lsp

import (
	"errors"
	"sort"
	"testing"

	"go.lsp.dev/protocol"
	"go.lsp.dev/uri"

	"github.com/hylla-io/bage/internal/locator"
)

func pos(line, char uint32) protocol.Position {
	return protocol.Position{Line: line, Character: char}
}

func TestByteOffset(t *testing.T) {
	tests := []struct {
		name    string
		src     string
		pos     protocol.Position
		want    int
		wantErr bool
	}{
		// --- ASCII, single line ---
		{name: "ascii start", src: "hello", pos: pos(0, 0), want: 0},
		{name: "ascii mid", src: "hello", pos: pos(0, 3), want: 3},
		{name: "ascii end", src: "hello", pos: pos(0, 5), want: 5},

		// --- multi-byte UTF-8: "é" is 2 bytes, 1 UTF-16 unit ---
		// "café" = c(1) a(1) f(1) é(2 bytes). char index 4 = after é.
		{name: "utf8 before accent", src: "café", pos: pos(0, 3), want: 3},
		{name: "utf8 after accent", src: "café", pos: pos(0, 4), want: 5},
		// a char index landing right after the accent in a longer line:
		// "é=2" : é(2 bytes,1 unit) =(1) ... char 1 -> byte 2.
		{name: "utf8 accent first", src: "é=2", pos: pos(0, 1), want: 2},

		// --- astral rune: "𝛂" U+1D6C2 is 4 UTF-8 bytes, 2 UTF-16 units ---
		// "𝛂x": char 2 (past the surrogate pair) -> byte 4 (start of x).
		{name: "astral past pair", src: "𝛂x", pos: pos(0, 2), want: 4},
		// char 1 lands *inside* the surrogate pair: we only advance whole runes,
		// so after consuming 2 units we stop — char budget 1 can't split the rune,
		// loop exits with consumed=0<1 but the rune is 2 units, so it advances once
		// to consumed=2 and offset=4. char index 1 therefore clamps to byte 4.
		{name: "astral inside pair clamps forward", src: "𝛂x", pos: pos(0, 1), want: 4},
		// emoji "😀" U+1F600 = 4 bytes, 2 UTF-16 units.
		{name: "emoji past pair", src: "😀!", pos: pos(0, 2), want: 4},

		// --- multi-line ---
		{name: "line 1 start", src: "ab\ncd\nef", pos: pos(1, 0), want: 3},
		{name: "line 1 mid", src: "ab\ncd\nef", pos: pos(1, 1), want: 4},
		{name: "line 2 start", src: "ab\ncd\nef", pos: pos(2, 0), want: 6},
		{name: "line 2 end", src: "ab\ncd\nef", pos: pos(2, 2), want: 8},
		// multi-byte on a non-first line: "x\né" line1 char1 -> byte after é.
		{name: "multibyte on line 1", src: "x\né!", pos: pos(1, 1), want: 4},

		// --- clamping ---
		{name: "char past line end mid-file", src: "ab\ncd", pos: pos(0, 99), want: 2},
		{name: "char past line end last line", src: "ab\ncd", pos: pos(1, 99), want: 5},
		{name: "line past EOF", src: "ab\ncd", pos: pos(9, 0), want: 5},
		{name: "line just past EOF no trailing nl", src: "ab", pos: pos(1, 0), want: 2},
		// trailing newline: line 1 is the empty line after it.
		{name: "empty trailing line", src: "ab\n", pos: pos(1, 0), want: 3},

		// --- edge: empty src ---
		{name: "empty src zero pos", src: "", pos: pos(0, 0), want: 0},
		{name: "empty src line past", src: "", pos: pos(3, 0), want: 0},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ByteOffset([]byte(tc.src), tc.pos)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("expected error, got offset %d", got)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.want {
				t.Fatalf("ByteOffset(%q, %v) = %d, want %d", tc.src, tc.pos, got, tc.want)
			}
		})
	}
}

func TestByteOffsetMalformedUTF8(t *testing.T) {
	// 0xFF is never a valid UTF-8 lead byte. Walking onto it on the target line
	// must be rejected rather than silently mis-counted.
	src := []byte{'a', 0xFF, 'b'}
	if _, err := ByteOffset(src, pos(0, 2)); err == nil {
		t.Fatalf("expected error on malformed UTF-8, got nil")
	}
	// But a position that stops before the bad byte is fine.
	got, err := ByteOffset(src, pos(0, 1))
	if err != nil {
		t.Fatalf("unexpected error before malformed byte: %v", err)
	}
	if got != 1 {
		t.Fatalf("got %d, want 1", got)
	}
}

// sortEdits gives a deterministic order for comparing flattened edits regardless
// of the (non-deterministic) map iteration order in WorkspaceEditToFileEdits.
func sortEdits(es []locator.FileEdit) {
	sort.Slice(es, func(i, j int) bool {
		if es[i].Path != es[j].Path {
			return es[i].Path < es[j].Path
		}
		if es[i].StartByte != es[j].StartByte {
			return es[i].StartByte < es[j].StartByte
		}
		return es[i].NewText < es[j].NewText
	})
}

func TestWorkspaceEditToFileEdits(t *testing.T) {
	fooURI := uri.File("/tmp/foo.go")
	barURI := uri.File("/tmp/bar.go")
	fooPath := fooURI.Filename()
	barPath := barURI.Filename()

	files := map[string][]byte{
		// "func café()" — é at bytes 9-10 (2 bytes), char index 9 is the é,
		// char 10 is past it.
		fooPath: []byte("func café()\n"),
		barPath: []byte("x := 𝛂\n"),
	}
	read := func(path string) ([]byte, error) {
		b, ok := files[path]
		if !ok {
			return nil, errors.New("not found")
		}
		return b, nil
	}

	t.Run("changes map single file multiple edits", func(t *testing.T) {
		we := protocol.WorkspaceEdit{
			Changes: map[protocol.DocumentURI][]protocol.TextEdit{
				fooURI: {
					{Range: protocol.Range{Start: pos(0, 0), End: pos(0, 4)}, NewText: "FUNC"},
					{Range: protocol.Range{Start: pos(0, 5), End: pos(0, 9)}, NewText: "cafe"},
				},
			},
		}
		got, err := WorkspaceEditToFileEdits(we, read)
		if err != nil {
			t.Fatalf("convert: %v", err)
		}
		sortEdits(got)
		want := []locator.FileEdit{
			{Path: fooPath, StartByte: 0, EndByte: 4, NewText: "FUNC"},
			// "func café()": c=byte5 a=6 f=7 é=bytes8-9(2 bytes,1 unit). char 5..9
			// spans "café" → bytes 5..10 (char 9 is past é, i.e. the ')').
			{Path: fooPath, StartByte: 5, EndByte: 10, NewText: "cafe"},
		}
		assertEdits(t, got, want)
	})

	t.Run("multiple files in changes", func(t *testing.T) {
		we := protocol.WorkspaceEdit{
			Changes: map[protocol.DocumentURI][]protocol.TextEdit{
				fooURI: {{Range: protocol.Range{Start: pos(0, 0), End: pos(0, 4)}, NewText: "F"}},
				barURI: {{Range: protocol.Range{Start: pos(0, 0), End: pos(0, 1)}, NewText: "y"}},
			},
		}
		got, err := WorkspaceEditToFileEdits(we, read)
		if err != nil {
			t.Fatalf("convert: %v", err)
		}
		sortEdits(got)
		want := []locator.FileEdit{
			{Path: barPath, StartByte: 0, EndByte: 1, NewText: "y"},
			{Path: fooPath, StartByte: 0, EndByte: 4, NewText: "F"},
		}
		assertEdits(t, got, want)
	})

	t.Run("astral range in bar", func(t *testing.T) {
		// "x := 𝛂\n": x(0) sp(1) :(2) =(3) sp(4) 𝛂(byte5..9, char5..7).
		we := protocol.WorkspaceEdit{
			Changes: map[protocol.DocumentURI][]protocol.TextEdit{
				barURI: {{Range: protocol.Range{Start: pos(0, 5), End: pos(0, 7)}, NewText: "Z"}},
			},
		}
		got, err := WorkspaceEditToFileEdits(we, read)
		if err != nil {
			t.Fatalf("convert: %v", err)
		}
		want := []locator.FileEdit{
			{Path: barPath, StartByte: 5, EndByte: 9, NewText: "Z"},
		}
		assertEdits(t, got, want)
	})

	t.Run("document changes form", func(t *testing.T) {
		we := protocol.WorkspaceEdit{
			DocumentChanges: []protocol.TextDocumentEdit{
				{
					TextDocument: protocol.OptionalVersionedTextDocumentIdentifier{
						TextDocumentIdentifier: protocol.TextDocumentIdentifier{URI: fooURI},
					},
					Edits: []protocol.TextEdit{
						{Range: protocol.Range{Start: pos(0, 5), End: pos(0, 9)}, NewText: "cafe"},
					},
				},
			},
		}
		got, err := WorkspaceEditToFileEdits(we, read)
		if err != nil {
			t.Fatalf("convert: %v", err)
		}
		want := []locator.FileEdit{
			{Path: fooPath, StartByte: 5, EndByte: 10, NewText: "cafe"},
		}
		assertEdits(t, got, want)
	})

	t.Run("changes and documentChanges combined", func(t *testing.T) {
		we := protocol.WorkspaceEdit{
			Changes: map[protocol.DocumentURI][]protocol.TextEdit{
				fooURI: {{Range: protocol.Range{Start: pos(0, 0), End: pos(0, 4)}, NewText: "F"}},
			},
			DocumentChanges: []protocol.TextDocumentEdit{
				{
					TextDocument: protocol.OptionalVersionedTextDocumentIdentifier{
						TextDocumentIdentifier: protocol.TextDocumentIdentifier{URI: barURI},
					},
					Edits: []protocol.TextEdit{
						{Range: protocol.Range{Start: pos(0, 0), End: pos(0, 1)}, NewText: "y"},
					},
				},
			},
		}
		got, err := WorkspaceEditToFileEdits(we, read)
		if err != nil {
			t.Fatalf("convert: %v", err)
		}
		sortEdits(got)
		want := []locator.FileEdit{
			{Path: barPath, StartByte: 0, EndByte: 1, NewText: "y"},
			{Path: fooPath, StartByte: 0, EndByte: 4, NewText: "F"},
		}
		assertEdits(t, got, want)
	})

	t.Run("read error is wrapped", func(t *testing.T) {
		we := protocol.WorkspaceEdit{
			Changes: map[protocol.DocumentURI][]protocol.TextEdit{
				uri.File("/tmp/missing.go"): {{Range: protocol.Range{Start: pos(0, 0), End: pos(0, 1)}, NewText: "z"}},
			},
		}
		if _, err := WorkspaceEditToFileEdits(we, read); err == nil {
			t.Fatalf("expected read error, got nil")
		}
	})
}

func assertEdits(t *testing.T, got, want []locator.FileEdit) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("got %d edits, want %d: %+v", len(got), len(want), got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("edit[%d] = %+v, want %+v", i, got[i], want[i])
		}
	}
}
