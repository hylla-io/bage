package bage_test

import (
	"context"
	"os"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/pkg/bage"
)

// TestReadBlocks proves ReadBlocks anchors each Outline Symbol with the same
// region_hash region.HashRegion produces, honors includeContent for both Go
// declaration blocks and the text-fallback line blocks, and embeds the Symbol.
func TestReadBlocks(t *testing.T) {
	cases := []struct {
		name string
		file string
		src  string
	}{
		{
			name: "go declarations",
			file: "main.go",
			src:  "package main\n\nfunc Hello() {}\n\nfunc World() int { return 1 }\n",
		},
		{
			name: "text line fallback",
			file: "notes.txt",
			src:  "alpha\nbeta\ngamma\n",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			p := writeTemp(t, tc.file, tc.src)
			opened, err := bage.OpenFile(context.Background(), p)
			if err != nil {
				t.Fatalf("OpenFile: %v", err)
			}
			defer opened.Close()

			rawSrc, err := os.ReadFile(p)
			if err != nil {
				t.Fatalf("ReadFile: %v", err)
			}

			outline := bage.Outline(opened.Tree)
			if len(outline) == 0 {
				t.Fatalf("Outline returned no symbols")
			}

			noContent := bage.ReadBlocks(opened, false)
			withContent := bage.ReadBlocks(opened, true)

			if len(noContent) != len(outline) || len(withContent) != len(outline) {
				t.Fatalf("block count: noContent=%d withContent=%d outline=%d",
					len(noContent), len(withContent), len(outline))
			}

			for i, sym := range outline {
				wantHash := region.HashRegion(opened.Tree.Source, sym.StartByte, sym.EndByte)

				// (3) Block embeds the Symbol fields.
				if noContent[i].Symbol != sym {
					t.Errorf("block %d: Symbol = %+v, want %+v", i, noContent[i].Symbol, sym)
				}

				// (1) RegionHash matches region.HashRegion for the matching Symbol.
				if noContent[i].RegionHash != wantHash {
					t.Errorf("block %d: RegionHash = %q, want %q", i, noContent[i].RegionHash, wantHash)
				}
				if withContent[i].RegionHash != wantHash {
					t.Errorf("block %d (content): RegionHash = %q, want %q", i, withContent[i].RegionHash, wantHash)
				}

				// (2a) includeContent=false -> Content == "".
				if noContent[i].Content != "" {
					t.Errorf("block %d: Content = %q, want empty", i, noContent[i].Content)
				}

				// (2b) includeContent=true -> Content == src[Start:End].
				wantContent := string(rawSrc[sym.StartByte:sym.EndByte])
				if withContent[i].Content != wantContent {
					t.Errorf("block %d: Content = %q, want %q", i, withContent[i].Content, wantContent)
				}
			}
		})
	}
}

// goReadSrc is the small Go file the Editor.Read tests open: two functions plus a
// type so symbol filtering has something to discriminate.
const goReadSrc = "package main\n\nfunc Alpha() {}\n\nfunc Beta() int { return 2 }\n"

// TestEditorRead proves Editor.Read reports the path, detected language, the raw
// and normalized whole-file hashes computed with the Editor's hasher, and the
// same Blocks ReadBlocks yields for the opened file with no content requested.
func TestEditorRead(t *testing.T) {
	p := writeTemp(t, "main.go", goReadSrc)
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	res, err := ed.Read(context.Background(), p, bage.ReadOptions{})
	if err != nil {
		t.Fatalf("Read: %v", err)
	}

	if res.Path != p {
		t.Errorf("Path = %q, want %q", res.Path, p)
	}
	if res.Lang != bage.LangGo.String() {
		t.Errorf("Lang = %q, want %q", res.Lang, bage.LangGo.String())
	}

	src, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("ReadFile: %v", err)
	}
	h := hashing.XXHasher{}
	if want := hashing.RawHash(h, src); res.RawHash != want {
		t.Errorf("RawHash = %q, want %q", res.RawHash, want)
	}
	if want := hashing.NormHash(h, src); res.NormHash != want {
		t.Errorf("NormHash = %q, want %q", res.NormHash, want)
	}

	opened, err := bage.OpenFile(context.Background(), p)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer opened.Close()
	want := bage.ReadBlocks(opened, false)
	if len(res.Blocks) != len(want) {
		t.Fatalf("Blocks len = %d, want %d", len(res.Blocks), len(want))
	}
	for i := range want {
		if res.Blocks[i] != want[i] {
			t.Errorf("block %d = %+v, want %+v", i, res.Blocks[i], want[i])
		}
		if res.Blocks[i].Content != "" {
			t.Errorf("block %d: Content = %q, want empty", i, res.Blocks[i].Content)
		}
	}
}

