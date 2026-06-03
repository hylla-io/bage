// Package parser defines the engine-agnostic parsing port and the concrete
// syntax tree (CST) data-transfer objects Båge uses to locate byte ranges.
//
// This drop is interface + DTOs only. The official CGO go-tree-sitter adapter
// that implements ParserPort lands in a later, dependency-gated drop per
// docs/adr/0002. Nothing here depends on cgo or any third-party package.
package parser

import (
	"context"
	"strings"
)

// Lang enumerates the source languages a ParserPort adapter may parse.
//
// The zero value is LangUnknown so an unset Lang is explicitly invalid rather
// than silently selecting a grammar.
type Lang int

const (
	// LangUnknown is the zero value and selects no grammar.
	LangUnknown Lang = iota
	// LangGo selects the Go grammar.
	LangGo
	// LangTypeScript selects the TypeScript grammar.
	LangTypeScript
	// LangTSX selects the TSX (TypeScript + JSX) grammar.
	LangTSX
	// LangJavaScript selects the JavaScript grammar.
	LangJavaScript
	// LangPython selects the Python grammar.
	LangPython
	// LangRust selects the Rust grammar.
	LangRust
	// LangJava selects the Java grammar.
	LangJava
	// LangC selects the C grammar.
	LangC
	// LangCPP selects the C++ grammar.
	LangCPP
	// LangCSharp selects the C# grammar.
	LangCSharp
	// LangRuby selects the Ruby grammar.
	LangRuby
	// LangJSON selects the JSON grammar.
	LangJSON
	// LangHTML selects the HTML grammar.
	LangHTML
	// LangCSS selects the CSS grammar.
	LangCSS
	// LangYAML selects the YAML grammar.
	LangYAML
	// LangTOML selects the TOML grammar.
	LangTOML
	// LangXML selects the XML grammar.
	LangXML
	// LangMakefile selects the Make grammar.
	LangMakefile
	// LangBash selects the Bash/shell grammar.
	LangBash
	// LangMarkdown selects the Markdown (block) grammar.
	LangMarkdown
	// LangText selects the grammar-free text fallback: the whole file is one
	// node (with line children). It guarantees any file type (MDX, Dockerfile,
	// SCSS, .txt, …) round-trips losslessly and is byte-anchorable even with no
	// registered grammar.
	LangText
)

// langNames maps each Lang to its lowercase canonical name.
var langNames = map[Lang]string{
	LangGo:         "go",
	LangTypeScript: "typescript",
	LangTSX:        "tsx",
	LangJavaScript: "javascript",
	LangPython:     "python",
	LangRust:       "rust",
	LangJava:       "java",
	LangC:          "c",
	LangCPP:        "cpp",
	LangCSharp:     "csharp",
	LangRuby:       "ruby",
	LangJSON:       "json",
	LangHTML:       "html",
	LangCSS:        "css",
	LangYAML:       "yaml",
	LangTOML:       "toml",
	LangXML:        "xml",
	LangMakefile:   "makefile",
	LangBash:       "bash",
	LangMarkdown:   "markdown",
	LangText:       "text",
}

// LangForPath selects a Lang from a file path by extension, with a few
// extensionless build files keyed by basename (Makefile, Dockerfile, …).
// Unknown or grammar-less types resolve to LangText, the grammar-free fallback,
// so an agent IDE can always open and losslessly round-trip ANY file. It never
// returns LangUnknown. Matching is case-insensitive on the extension.
func LangForPath(path string) Lang {
	base := path
	if i := strings.LastIndexAny(path, "/\\"); i >= 0 {
		base = path[i+1:]
	}
	switch base {
	case "Makefile", "makefile", "GNUmakefile":
		return LangMakefile
	case "Dockerfile", "Containerfile", "Justfile", "justfile":
		// No Go grammar binding exists for these; round-trip via text fallback.
		return LangText
	}
	ext := ""
	if i := strings.LastIndex(base, "."); i > 0 { // i>0 so dotfiles (".env") are text
		ext = strings.ToLower(base[i:])
	}
	switch ext {
	case ".go":
		return LangGo
	case ".ts", ".mts", ".cts":
		return LangTypeScript
	case ".tsx":
		return LangTSX
	case ".js", ".jsx", ".mjs", ".cjs":
		return LangJavaScript
	case ".py", ".pyi":
		return LangPython
	case ".rs":
		return LangRust
	case ".java":
		return LangJava
	case ".c", ".h":
		return LangC
	case ".cc", ".cpp", ".cxx", ".hpp", ".hh", ".hxx":
		return LangCPP
	case ".cs":
		return LangCSharp
	case ".rb":
		return LangRuby
	case ".json":
		return LangJSON
	case ".html", ".htm":
		return LangHTML
	case ".css":
		return LangCSS
	case ".yaml", ".yml":
		return LangYAML
	case ".toml":
		return LangTOML
	case ".xml":
		return LangXML
	case ".mk":
		return LangMakefile
	case ".sh", ".bash":
		return LangBash
	case ".md", ".markdown":
		return LangMarkdown
	default:
		return LangText
	}
}

