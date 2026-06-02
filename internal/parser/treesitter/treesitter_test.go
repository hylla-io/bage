package treesitter

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/parser"
)

// findKind returns the first node of the given kind in a pre-order walk, or nil.
func findKind(n *parser.Node, kind string) *parser.Node {
	if n == nil {
		return nil
	}
	if n.Kind == kind {
		return n
	}
	for _, c := range n.Children {
		if got := findKind(c, kind); got != nil {
			return got
		}
	}
	return nil
}

func TestParseGo(t *testing.T) {
	a := New()
	src := []byte("package main\n\nfunc main() {}\n")

	tree, err := a.Parse(context.Background(), parser.LangGo, src)
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	defer tree.Close()

	if tree.Root == nil || tree.Root.Kind != "source_file" {
		t.Fatalf("root kind = %v, want source_file", tree.Root)
	}
	if string(tree.Source) != string(src) {
		t.Fatalf("Source not preserved")
	}

	fn := findKind(tree.Root, "function_declaration")
	if fn == nil {
		t.Fatalf("no function_declaration node found")
	}
	if !fn.Named {
		t.Fatalf("function_declaration should be a named node")
	}
	if fn.StartByte < 0 || fn.EndByte <= fn.StartByte || fn.EndByte > len(src) {
		t.Fatalf("function_declaration byte range invalid: [%d:%d]", fn.StartByte, fn.EndByte)
	}
	if got := string(src[fn.StartByte:fn.EndByte]); got != "func main() {}" {
		t.Fatalf("function_declaration text = %q, want %q", got, "func main() {}")
	}
}

// TestParsePolyglot exercises every registered grammar: each tiny valid snippet
// must parse into a tree whose root spans the whole source and has children,
// survive a byte-range splice round-trip, and re-parse afterwards. Invariants are
// structural (full span + non-empty children) so they hold across grammars
// without hardcoding root-kind strings.
func TestParsePolyglot(t *testing.T) {
	a := New()
	cases := []struct {
		name string
		lang parser.Lang
		src  string
		// edit replaces old (a byte substring present in src) with new and the
		// spliced source must still parse.
		old, new string
	}{
		{"go", parser.LangGo, "package main\n\nfunc main() {}\n", "main", "run"},
		{"typescript", parser.LangTypeScript, "const x: number = 1;\n", "1", "2"},
		{"tsx", parser.LangTSX, "const e = <div>hi</div>;\n", "hi", "yo"},
		{"javascript", parser.LangJavaScript, "const x = 1;\n", "1", "2"},
		{"python", parser.LangPython, "def f():\n    return 1\n", "1", "2"},
		{"rust", parser.LangRust, "fn main() { let x = 1; }\n", "1", "2"},
		{"java", parser.LangJava, "class C { int x = 1; }\n", "1", "2"},
		{"c", parser.LangC, "int main() { return 0; }\n", "0", "1"},
		{"cpp", parser.LangCPP, "int main() { return 0; }\n", "0", "1"},
		{"csharp", parser.LangCSharp, "class C { int X = 1; }\n", "1", "2"},
		{"ruby", parser.LangRuby, "def f\n  1\nend\n", "1", "2"},
		{"json", parser.LangJSON, "{\"a\": 1}\n", "1", "2"},
		{"html", parser.LangHTML, "<div>hi</div>\n", "hi", "yo"},
		{"css", parser.LangCSS, "a { color: red; }\n", "red", "blue"},
		{"yaml", parser.LangYAML, "version: \"3\"\nservices:\n  web:\n    image: nginx:latest\n    ports:\n      - \"8080:80\"\n", "nginx", "caddy"},
		{"toml", parser.LangTOML, "[package]\nname = \"smoke\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1.0\"\n", "smoke", "demo"},
		{"xml", parser.LangXML, "<project>\n  <name>smoke</name>\n  <version>1.0</version>\n</project>\n", "smoke", "demo"},
		{"makefile", parser.LangMakefile, "build:\n\tgo build ./...\n\ntest:\n\tgo test ./...\n", "go test", "go vet"},
		{"bash", parser.LangBash, "#!/bin/bash\nset -euo pipefail\n\ngreet() {\n  echo \"hi\"\n}\n\ngreet\n", "hi", "yo"},
		{"markdown", parser.LangMarkdown, "# Title\n\nA paragraph with **bold** text.\n\n- item one\n- item two\n", "bold", "strong"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			src := []byte(tc.src)
			tree, err := a.Parse(context.Background(), tc.lang, src)
			if err != nil {
				t.Fatalf("Parse(%s): %v", tc.name, err)
			}
			defer tree.Close()

			if tree.Root == nil {
				t.Fatalf("%s: nil root", tc.name)
			}
			if tree.Root.StartByte != 0 || tree.Root.EndByte != len(src) {
				t.Fatalf("%s: root span [%d:%d], want [0:%d]", tc.name, tree.Root.StartByte, tree.Root.EndByte, len(src))
			}
			if len(tree.Root.Children) == 0 {
				t.Fatalf("%s: root has no children", tc.name)
			}
			if string(tree.Source) != tc.src {
				t.Fatalf("%s: Source not preserved", tc.name)
			}

			// Byte-range round-trip: splice tc.old -> tc.new and re-parse.
			idx := strings.Index(tc.src, tc.old)
			if idx < 0 {
				t.Fatalf("%s: test bug, old %q not in src", tc.name, tc.old)
			}
			spliced := tc.src[:idx] + tc.new + tc.src[idx+len(tc.old):]
			tree2, err := a.Parse(context.Background(), tc.lang, []byte(spliced))
			if err != nil {
				t.Fatalf("%s: re-parse after splice: %v", tc.name, err)
			}
			defer tree2.Close()

			if tree2.Root == nil {
				t.Fatalf("%s: nil root after splice", tc.name)
			}
			if tree2.Root.EndByte != len(spliced) {
				t.Fatalf("%s: spliced root end %d, want %d", tc.name, tree2.Root.EndByte, len(spliced))
			}
			if len(tree2.Root.Children) == 0 {
				t.Fatalf("%s: spliced root has no children", tc.name)
			}
		})
	}
}

