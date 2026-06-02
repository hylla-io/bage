package bage_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"

	"github.com/hylla-io/bage/pkg/bage"
)

func writeTemp(t *testing.T, name, content string) string {
	t.Helper()
	dir := t.TempDir()
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
		t.Fatalf("write temp %q: %v", p, err)
	}
	return p
}

func TestOpenFile_Go(t *testing.T) {
	p := writeTemp(t, "main.go", "package main\n\nfunc Hello() {}\n")
	o, err := bage.OpenFile(context.Background(), p)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer o.Close()
	if o.Lang != bage.LangGo {
		t.Errorf("Lang = %v, want LangGo", o.Lang)
	}
	if o.Tree == nil || o.Tree.Root == nil {
		t.Fatalf("Tree/Root nil: %+v", o.Tree)
	}
	// Close is idempotent / nil-safe.
	o.Close()
	o.Close()
}

func TestOpenFile_TextFallback(t *testing.T) {
	// .txt has no registered grammar, so it exercises the text fallback. (.md
	// would resolve to the Markdown grammar — LangForPath maps it there.)
	p := writeTemp(t, "notes.txt", "first line\n\nthird line\n")
	o, err := bage.OpenFile(context.Background(), p)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer o.Close()
	if o.Lang != bage.LangText {
		t.Errorf("Lang = %v, want LangText", o.Lang)
	}
	if o.Tree.Root.Kind != "document" {
		t.Errorf("Root.Kind = %q, want document", o.Tree.Root.Kind)
	}
	// The text fallback is the engine-free tree (nil native handle); it now
	// carries line children, so identity is Native==nil, not child count.
	if o.Tree.Native != nil {
		t.Errorf("text fallback Tree.Native = %v, want nil", o.Tree.Native)
	}
	if len(o.Tree.Root.Children) == 0 {
		t.Errorf("text root should have line children")
	}
}

func TestOpenFile_MissingFile(t *testing.T) {
	_, err := bage.OpenFile(context.Background(), filepath.Join(t.TempDir(), "nope.go"))
	if err == nil {
		t.Fatal("OpenFile on missing file: want error, got nil")
	}
	if !errors.Is(err, os.ErrNotExist) {
		t.Errorf("err = %v, want wrapping os.ErrNotExist", err)
	}
}

func TestOutline_Go(t *testing.T) {
	src := "package main\n\ntype T struct{}\n\nfunc (t T) M() {}\n\nfunc F() {}\n"
	p := writeTemp(t, "x.go", src)
	o, err := bage.OpenFile(context.Background(), p)
	if err != nil {
		t.Fatalf("OpenFile: %v", err)
	}
	defer o.Close()

	syms := bage.Outline(o.Tree)
	if len(syms) == 0 {
		t.Fatal("Outline returned no symbols")
	}
	wantNames := map[string]bool{"T": false, "M": false, "F": false}
	for _, s := range syms {
		if s.EndByte > len(src) || s.StartByte < 0 || s.StartByte > s.EndByte {
			t.Errorf("symbol %q bad byte range [%d:%d] (len %d)", s.Kind, s.StartByte, s.EndByte, len(src))
		}
		if s.StartLine < 1 || s.EndLine < s.StartLine {
			t.Errorf("symbol %q bad line range %d-%d", s.Kind, s.StartLine, s.EndLine)
		}
		if _, ok := wantNames[s.Name]; ok {
			wantNames[s.Name] = true
		}
	}
	for n, seen := range wantNames {
		if !seen {
			t.Errorf("Outline missing declaration name %q; got %+v", n, syms)
		}
	}
}

func TestOutline_Recurse(t *testing.T) {
	// The method m is nested inside the impl region of a Rust impl block;
	// walkDecls must recurse to surface it.
	src := "struct S;\nimpl S {\n    fn m(&self) {}\n}\n"
	tree, err := bage.NewParser().Parse(context.Background(), bage.LangRust, []byte(src))
	if err != nil {
		t.Fatalf("Parse rust: %v", err)
	}
	defer tree.Close()
	syms := bage.Outline(tree)
	var sawFn bool
	for _, s := range syms {
		if s.Name == "m" {
			sawFn = true
		}
	}
	if !sawFn {
		t.Errorf("Outline did not surface nested fn m; got %+v", syms)
	}
}

func TestOutline_TextLines(t *testing.T) {
	src := []byte("a\n\nbb\nccc")
	tree, err := bage.NewParser().Parse(context.Background(), bage.LangText, src)
	if err != nil {
		t.Fatalf("Parse text: %v", err)
	}
	defer tree.Close()
	syms := bage.Outline(tree)
	if len(syms) != 4 {
		t.Fatalf("got %d line symbols, want 4: %+v", len(syms), syms)
	}
	want := []struct {
		start, end, line int
	}{
		{0, 1, 1}, // "a"
		{2, 2, 2}, // "" (empty interior line)
		{3, 5, 3}, // "bb"
		{6, 9, 4}, // "ccc" (no trailing newline)
	}
	for i, w := range want {
		s := syms[i]
		if s.Kind != "line" || s.StartByte != w.start || s.EndByte != w.end || s.StartLine != w.line || s.EndLine != w.line {
			t.Errorf("line %d = %+v, want kind=line bytes [%d:%d] line %d", i, s, w.start, w.end, w.line)
		}
	}
}

func TestOutline_TextTrailingNewline(t *testing.T) {
	tree, err := bage.NewParser().Parse(context.Background(), bage.LangText, []byte("x\n"))
	if err != nil {
		t.Fatalf("Parse text: %v", err)
	}
	defer tree.Close()
	syms := bage.Outline(tree)
	if len(syms) != 1 {
		t.Fatalf("trailing-newline source: got %d symbols, want 1 (no phantom line): %+v", len(syms), syms)
	}
	if syms[0].StartByte != 0 || syms[0].EndByte != 1 {
		t.Errorf("line bytes = [%d:%d], want [0:1]", syms[0].StartByte, syms[0].EndByte)
	}
}

func TestOutline_Nil(t *testing.T) {
	if got := bage.Outline(nil); got != nil {
		t.Errorf("Outline(nil) = %+v, want nil", got)
	}
	if got := bage.Outline(&bage.Tree{}); got != nil {
		t.Errorf("Outline(&Tree{}) = %+v, want nil", got)
	}
}