// TestEditorReadSymbol proves ReadOptions.Symbol filters the result Blocks to
// only those whose embedded Symbol.Name matches.
func TestEditorReadSymbol(t *testing.T) {
	p := writeTemp(t, "main.go", goReadSrc)
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	res, err := ed.Read(context.Background(), p, bage.ReadOptions{Symbol: "Beta"})
	if err != nil {
		t.Fatalf("Read: %v", err)
	}
	if len(res.Blocks) != 1 {
		t.Fatalf("Blocks len = %d, want 1: %+v", len(res.Blocks), res.Blocks)
	}
	if res.Blocks[0].Name != "Beta" {
		t.Errorf("block Name = %q, want %q", res.Blocks[0].Name, "Beta")
	}
}

// TestEditorReadLine proves ReadOptions.Line addresses a single line: Read returns
// exactly one "range" Block covering line 2's resolved byte range, anchored by the
// same region_hash region.HashRegion produces for that range.
func TestEditorReadLine(t *testing.T) {
	src := "alpha\nbeta\ngamma\n"
	p := writeTemp(t, "notes.txt", src)
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	res, err := ed.Read(context.Background(), p, bage.ReadOptions{Line: 2})
	if err != nil {
		t.Fatalf("Read: %v", err)
	}
	if len(res.Blocks) != 1 {
		t.Fatalf("Blocks len = %d, want 1: %+v", len(res.Blocks), res.Blocks)
	}
	b := res.Blocks[0]
	if b.Kind != "range" {
		t.Errorf("Kind = %q, want %q", b.Kind, "range")
	}

	rawSrc, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("ReadFile: %v", err)
	}
	reg, err := bage.ResolveRange(rawSrc, 2, "", -1, -1)
	if err != nil {
		t.Fatalf("ResolveRange: %v", err)
	}
	if b.StartByte != reg.StartByte || b.EndByte != reg.EndByte {
		t.Errorf("range = [%d:%d], want [%d:%d]", b.StartByte, b.EndByte, reg.StartByte, reg.EndByte)
	}
	if b.StartLine != reg.StartLine || b.EndLine != reg.EndLine {
		t.Errorf("lines = [%d:%d], want [%d:%d]", b.StartLine, b.EndLine, reg.StartLine, reg.EndLine)
	}
	if want := region.HashRegion(rawSrc, reg.StartByte, reg.EndByte); b.RegionHash != want {
		t.Errorf("RegionHash = %q, want %q", b.RegionHash, want)
	}
}

// TestEditorReadByteRange proves ReadOptions.StartByte/EndByte (EndByte>StartByte)
// addresses a raw byte range: Read returns exactly one "range" Block over that
// exact range, anchored by region.HashRegion.
func TestEditorReadByteRange(t *testing.T) {
	src := "alpha\nbeta\ngamma\n"
	p := writeTemp(t, "notes.txt", src)
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	const a, b = 6, 10
	res, err := ed.Read(context.Background(), p, bage.ReadOptions{StartByte: a, EndByte: b})
	if err != nil {
		t.Fatalf("Read: %v", err)
	}
	if len(res.Blocks) != 1 {
		t.Fatalf("Blocks len = %d, want 1: %+v", len(res.Blocks), res.Blocks)
	}
	blk := res.Blocks[0]
	if blk.Kind != "range" {
		t.Errorf("Kind = %q, want %q", blk.Kind, "range")
	}
	if blk.StartByte != a || blk.EndByte != b {
		t.Errorf("range = [%d:%d], want [%d:%d]", blk.StartByte, blk.EndByte, a, b)
	}

	rawSrc, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("ReadFile: %v", err)
	}
	if want := region.HashRegion(rawSrc, a, b); blk.RegionHash != want {
		t.Errorf("RegionHash = %q, want %q", blk.RegionHash, want)
	}
}

// TestEditorReadModeExclusive proves combining Symbol with a line (or byte)
// addressing mode is rejected: the addressing modes are mutually exclusive.
func TestEditorReadModeExclusive(t *testing.T) {
	p := writeTemp(t, "main.go", goReadSrc)
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	if _, err := ed.Read(context.Background(), p, bage.ReadOptions{Symbol: "Beta", Line: 2}); err == nil {
		t.Fatal("Read with Symbol+Line = nil error, want error")
	}
}

// TestEditorReadContent proves ReadOptions.IncludeContent populates each Block's
// Content with its source slice.
func TestEditorReadContent(t *testing.T) {
	p := writeTemp(t, "main.go", goReadSrc)
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	res, err := ed.Read(context.Background(), p, bage.ReadOptions{IncludeContent: true})
	if err != nil {
		t.Fatalf("Read: %v", err)
	}
	if len(res.Blocks) == 0 {
		t.Fatalf("no blocks")
	}
	src, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("ReadFile: %v", err)
	}
	for i, b := range res.Blocks {
		want := string(src[b.StartByte:b.EndByte])
		if b.Content != want {
			t.Errorf("block %d: Content = %q, want %q", i, b.Content, want)
		}
	}
}
