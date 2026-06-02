package parser

import (
	"context"
	"errors"
	"testing"
)

func TestLangForPath(t *testing.T) {
	tests := []struct {
		path string
		want Lang
	}{
		// programming languages + their extension variants
		{"main.go", LangGo},
		{"a/b/c/server.go", LangGo},
		{"x.ts", LangTypeScript},
		{"x.mts", LangTypeScript},
		{"app.tsx", LangTSX},
		{"app.js", LangJavaScript},
		{"app.jsx", LangJavaScript},
		{"app.mjs", LangJavaScript},
		{"app.cjs", LangJavaScript},
		{"m.py", LangPython},
		{"m.rs", LangRust},
		{"M.java", LangJava},
		{"u.c", LangC},
		{"u.h", LangC},
		{"u.cpp", LangCPP},
		{"u.hpp", LangCPP},
		{"P.cs", LangCSharp},
		{"s.rb", LangRuby},
		// data / markup / config with real grammars
		{"pkg.json", LangJSON},
		{"index.html", LangHTML},
		{"a.css", LangCSS},
		{"docker-compose.yaml", LangYAML},
		{"config.yml", LangYAML},
		{"Cargo.toml", LangTOML},
		{"pom.xml", LangXML},
		{"deploy.sh", LangBash},
		{"build.bash", LangBash},
		// extensionless build files by basename
		{"Makefile", LangMakefile},
		{"src/Makefile", LangMakefile},
		{"GNUmakefile", LangMakefile},
		{"rules.mk", LangMakefile},
		{"Dockerfile", LangText},
		{"deploy/Dockerfile", LangText},
		{"justfile", LangText},
		// markdown has a real grammar; its variants/relatives fall back to text
		{"README.md", LangMarkdown},
		{"docs/guide.markdown", LangMarkdown},
		{"page.mdx", LangText},
		{"styles.scss", LangText},
		{"notes.txt", LangText},
		// edge cases
		{"x.TS", LangTypeScript},           // case-insensitive extension
		{"a.b.c.go", LangGo},               // multiple dots
		{".gitignore", LangText},           // dotfile: no real extension
		{".env", LangText},                 // dotfile
		{"noextension", LangText},          // no extension at all
		{"", LangText},                     // empty path
		{"weird.unknownext", LangText},     // unknown extension
		{"C:\\win\\path\\main.go", LangGo}, // backslash separators
	}
	for _, tc := range tests {
		t.Run(tc.path, func(t *testing.T) {
			if got := LangForPath(tc.path); got != tc.want {
				t.Fatalf("LangForPath(%q) = %v, want %v", tc.path, got, tc.want)
			}
		})
	}
	// Invariant: LangForPath NEVER returns LangUnknown — every file is at least text.
	for _, p := range []string{"", "x", "x.zzz", "..", "a/b/"} {
		if LangForPath(p) == LangUnknown {
			t.Fatalf("LangForPath(%q) returned LangUnknown; must fall back to LangText", p)
		}
	}
}

func TestLangString(t *testing.T) {
	tests := []struct {
		name string
		lang Lang
		want string
	}{
		{name: "go", lang: LangGo, want: "go"},
		{name: "unknown explicit", lang: LangUnknown, want: "unknown"},
		{name: "zero value", lang: Lang(0), want: "unknown"},
		{name: "out of range", lang: Lang(99), want: "unknown"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.lang.String(); got != tt.want {
				t.Errorf("Lang(%d).String() = %q, want %q", tt.lang, got, tt.want)
			}
		})
	}
}

func TestZeroValues(t *testing.T) {
	var p Point
	if p.Row != 0 || p.Col != 0 {
		t.Errorf("zero Point = %+v, want {Row:0 Col:0}", p)
	}

	var br ByteRange
	if br.Start != 0 || br.End != 0 {
		t.Errorf("zero ByteRange = %+v, want {Start:0 End:0}", br)
	}

	var n Node
	if n.Kind != "" || n.StartByte != 0 || n.EndByte != 0 || n.Named || n.Children != nil {
		t.Errorf("zero Node not empty: %+v", n)
	}

	var tr Tree
	if tr.Root != nil || tr.Source != nil {
		t.Errorf("zero Tree not empty: %+v", tr)
	}

	var ie InputEdit
	if ie.StartByte != 0 || ie.OldEndByte != 0 || ie.NewEndByte != 0 {
		t.Errorf("zero InputEdit not empty: %+v", ie)
	}
}

func TestNodeChildrenAndPoints(t *testing.T) {
	child := &Node{Kind: "identifier", StartByte: 5, EndByte: 8, Named: true}
	root := &Node{
		Kind:       "source_file",
		StartByte:  0,
		EndByte:    8,
		StartPoint: Point{Row: 0, Col: 0},
		EndPoint:   Point{Row: 0, Col: 8},
		Named:      true,
		Children:   []*Node{child},
	}
	if len(root.Children) != 1 || root.Children[0] != child {
		t.Fatalf("Children wiring wrong: %+v", root.Children)
	}
	if root.EndPoint != (Point{Row: 0, Col: 8}) {
		t.Errorf("EndPoint = %+v, want {0 8}", root.EndPoint)
	}
}

// fakeParser is a minimal in-test implementation proving ParserPort is
// satisfiable and usable without a real engine.
type fakeParser struct {
	err error
}

func (f fakeParser) Parse(_ context.Context, _ Lang, src []byte) (*Tree, error) {
	if f.err != nil {
		return nil, f.err
	}
	return &Tree{
		Root:   &Node{Kind: "source_file", StartByte: 0, EndByte: len(src), Named: true},
		Source: src,
	}, nil
}

func (f fakeParser) ParseIncremental(ctx context.Context, lang Lang, src []byte, _ *Tree, _ InputEdit) (*Tree, error) {
	return f.Parse(ctx, lang, src)
}

func (f fakeParser) ChangedRanges(_, _ *Tree) []ByteRange {
	return []ByteRange{{Start: 0, End: 1}}
}

func TestParserPortSatisfiable(t *testing.T) {
	var p ParserPort = fakeParser{}

	src := []byte("package main")
	tree, err := p.Parse(context.Background(), LangGo, src)
	if err != nil {
		t.Fatalf("Parse returned error: %v", err)
	}
	if tree.Root.EndByte != len(src) {
		t.Errorf("Root.EndByte = %d, want %d", tree.Root.EndByte, len(src))
	}

	inc, err := p.ParseIncremental(context.Background(), LangGo, src, tree, InputEdit{NewEndByte: len(src)})
	if err != nil {
		t.Fatalf("ParseIncremental returned error: %v", err)
	}
	if inc.Root == nil {
		t.Error("ParseIncremental returned nil Root")
	}

	if ranges := p.ChangedRanges(tree, inc); len(ranges) != 1 || ranges[0] != (ByteRange{Start: 0, End: 1}) {
		t.Errorf("ChangedRanges = %+v, want [{0 1}]", ranges)
	}
}

func TestParserPortErrorPropagation(t *testing.T) {
	sentinel := errors.New("parse failed")
	var p ParserPort = fakeParser{err: sentinel}

	if _, err := p.Parse(context.Background(), LangGo, nil); !errors.Is(err, sentinel) {
		t.Errorf("Parse error = %v, want %v", err, sentinel)
	}
}
