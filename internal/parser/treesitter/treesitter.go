// Package treesitter implements parser.ParserPort with the official CGO
// go-tree-sitter bindings (docs/adr/0002). It parses source into Båge's
// engine-agnostic CST DTOs, retaining the native tree on parser.Tree.Native so
// incremental reparsing and changed-range queries can reuse it.
//
// Lifecycle: every returned *parser.Tree owns a native tree; the caller MUST
// call Tree.Close to free the underlying C memory. Parsers are created and
// closed per call.
package treesitter

import (
	"context"
	"errors"
	"fmt"

	tsmake "github.com/tree-sitter-grammars/tree-sitter-make/bindings/go"
	tsmd "github.com/tree-sitter-grammars/tree-sitter-markdown/bindings/go"
	tstoml "github.com/tree-sitter-grammars/tree-sitter-toml/bindings/go"
	tsxml "github.com/tree-sitter-grammars/tree-sitter-xml/bindings/go"
	tsyaml "github.com/tree-sitter-grammars/tree-sitter-yaml/bindings/go"
	ts "github.com/tree-sitter/go-tree-sitter"
	tsbash "github.com/tree-sitter/tree-sitter-bash/bindings/go"
	tscsharp "github.com/tree-sitter/tree-sitter-c-sharp/bindings/go"
	tsc "github.com/tree-sitter/tree-sitter-c/bindings/go"
	tscpp "github.com/tree-sitter/tree-sitter-cpp/bindings/go"
	tscss "github.com/tree-sitter/tree-sitter-css/bindings/go"
	tsgo "github.com/tree-sitter/tree-sitter-go/bindings/go"
	tshtml "github.com/tree-sitter/tree-sitter-html/bindings/go"
	tsjava "github.com/tree-sitter/tree-sitter-java/bindings/go"
	tsjs "github.com/tree-sitter/tree-sitter-javascript/bindings/go"
	tsjson "github.com/tree-sitter/tree-sitter-json/bindings/go"
	tspython "github.com/tree-sitter/tree-sitter-python/bindings/go"
	tsruby "github.com/tree-sitter/tree-sitter-ruby/bindings/go"
	tsrust "github.com/tree-sitter/tree-sitter-rust/bindings/go"
	tsts "github.com/tree-sitter/tree-sitter-typescript/bindings/go"

	"github.com/hylla-io/bage/internal/parser"
)

// ErrUnsupportedLang is returned when no grammar is registered for a language.
var ErrUnsupportedLang = errors.New("treesitter: unsupported language")

// Adapter is a parser.ParserPort backed by go-tree-sitter. It caches one
// *ts.Language per supported language for the process lifetime. The zero value
// is not usable; construct it with New.
type Adapter struct {
	langs map[parser.Lang]*ts.Language
}

// New constructs an Adapter with every resolved grammar registered, making the
// adapter polyglot. TypeScript and TSX both come from the tree-sitter-typescript
// binding via its two distinct exported language functions.
func New() *Adapter {
	return &Adapter{
		langs: map[parser.Lang]*ts.Language{
			parser.LangGo:         ts.NewLanguage(tsgo.Language()),
			parser.LangTypeScript: ts.NewLanguage(tsts.LanguageTypescript()),
			parser.LangTSX:        ts.NewLanguage(tsts.LanguageTSX()),
			parser.LangJavaScript: ts.NewLanguage(tsjs.Language()),
			parser.LangPython:     ts.NewLanguage(tspython.Language()),
			parser.LangRust:       ts.NewLanguage(tsrust.Language()),
			parser.LangJava:       ts.NewLanguage(tsjava.Language()),
			parser.LangC:          ts.NewLanguage(tsc.Language()),
			parser.LangCPP:        ts.NewLanguage(tscpp.Language()),
			parser.LangCSharp:     ts.NewLanguage(tscsharp.Language()),
			parser.LangRuby:       ts.NewLanguage(tsruby.Language()),
			parser.LangJSON:       ts.NewLanguage(tsjson.Language()),
			parser.LangHTML:       ts.NewLanguage(tshtml.Language()),
			parser.LangCSS:        ts.NewLanguage(tscss.Language()),
			parser.LangYAML:       ts.NewLanguage(tsyaml.Language()),
			parser.LangTOML:       ts.NewLanguage(tstoml.Language()),
			parser.LangXML:        ts.NewLanguage(tsxml.LanguageXML()),
			parser.LangMakefile:   ts.NewLanguage(tsmake.Language()),
			parser.LangBash:       ts.NewLanguage(tsbash.Language()),
			parser.LangMarkdown:   ts.NewLanguage(tsmd.Language()),
		},
	}
}