// String returns the lowercase canonical name of the language, or "unknown"
// for unrecognized values (including the zero value).
func (l Lang) String() string {
	if name, ok := langNames[l]; ok {
		return name
	}
	return "unknown"
}

// Point is a zero-based row/column position within a source file. Col is a byte
// offset within the row, consistent with tree-sitter's point semantics.
type Point struct {
	// Row is the zero-based line number.
	Row int
	// Col is the zero-based byte offset within the row.
	Col int
}

// ByteRange is a half-open [Start, End) span of byte offsets within a source
// file.
type ByteRange struct {
	// Start is the inclusive starting byte offset.
	Start int
	// End is the exclusive ending byte offset.
	End int
}

// Node is a single concrete-syntax-tree node addressed by byte range and point.
type Node struct {
	// Kind is the grammar node type (e.g. "function_declaration").
	Kind string
	// StartByte is the inclusive starting byte offset of the node.
	StartByte int
	// EndByte is the exclusive ending byte offset of the node.
	EndByte int
	// StartPoint is the row/column position of StartByte.
	StartPoint Point
	// EndPoint is the row/column position of EndByte.
	EndPoint Point
	// Named reports whether the node is a named node (vs. an anonymous token).
	Named bool
	// Missing reports whether the node is a MISSING node — a zero-width node the
	// parser inserts to recover from a syntax error (e.g. an absent closing
	// brace). A MISSING node has a normal Kind, so this flag is the only way to
	// distinguish it from a genuine token; parse-health diagnostics surface it
	// alongside ERROR-kind nodes (SPEC §10.5).
	Missing bool
	// Children are the node's direct child nodes in source order.
	Children []*Node
}

// Tree is a parsed concrete syntax tree together with the source bytes it was
// parsed from.
//
// Root and Source are fully materialized and independent of any engine: a
// consumer may use them after the underlying engine tree is freed. Native is an
// opaque, adapter-owned handle to the engine's native tree (e.g. a
// *tree_sitter.Tree), retained only so an adapter can reuse it for incremental
// reparsing and changed-range queries. Consumers MUST treat Native as opaque and
// MUST call Close when done so the adapter can free any native (C) resources.
type Tree struct {
	// Root is the root node of the tree.
	Root *Node
	// Source is the byte slice the tree was parsed from.
	Source []byte
	// Native is an opaque, adapter-owned engine handle; nil for engine-free
	// trees (e.g. test fakes). Consumers never inspect it.
	Native any
}

// Close releases any native resources held by the tree's adapter handle. It is
// idempotent: after the first call the native handle is released and cleared, so
// subsequent calls (and calls on a nil tree or a tree with no native handle) are
// no-ops. This matters because some engine handles (e.g. the CGO tree-sitter
// tree) double-free if their own Close is called twice. Close is not safe for
// concurrent use; callers serialize per tree (SPEC §7, one writer per file).
func (t *Tree) Close() {
	if t == nil {
		return
	}
	if c, ok := t.Native.(interface{ Close() }); ok {
		c.Close()
	}
	t.Native = nil
}

// InputEdit describes a single text edit for incremental reparsing, in the
// shape tree-sitter expects: byte offsets plus the corresponding points.
type InputEdit struct {
	// StartByte is the byte offset where the edit begins.
	StartByte int
	// OldEndByte is the byte offset where the replaced region ended.
	OldEndByte int
	// NewEndByte is the byte offset where the replacement region ends.
	NewEndByte int
	// StartPoint is the point for StartByte.
	StartPoint Point
	// OldEndPoint is the point for OldEndByte.
	OldEndPoint Point
	// NewEndPoint is the point for NewEndByte.
	NewEndPoint Point
}

// ParserPort is the engine-agnostic contract for parsing source into a Tree and
// reparsing incrementally. Adapters (e.g. the CGO go-tree-sitter binding) must
// implement it; the rest of Båge depends only on this interface.
type ParserPort interface {
	// Parse parses src under lang into a fresh Tree.
	Parse(ctx context.Context, lang Lang, src []byte) (*Tree, error)
	// ParseIncremental reparses src under lang, reusing old after applying edit.
	ParseIncremental(ctx context.Context, lang Lang, src []byte, old *Tree, edit InputEdit) (*Tree, error)
	// ChangedRanges reports the byte ranges that differ between old and new.
	ChangedRanges(old, new *Tree) []ByteRange
}
