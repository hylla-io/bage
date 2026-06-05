package bage_test

import (
	"context"
	"os"
	"testing"

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
