package bage

import (
	"context"
	"fmt"
	"os"
	"strings"

	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
)

// OpenedFile is a freshly parsed file handle: the path, the selected language,
// and the concrete syntax tree. It is the read-only convenience an agent IDE
// uses to inspect a file without opening a full Editor. The caller MUST Close it
// when done so the adapter can free the native tree.
type OpenedFile struct {
	// Path is the file path that was opened (as supplied by the caller).
	Path string
	// Lang is the language selected for Path via LangForPath; never LangUnknown.
	Lang Lang
	// Tree is the parsed CST together with the source bytes it was parsed from.
	Tree *Tree
}

// Close releases the native resources held by the opened tree. It is nil-safe
// and idempotent (parser.Tree.Close is idempotent), so calling it twice or on a
// zero OpenedFile is a no-op.
func (o *OpenedFile) Close() {
	if o == nil || o.Tree == nil {
		return
	}
	o.Tree.Close()
}

// OpenFile reads path, selects a language with LangForPath (falling back to the
// grammar-free text mode for any type without a registered grammar, so ANY file
// opens), and parses it with the same tree-sitter adapter Båge edits with. The
// returned OpenedFile must be Closed by the caller.
func OpenFile(ctx context.Context, path string) (*OpenedFile, error) {
	src, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("bage: open file %q: %w", path, err)
	}
	lang := parser.LangForPath(path)
	tree, err := treesitter.New().Parse(ctx, lang, src)
	if err != nil {
		return nil, fmt.Errorf("bage: parse %q (%s): %w", path, lang, err)
	}
	return &OpenedFile{Path: path, Lang: lang, Tree: tree}, nil
}

// Symbol is one entry in a file's Outline: a named declaration node (or, for the
// grammar-free text fallback, a single line). Bytes are the half-open CST range;
// StartLine and EndLine are 1-based to match EditResult line numbering. Name is
// best-effort and may be empty when no identifier child is found.
type Symbol struct {
	// Kind is the grammar node kind (e.g. "function_declaration"), or "line" for
	// the text fallback.
	Kind string
	// Name is the declared identifier, best-effort; "" when none was found.
	Name string
	// StartByte is the inclusive start byte offset of the node.
	StartByte int
	// EndByte is the exclusive end byte offset of the node.
	EndByte int
	// StartLine is the 1-based start line of the node.
	StartLine int
	// EndLine is the 1-based end line of the node.
	EndLine int
}

// Outline returns a documentSymbol-like listing of a parsed tree: every named
// declaration node, in source order, with its byte and line ranges. It is
// grammar-agnostic — it selects declaration nodes by named-node kind, so it works
// for any tree-sitter grammar. For the grammar-free text fallback it returns one
// Symbol per source line instead.
//
// The text fallback is identified by a nil native tree (treesitter.textTree sets
// Tree.Native == nil), NOT by child count: the text document now carries line
// children, and some real grammars (e.g. HTML) also use a "document" root — so
// the engine-free handle is the unambiguous discriminator.
func Outline(tree *Tree) []Symbol {
	if tree == nil || tree.Root == nil {
		return nil
	}
	if tree.Native == nil {
		return outlineLines(tree.Source)
	}
	var out []Symbol
	walkDecls(tree.Root, tree.Source, &out)
	return out
}

// walkDecls recursively appends a Symbol for every named declaration-kind node
// under n, in source order. It always recurses (even into a matched declaration)
// so methods nested in a class/impl/struct body are captured. The root itself is
// never a declaration kind, so it is never emitted.
func walkDecls(n *Node, src []byte, out *[]Symbol) {
	for _, c := range n.Children {
		if c == nil {
			continue
		}
		if c.Named && isDeclKind(c.Kind) {
			*out = append(*out, Symbol{
				Kind:      c.Kind,
				Name:      declName(c, src),
				StartByte: c.StartByte,
				EndByte:   c.EndByte,
				StartLine: c.StartPoint.Row + 1,
				EndLine:   c.EndPoint.Row + 1,
			})
		}
		walkDecls(c, src, out)
	}
}

// declKindSubstrings are the substrings whose presence in a node kind marks it a
// declaration across the supported grammars (Go/TS/TSX/JS/JSX/Python/Rust/C/C++).
var declKindSubstrings = []string{
	"declaration", "definition", "function", "method",
	"class", "struct", "interface", "impl", "enum",
	"trait", "module", "namespace",
}

// isDeclKind reports whether a node kind names a declaration. It matches Rust's
// "*_item" kinds (function_item, struct_item, …) and the cross-grammar substring
// set above — painfully simple and grammar-table-free. It first excludes obvious
// sub-parts that are not outline-worthy: parameters, list containers (e.g.
// field_declaration_list), and bare type expressions (Go struct_type /
// interface_type), so the outline lists declarations, not their innards.
func isDeclKind(kind string) bool {
	if strings.Contains(kind, "parameter") ||
		strings.HasSuffix(kind, "_list") ||
		strings.HasSuffix(kind, "_type") {
		return false
	}
	if strings.HasSuffix(kind, "_item") {
		return true
	}
	for _, sub := range declKindSubstrings {
		if strings.Contains(kind, sub) {
			return true
		}
	}
	return false
}

// isNameKind reports whether a direct-child node kind carries a declaration's
// name across the supported grammars.
func isNameKind(kind string) bool {
	switch kind {
	case "name", "field_identifier", "type_identifier", "property_identifier":
		return true
	}
	return strings.Contains(kind, "identifier")
}

// declName returns the declared identifier text for n, best-effort. It first
// looks at n's direct named children, then — since some grammars wrap the name
// one level down (Go type_declaration → type_spec → type_identifier, C
// declaration → declarator → identifier) — at the direct named children of n's
// named children. It stays shallow (≤2 levels) so it never grabs an identifier
// from a function body. Returns "" when none is found; slice bounds are guarded.
func declName(n *Node, src []byte) string {
	if name := directName(n, src); name != "" {
		return name
	}
	for _, c := range n.Children {
		if c == nil || !c.Named {
			continue
		}
		if name := directName(c, src); name != "" {
			return name
		}
	}
	return ""
}

// directName returns the text of n's first direct named identifier-kind child,
// or "" if none. Slice bounds are guarded.
func directName(n *Node, src []byte) string {
	for _, c := range n.Children {
		if c == nil || !c.Named || !isNameKind(c.Kind) {
			continue
		}
		if c.StartByte < 0 || c.EndByte < c.StartByte || c.EndByte > len(src) {
			continue
		}
		return string(src[c.StartByte:c.EndByte])
	}
	return ""
}

// outlineLines emits one "line" Symbol per source line for the grammar-free text
// fallback. Each Symbol's byte range excludes the trailing newline. A trailing
// newline does not produce a phantom empty final line; genuine interior/leading
// empty lines are kept. Empty source returns nil.
func outlineLines(src []byte) []Symbol {
	if len(src) == 0 {
		return nil
	}
	var out []Symbol
	row := 1
	lineStart := 0
	for i := 0; i < len(src); i++ {
		if src[i] != '\n' {
			continue
		}
		out = append(out, Symbol{
			Kind:      "line",
			StartByte: lineStart,
			EndByte:   i, // exclude the '\n'
			StartLine: row,
			EndLine:   row,
		})
		row++
		lineStart = i + 1
	}
	if lineStart < len(src) {
		out = append(out, Symbol{
			Kind:      "line",
			StartByte: lineStart,
			EndByte:   len(src),
			StartLine: row,
			EndLine:   row,
		})
	}
	return out
}