// language returns the cached grammar for lang or ErrUnsupportedLang.
func (a *Adapter) language(lang parser.Lang) (*ts.Language, error) {
	l, ok := a.langs[lang]
	if !ok {
		return nil, fmt.Errorf("%w: %s", ErrUnsupportedLang, lang)
	}
	return l, nil
}

// Parse parses src under lang into a fresh Tree whose Native holds the engine
// tree (close it via Tree.Close).
func (a *Adapter) Parse(ctx context.Context, lang parser.Lang, src []byte) (*parser.Tree, error) {
	if lang == parser.LangText {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		return textTree(src), nil
	}
	l, err := a.language(lang)
	if err != nil {
		return nil, err
	}
	p := ts.NewParser()
	defer p.Close()
	if err := p.SetLanguage(l); err != nil {
		return nil, fmt.Errorf("treesitter: set language %s: %w", lang, err)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	tree := p.Parse(src, nil)
	if tree == nil {
		return nil, fmt.Errorf("treesitter: parse %s returned no tree", lang)
	}
	return &parser.Tree{Root: convert(tree.RootNode()), Source: src, Native: tree}, nil
}

// ParseIncremental applies edit to old's native tree and reparses src, reusing
// unchanged subtrees. old must carry a native tree from this adapter.
func (a *Adapter) ParseIncremental(ctx context.Context, lang parser.Lang, src []byte, old *parser.Tree, edit parser.InputEdit) (*parser.Tree, error) {
	if lang == parser.LangText {
		// No native tree to reuse; a fresh whole-file node is correct (just not
		// incrementally optimized) for grammar-free text.
		if err := validateEdit(edit); err != nil {
			return nil, err
		}
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		return textTree(src), nil
	}
	l, err := a.language(lang)
	if err != nil {
		return nil, err
	}
	if err := validateEdit(edit); err != nil {
		return nil, err
	}
	oldTS, ok := nativeTree(old)
	if !ok {
		return nil, errors.New("treesitter: old tree has no native handle")
	}
	e := toTSInputEdit(edit)
	oldTS.Edit(&e)

	p := ts.NewParser()
	defer p.Close()
	if err := p.SetLanguage(l); err != nil {
		return nil, fmt.Errorf("treesitter: set language %s: %w", lang, err)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	tree := p.Parse(src, oldTS)
	if tree == nil {
		return nil, fmt.Errorf("treesitter: incremental parse %s returned no tree", lang)
	}
	return &parser.Tree{Root: convert(tree.RootNode()), Source: src, Native: tree}, nil
}

// ChangedRanges reports the byte ranges that differ between old and new. Both
// must carry native trees from this adapter; otherwise it returns nil.
func (a *Adapter) ChangedRanges(old, new *parser.Tree) []parser.ByteRange {
	oldTS, ok1 := nativeTree(old)
	newTS, ok2 := nativeTree(new)
	if !ok1 || !ok2 {
		return nil
	}
	tsRanges := oldTS.ChangedRanges(newTS)
	if len(tsRanges) == 0 {
		return nil
	}
	out := make([]parser.ByteRange, 0, len(tsRanges))
	for _, r := range tsRanges {
		out = append(out, parser.ByteRange{Start: int(r.StartByte), End: int(r.EndByte)})
	}
	return out
}

// textTree builds a grammar-free tree: a named "document" node spanning the
// whole source, with one named "line" child per source line. Each line node
// includes its terminating '\n' (the newline is kept WITH the line it ends), so
// concatenating the children's byte ranges reproduces the source EXACTLY — gap-
// free and overlap-free. This gives coarse-but-useful, byte-anchorable nodes for
// any file type with no registered grammar (MDX, SCSS, Dockerfile, .txt, config
// files, …) while still round-tripping losslessly. An empty file
// yields a document node with no children. Native is nil, so Tree.Close is a
// no-op and ChangedRanges degrades to nil (callers fall back to a full reparse)
// — correct, not optimized.
func textTree(src []byte) *parser.Tree {
	root := &parser.Node{
		Kind:       "document",
		StartByte:  0,
		EndByte:    len(src),
		StartPoint: parser.Point{Row: 0, Col: 0},
		EndPoint:   textEndPoint(src),
		Named:      true,
		Children:   textLines(src),
	}
	return &parser.Tree{Root: root, Source: src, Native: nil}
}

// textLines splits src into one "line" node per source line. A line spans from
// its first byte through and INCLUDING its terminating '\n'; trailing content
// with no final newline becomes a final line node without one. A source that
// ends on '\n' (or is empty) produces NO phantom empty trailing line. The
// returned nodes partition [0:len(src)) contiguously, so concatenating their
// byte ranges is lossless. Points follow tree-sitter semantics: line k starts at
// {Row:k, Col:0}; a newline-terminated line ends at the next row {Row:k+1, Col:0},
// and a final newline-less line ends at {Row:k, Col:byteLen}. The last line's
// EndPoint therefore equals textEndPoint(src) for the whole file.
func textLines(src []byte) []*parser.Node {
	if len(src) == 0 {
		return nil
	}
	var lines []*parser.Node
	lineStart := 0
	row := 0
	for i, b := range src {
		if b != '\n' {
			continue
		}
		lines = append(lines, &parser.Node{
			Kind:       "line",
			StartByte:  lineStart,
			EndByte:    i + 1,
			StartPoint: parser.Point{Row: row, Col: 0},
			EndPoint:   parser.Point{Row: row + 1, Col: 0},
			Named:      true,
		})
		lineStart = i + 1
		row++
	}
	if lineStart < len(src) {
		lines = append(lines, &parser.Node{
			Kind:       "line",
			StartByte:  lineStart,
			EndByte:    len(src),
			StartPoint: parser.Point{Row: row, Col: 0},
			EndPoint:   parser.Point{Row: row, Col: len(src) - lineStart},
			Named:      true,
		})
	}
	return lines
}

// textEndPoint returns the row/col one past the last byte of src, matching
// tree-sitter point semantics (row = newline count, col = bytes after the last
// newline).
func textEndPoint(src []byte) parser.Point {
	row, col := 0, 0
	for _, b := range src {
		if b == '\n' {
			row++
			col = 0
		} else {
			col++
		}
	}
	return parser.Point{Row: row, Col: col}
}

// nativeTree extracts the *ts.Tree stored on a parser.Tree, if present.
func nativeTree(t *parser.Tree) (*ts.Tree, bool) {
	if t == nil {
		return nil, false
	}
	n, ok := t.Native.(*ts.Tree)
	return n, ok
}

// convert eagerly materializes a native node and its descendants into Båge's
// engine-independent Node DTO so the result outlives the native tree. It walks
// with an explicit stack rather than recursion, so a deeply nested tree cannot
// exhaust the goroutine stack.
func convert(root *ts.Node) *parser.Node {
	if root == nil {
		return nil
	}
	out := newNode(root)
	type frame struct {
		ts *ts.Node
		pn *parser.Node
	}
	stack := []frame{{root, out}}
	for len(stack) > 0 {
		f := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		count := f.ts.ChildCount()
		if count == 0 {
			continue
		}
		f.pn.Children = make([]*parser.Node, count)
		for i := range count {
			child := f.ts.Child(i)
			cn := newNode(child)
			f.pn.Children[i] = cn
			stack = append(stack, frame{child, cn})
		}
	}
	return out
}

// newNode builds a childless Node DTO from a native node.
func newNode(n *ts.Node) *parser.Node {
	return &parser.Node{
		Kind:       n.Kind(),
		StartByte:  int(n.StartByte()),
		EndByte:    int(n.EndByte()),
		StartPoint: toPoint(n.StartPosition()),
		EndPoint:   toPoint(n.EndPosition()),
		Named:      n.IsNamed(),
	}
}

// toPoint converts a native point to the DTO point.
func toPoint(p ts.Point) parser.Point {
	return parser.Point{Row: int(p.Row), Col: int(p.Column)}
}

// validateEdit rejects an InputEdit with any negative field. The native edit
// uses uint, so a negative int would wrap to a huge value and drive an
// out-of-range edit into the C reparse.
func validateEdit(e parser.InputEdit) error {
	if e.StartByte < 0 || e.OldEndByte < 0 || e.NewEndByte < 0 ||
		e.StartPoint.Row < 0 || e.StartPoint.Col < 0 ||
		e.OldEndPoint.Row < 0 || e.OldEndPoint.Col < 0 ||
		e.NewEndPoint.Row < 0 || e.NewEndPoint.Col < 0 {
		return fmt.Errorf("treesitter: invalid InputEdit with negative offset: %+v", e)
	}
	return nil
}

// toTSInputEdit converts the DTO edit into the native edit shape.
func toTSInputEdit(e parser.InputEdit) ts.InputEdit {
	return ts.InputEdit{
		StartByte:      uint(e.StartByte),
		OldEndByte:     uint(e.OldEndByte),
		NewEndByte:     uint(e.NewEndByte),
		StartPosition:  ts.Point{Row: uint(e.StartPoint.Row), Column: uint(e.StartPoint.Col)},
		OldEndPosition: ts.Point{Row: uint(e.OldEndPoint.Row), Column: uint(e.OldEndPoint.Col)},
		NewEndPosition: ts.Point{Row: uint(e.NewEndPoint.Row), Column: uint(e.NewEndPoint.Col)},
	}
}

// Compile-time assertion that Adapter implements the port.
var _ parser.ParserPort = (*Adapter)(nil)
