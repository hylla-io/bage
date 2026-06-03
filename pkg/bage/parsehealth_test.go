package bage

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

// TestParseHealthBrokenGo asserts that ParseHealth surfaces the ERROR/MISSING
// node a deliberately broken Go file produces, with a 1-based line/col that
// points at the defect and a non-empty byte range. tree-sitter recovers from the
// missing closing brace by inserting a MISSING node (or producing an ERROR), so a
// healthy parser MUST report at least one defect here.
func TestParseHealthBrokenGo(t *testing.T) {
	// The function body is never closed: a recoverable syntax error.
	src := "package main\n\nfunc broken() {\n\treturn\n"
	path := writeTemp(t, "broken.go", src)

	opened, err := OpenFile(context.Background(), path)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer opened.Close()

	defects := ParseHealth(opened)
	if len(defects) == 0 {
		t.Fatalf("expected at least one parse defect for broken Go, got none")
	}
	for _, d := range defects {
		if d.Line < 1 || d.Col < 1 {
			t.Fatalf("defect line/col must be 1-based >= 1, got line=%d col=%d (%+v)", d.Line, d.Col, d)
		}
		if d.EndByte < d.StartByte || d.StartByte < 0 {
			t.Fatalf("defect byte range invalid: %+v", d)
		}
		if d.Kind != "ERROR" && d.Kind != "MISSING" {
			t.Fatalf("defect Kind must be ERROR or MISSING, got %q", d.Kind)
		}
	}
}

// TestParseHealthCleanGo asserts a syntactically valid Go file reports NO defects.
func TestParseHealthCleanGo(t *testing.T) {
	src := "package main\n\nfunc ok() int { return 1 }\n"
	path := writeTemp(t, "ok.go", src)

	opened, err := OpenFile(context.Background(), path)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer opened.Close()

	if defects := ParseHealth(opened); len(defects) != 0 {
		t.Fatalf("expected no defects for clean Go, got %d: %+v", len(defects), defects)
	}
}

// TestParseHealthTextAlwaysClean asserts a grammar-free text file (LangText)
// reports NO defects: the text fallback always parses losslessly, so there is no
// such thing as a parse error for it. The fixture is deliberately content that
// would be a syntax error under a real grammar (unbalanced braces) to prove the
// text path never invents a defect.
func TestParseHealthTextAlwaysClean(t *testing.T) {
	src := "this is { not balanced (\nand never errors\n"
	path := writeTemp(t, "notes.txt", src)

	opened, err := OpenFile(context.Background(), path)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer opened.Close()

	if opened.Lang.String() != "text" {
		t.Fatalf("expected .txt to resolve to text lang, got %s", opened.Lang)
	}
	if defects := ParseHealth(opened); len(defects) != 0 {
		t.Fatalf("expected no defects for text fallback, got %d: %+v", len(defects), defects)
	}
}

// writeTemp writes content to a temp file named name and returns its path.
func writeTemp(t *testing.T, name, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("write %q: %v", path, err)
	}
	return path
}
