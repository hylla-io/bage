package bage_test

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/hylla-io/bage/pkg/bage"
)

// TestEditorAutoDetectMultiFile exercises the public facade end-to-end in
// auto-detect mode: an Editor opened with NO Lang edits a .go file and a .py
// file in one atomic Apply, with each file's language resolved per path via
// LangForPath. It also drives the edit entirely through the PUBLIC hash
// re-exports (bage.RegionHash / RawHash / NormHash) — the exact functions a host
// like Hylla uses — so a regression in either the auto-detect wiring or the
// re-exports fails here.
func TestEditorAutoDetectMultiFile(t *testing.T) {
	dir := t.TempDir()
	goSrc := "package main\n\nvar G = \"hi\"\n"
	pySrc := "name = \"hi\"\n"
	goFile := filepath.Join(dir, "main.go")
	pyFile := filepath.Join(dir, "app.py")
	if err := os.WriteFile(goFile, []byte(goSrc), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(pyFile, []byte(pySrc), 0o644); err != nil {
		t.Fatal(err)
	}

	// No Lang → per-file auto-detect (main.go → Go, app.py → Python).
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open (auto-detect): %v", err)
	}
	defer ed.Close()

	mkEdit := func(file, src, old, repl string) (bage.Edit, bage.FileAnchor) {
		start := strings.Index(src, old)
		if start < 0 {
			t.Fatalf("test bug: %q not in %q", old, src)
		}
		end := start + len(old)
		live := []byte(src)
		h := bage.XXHasher{}
		return bage.Edit{
				Region: bage.Region{
					Path:       file,
					StartByte:  start,
					EndByte:    end,
					RegionHash: bage.RegionHash(live, start, end),
				},
				NewText: repl,
			}, bage.FileAnchor{
				Path:     file,
				RawHash:  bage.RawHash(h, live),
				NormHash: bage.NormHash(h, live),
			}
	}

	goEdit, goAnchor := mkEdit(goFile, goSrc, "hi", "hello")
	pyEdit, pyAnchor := mkEdit(pyFile, pySrc, "hi", "hello")

	results, err := ed.Apply(context.Background(),
		[]bage.Edit{goEdit, pyEdit}, []bage.FileAnchor{goAnchor, pyAnchor})
	if err != nil {
		t.Fatalf("Apply: %v", err)
	}
	if len(results) != 2 {
		t.Fatalf("want 2 EditResults, got %d", len(results))
	}

	if got := readAll(t, goFile); got != "package main\n\nvar G = \"hello\"\n" {
		t.Errorf("go file = %q", got)
	}
	if got := readAll(t, pyFile); got != "name = \"hello\"\n" {
		t.Errorf("py file = %q", got)
	}
}

func readAll(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %q: %v", path, err)
	}
	return string(b)
}