// TestParseTextFallback exercises the grammar-free text fallback with REAL
// content for the file types that have no registered tree-sitter grammar
// (Markdown, MDX, Dockerfile, SCSS, plain text) plus an empty-file edge case.
// The fallback must round-trip every byte exactly: the single "document" node
// spans the whole file, Source is preserved, and a splice re-parses losslessly.
func TestParseTextFallback(t *testing.T) {
	a := New()
	cases := []struct{ name, src, old, new string }{
		{"markdown", "# Title\n\nSome **bold** text and a [link](http://x).\n", "bold", "strong"},
		{"mdx", "import X from 'x'\n\n# Title\n\n<X/>\n", "Title", "Heading"},
		{"dockerfile", "FROM golang:1.24\nWORKDIR /app\nCOPY . .\nRUN go build ./...\nCMD [\"/app/bin\"]\n", "1.24", "1.25"},
		{"scss", "$color: red;\n.btn { color: $color; }\n", "red", "blue"},
		{"txt", "Just some plain text.\nSecond line.\n", "plain", "simple"},
		{"empty", "", "", ""},
		// adversarial bytes an agent IDE will hit in the wild
		{"unicode", "café ☕ 日本語 — emoji 😀\nsecond\n", "café", "tea"},
		{"crlf", "line1\r\nline2\r\nline3\r\n", "line2", "lineB"},
		{"no-trailing-newline", "last line has no newline", "newline", "EOL"},
		{"only-newlines", "\n\n\n", "", ""},
		{"leading-bom", "\xEF\xBB\xBF# Title\n", "Title", "Heading"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			src := []byte(tc.src)
			tree, err := a.Parse(context.Background(), parser.LangText, src)
			if err != nil {
				t.Fatalf("Parse text %s: %v", tc.name, err)
			}
			defer tree.Close()

			if tree.Root == nil || tree.Root.Kind != "document" {
				t.Fatalf("%s: root = %v, want document", tc.name, tree.Root)
			}
			if tree.Root.StartByte != 0 || tree.Root.EndByte != len(src) {
				t.Fatalf("%s: root span [%d:%d], want [0:%d]", tc.name, tree.Root.StartByte, tree.Root.EndByte, len(src))
			}
			if string(tree.Source) != tc.src {
				t.Fatalf("%s: Source not preserved", tc.name)
			}
			if got := string(src[tree.Root.StartByte:tree.Root.EndByte]); got != tc.src {
				t.Fatalf("%s: round-trip text mismatch", tc.name)
			}

			if tc.old == "" {
				return // empty / no-splice edge case
			}
			idx := strings.Index(tc.src, tc.old)
			if idx < 0 {
				t.Fatalf("%s: test bug, old %q not in src", tc.name, tc.old)
			}
			spliced := tc.src[:idx] + tc.new + tc.src[idx+len(tc.old):]
			tree2, err := a.Parse(context.Background(), parser.LangText, []byte(spliced))
			if err != nil {
				t.Fatalf("%s: re-parse after splice: %v", tc.name, err)
			}
			defer tree2.Close()
			if tree2.Root.EndByte != len(spliced) {
				t.Fatalf("%s: spliced root end %d, want %d", tc.name, tree2.Root.EndByte, len(spliced))
			}
			if string(tree2.Source) != spliced {
				t.Fatalf("%s: spliced Source not preserved", tc.name)
			}
		})
	}
}

// FuzzTextFallbackLossless asserts the agent-IDE core guarantee: the grammar-
// free text fallback round-trips ANY bytes exactly — the document node spans the
// whole input and Source is preserved byte-for-byte, including binary, multibyte
// UTF-8, emoji, lone CRs, and NUL bytes. If this ever fails, an agent could
// corrupt a file Båge claims it can edit.
func FuzzTextFallbackLossless(f *testing.F) {
	a := New()
	for _, s := range []string{
		"", "x", "a\nb\n", "\r\n\r\n", "emoji 😀 café 日本語\n",
		"no final newline", "\x00\x01binary\xff", "   \t  ", "\xEF\xBB\xBFbom",
	} {
		f.Add([]byte(s))
	}
	f.Fuzz(func(t *testing.T, src []byte) {
		tree, err := a.Parse(context.Background(), parser.LangText, src)
		if err != nil {
			t.Fatalf("Parse text: %v", err)
		}
		defer tree.Close()
		if !bytes.Equal(tree.Source, src) {
			t.Fatalf("text fallback corrupted source: got %q want %q", tree.Source, src)
		}
		if tree.Root == nil || tree.Root.StartByte != 0 || tree.Root.EndByte != len(src) {
			t.Fatalf("text fallback root span wrong for %q: %+v", src, tree.Root)
		}
	})
}

// TestTextLineChildren asserts the line-granular structure of the grammar-free
// text fallback: the document node carries one "line" child per source line,
// concatenating children reproduces the source exactly, children tile [0:len)
// contiguously, and line points obey tree-sitter semantics (tied to the whole-
// file textEndPoint). Covers empty, no-final-newline, only-newlines, CRLF,
// unicode, and BOM edges.
func TestTextLineChildren(t *testing.T) {
	a := New()
	cases := []struct {
		name      string
		src       string
		wantLines int
	}{
		{"empty", "", 0},
		{"one-no-newline", "abc", 1},
		{"one-with-newline", "abc\n", 1},
		{"two-no-trailing-newline", "a\nb", 2},
		{"two-trailing-newline", "a\nb\n", 2},
		{"only-newlines", "\n\n\n", 3},
		{"crlf", "a\r\nb\r\n", 2},
		{"unicode", "café ☕\n日本語\n", 2},
		{"leading-bom", "\xEF\xBB\xBF# Title\n", 1},
		{"blank-then-text", "\nx", 2},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			src := []byte(tc.src)
			tree, err := a.Parse(context.Background(), parser.LangText, src)
			if err != nil {
				t.Fatalf("Parse text %s: %v", tc.name, err)
			}
			defer tree.Close()

			children := tree.Root.Children
			if len(children) != tc.wantLines {
				t.Fatalf("%s: got %d line children, want %d", tc.name, len(children), tc.wantLines)
			}
			if len(children) == 0 {
				if len(src) != 0 {
					t.Fatalf("%s: no children but src is non-empty", tc.name)
				}
				return
			}

			var concat []byte
			for i, c := range children {
				if c.Kind != "line" {
					t.Fatalf("%s: child %d kind = %q, want line", tc.name, i, c.Kind)
				}
				if !c.Named {
					t.Fatalf("%s: child %d not Named", tc.name, i)
				}
				if c.StartByte < 0 || c.EndByte > len(src) || c.StartByte >= c.EndByte {
					t.Fatalf("%s: child %d span [%d:%d] out of range for len %d", tc.name, i, c.StartByte, c.EndByte, len(src))
				}
				if i > 0 && children[i-1].EndByte != c.StartByte {
					t.Fatalf("%s: child %d gap/overlap: prev end %d != start %d", tc.name, i, children[i-1].EndByte, c.StartByte)
				}
				concat = append(concat, src[c.StartByte:c.EndByte]...)
			}
			if !bytes.Equal(concat, src) {
				t.Fatalf("%s: child concatenation mismatch: got %q want %q", tc.name, concat, src)
			}
			if children[0].StartByte != 0 {
				t.Fatalf("%s: first child start %d, want 0", tc.name, children[0].StartByte)
			}
			last := children[len(children)-1]
			if last.EndByte != len(src) {
				t.Fatalf("%s: last child end %d, want %d", tc.name, last.EndByte, len(src))
			}
			if children[0].StartPoint != (parser.Point{Row: 0, Col: 0}) {
				t.Fatalf("%s: first child start point %+v, want {0,0}", tc.name, children[0].StartPoint)
			}
			if last.EndPoint != textEndPoint(src) {
				t.Fatalf("%s: last child end point %+v, want %+v", tc.name, last.EndPoint, textEndPoint(src))
			}
			for i, c := range children {
				endsWithNewline := src[c.EndByte-1] == '\n'
				if endsWithNewline {
					if c.EndPoint.Col != 0 || c.EndPoint.Row != c.StartPoint.Row+1 {
						t.Fatalf("%s: child %d newline-terminated end point %+v, want {%d,0}", tc.name, i, c.EndPoint, c.StartPoint.Row+1)
					}
				} else {
					if c.EndPoint.Row != c.StartPoint.Row || c.EndPoint.Col != c.EndByte-c.StartByte {
						t.Fatalf("%s: child %d trailing end point %+v, want {%d,%d}", tc.name, i, c.EndPoint, c.StartPoint.Row, c.EndByte-c.StartByte)
					}
				}
			}
		})
	}
}

func TestParseUnsupportedLang(t *testing.T) {
	a := New()
	_, err := a.Parse(context.Background(), parser.LangUnknown, []byte("x"))
	if !errors.Is(err, ErrUnsupportedLang) {
		t.Fatalf("Parse(LangUnknown) error = %v, want ErrUnsupportedLang", err)
	}
}

func TestParseIncremental(t *testing.T) {
	a := New()
	ctx := context.Background()
	src1 := "package main\n\nfunc main() {}\n"
	// Append a new top-level declaration — a structural change, so the reparse
	// adds a node and ChangedRanges reports it (a same-structure rename would
	// legitimately yield no changed ranges).
	src2 := src1 + "var z = 1\n"
	insertAt := len(src1)

	tree1, err := a.Parse(ctx, parser.LangGo, []byte(src1))
	if err != nil {
		t.Fatalf("Parse src1: %v", err)
	}
	defer tree1.Close()

	edit := parser.InputEdit{
		StartByte:   insertAt,
		OldEndByte:  insertAt,
		NewEndByte:  len(src2),
		StartPoint:  parser.Point{Row: 3, Col: 0},
		OldEndPoint: parser.Point{Row: 3, Col: 0},
		NewEndPoint: parser.Point{Row: 4, Col: 0},
	}

	tree2, err := a.ParseIncremental(ctx, parser.LangGo, []byte(src2), tree1, edit)
	if err != nil {
		t.Fatalf("ParseIncremental: %v", err)
	}
	defer tree2.Close()

	if tree2.Root == nil || tree2.Root.Kind != "source_file" {
		t.Fatalf("incremental root kind = %v, want source_file", tree2.Root)
	}
	if string(tree2.Source) != src2 {
		t.Fatalf("incremental Source not preserved")
	}
	if findKind(tree2.Root, "function_declaration") == nil {
		t.Fatalf("incremental tree missing function_declaration")
	}
	if findKind(tree2.Root, "var_declaration") == nil {
		t.Fatalf("incremental tree missing the appended var_declaration")
	}

	ranges := a.ChangedRanges(tree1, tree2)
	if len(ranges) == 0 {
		t.Fatalf("ChangedRanges returned none, want the appended region")
	}
	for _, r := range ranges {
		if r.End < r.Start || r.End > len(src2) {
			t.Fatalf("changed range invalid: [%d:%d]", r.Start, r.End)
		}
	}
}

func TestTreeCloseTwiceSafe(t *testing.T) {
	a := New()
	tree, err := a.Parse(context.Background(), parser.LangGo, []byte("package main\n"))
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	tree.Close()
	tree.Close() // must not double-free the native C tree
}

func TestParseContextCancelled(t *testing.T) {
	a := New()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := a.Parse(ctx, parser.LangGo, []byte("package main\n")); err == nil {
		t.Fatalf("Parse with cancelled context: want error, got nil")
	}
}

func TestParseIncrementalNoNativeHandle(t *testing.T) {
	a := New()
	old := &parser.Tree{} // Native nil — e.g. an engine-free fake tree
	_, err := a.ParseIncremental(context.Background(), parser.LangGo, []byte("package main\n"), old, parser.InputEdit{})
	if err == nil {
		t.Fatalf("ParseIncremental with no native handle: want error, got nil")
	}
}

func TestParseIncrementalNegativeEdit(t *testing.T) {
	a := New()
	ctx := context.Background()
	tree, err := a.Parse(ctx, parser.LangGo, []byte("package main\n"))
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	defer tree.Close()
	bad := parser.InputEdit{StartByte: -1}
	if _, err := a.ParseIncremental(ctx, parser.LangGo, []byte("package main\n"), tree, bad); err == nil {
		t.Fatalf("ParseIncremental with negative offset: want error, got nil")
	}
}

func TestConvertIncludesAnonymousTokens(t *testing.T) {
	a := New()
	tree, err := a.Parse(context.Background(), parser.LangGo, []byte("package main\n\nfunc main() {}\n"))
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	defer tree.Close()
	// "func", "(", "{" etc. are anonymous tokens; convert must materialize them.
	if !hasAnonymous(tree.Root) {
		t.Fatalf("no anonymous (Named==false) token materialized")
	}
}

// hasAnonymous reports whether the subtree contains an anonymous (unnamed) node.
func hasAnonymous(n *parser.Node) bool {
	if n == nil {
		return false
	}
	if !n.Named {
		return true
	}
	for _, c := range n.Children {
		if hasAnonymous(c) {
			return true
		}
	}
	return false
}
