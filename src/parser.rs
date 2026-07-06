//! Engine-agnostic parsing port, the concrete syntax tree (CST) DTOs Båge
//! uses to locate byte ranges, and the tree-sitter adapter implementing the
//! port with the native Rust bindings (no FFI shim of our own — the grammar C
//! sources are compiled by each grammar crate).
//!
//! Lifecycle: [`Tree`] owns its native engine tree; dropping the `Tree` frees
//! it (RAII replaces the Go side's idempotent `Close`).

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use tree_sitter as ts;

/// Enumerates the source languages the parser may parse.
///
/// There is no `Unknown` variant: an unset language is unrepresentable, and
/// [`Lang::for_path`] is total — anything without a registered grammar
/// resolves to [`Lang::Text`], the grammar-free fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Go,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Json,
    Html,
    Css,
    Yaml,
    Toml,
    Xml,
    Makefile,
    Bash,
    Markdown,
    /// The grammar-free text fallback: the whole file is one node (with line
    /// children). It guarantees any file type (MDX, Dockerfile, SCSS, .txt,
    /// …) round-trips losslessly and is byte-anchorable even with no
    /// registered grammar.
    Text,
}

impl Lang {
    /// Every language, in the Go implementation's declaration order.
    pub const ALL: [Lang; 21] = [
        Lang::Go,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::JavaScript,
        Lang::Python,
        Lang::Rust,
        Lang::Java,
        Lang::C,
        Lang::Cpp,
        Lang::CSharp,
        Lang::Ruby,
        Lang::Json,
        Lang::Html,
        Lang::Css,
        Lang::Yaml,
        Lang::Toml,
        Lang::Xml,
        Lang::Makefile,
        Lang::Bash,
        Lang::Markdown,
        Lang::Text,
    ];

    /// Returns the lowercase canonical name of the language.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Go => "go",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::Rust => "rust",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::CSharp => "csharp",
            Lang::Ruby => "ruby",
            Lang::Json => "json",
            Lang::Html => "html",
            Lang::Css => "css",
            Lang::Yaml => "yaml",
            Lang::Toml => "toml",
            Lang::Xml => "xml",
            Lang::Makefile => "makefile",
            Lang::Bash => "bash",
            Lang::Markdown => "markdown",
            Lang::Text => "text",
        }
    }

    /// Parses a canonical lowercase language name; `None` when unrecognized.
    pub fn from_name(name: &str) -> Option<Lang> {
        Lang::ALL.into_iter().find(|l| l.name() == name)
    }

    /// Selects a `Lang` from a file path by extension, with a few
    /// extensionless build files keyed by basename (Makefile, Dockerfile, …).
    /// Unknown or grammar-less types resolve to [`Lang::Text`], so any file
    /// can always be opened and losslessly round-tripped — the function is
    /// total. Matching is case-insensitive on the extension; dotfiles
    /// (".env") are text.
    pub fn for_path(path: &str) -> Lang {
        let base = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match base.as_str() {
            "Makefile" | "makefile" | "GNUmakefile" => return Lang::Makefile,
            // No grammar binding exists for these; round-trip via text fallback.
            "Dockerfile" | "Containerfile" | "Justfile" | "justfile" => return Lang::Text,
            _ => {}
        }
        let ext = match base.rfind('.') {
            // i > 0 so dotfiles (".env") are text.
            Some(i) if i > 0 => base[i..].to_ascii_lowercase(),
            _ => String::new(),
        };
        match ext.as_str() {
            ".go" => Lang::Go,
            ".ts" | ".mts" | ".cts" => Lang::TypeScript,
            ".tsx" => Lang::Tsx,
            ".js" | ".jsx" | ".mjs" | ".cjs" => Lang::JavaScript,
            ".py" | ".pyi" => Lang::Python,
            ".rs" => Lang::Rust,
            ".java" => Lang::Java,
            ".c" | ".h" => Lang::C,
            ".cc" | ".cpp" | ".cxx" | ".hpp" | ".hh" | ".hxx" => Lang::Cpp,
            ".cs" => Lang::CSharp,
            ".rb" => Lang::Ruby,
            ".json" => Lang::Json,
            ".html" | ".htm" => Lang::Html,
            ".css" => Lang::Css,
            ".yaml" | ".yml" => Lang::Yaml,
            ".toml" => Lang::Toml,
            ".xml" => Lang::Xml,
            ".mk" => Lang::Makefile,
            ".sh" | ".bash" => Lang::Bash,
            ".md" | ".markdown" => Lang::Markdown,
            _ => Lang::Text,
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A zero-based row/column position within a source file. `col` is a byte
/// offset within the row, consistent with tree-sitter's point semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    /// Zero-based line number.
    pub row: usize,
    /// Zero-based byte offset within the row.
    pub col: usize,
}

/// A half-open `[start, end)` span of byte offsets within a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    /// Inclusive starting byte offset.
    pub start: usize,
    /// Exclusive ending byte offset.
    pub end: usize,
}

/// A single concrete-syntax-tree node addressed by byte range and point.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Node {
    /// The grammar node type (e.g. "function_declaration").
    pub kind: String,
    /// Inclusive starting byte offset of the node.
    pub start_byte: usize,
    /// Exclusive ending byte offset of the node.
    pub end_byte: usize,
    /// Row/column position of `start_byte`.
    pub start_point: Point,
    /// Row/column position of `end_byte`.
    pub end_point: Point,
    /// Whether the node is a named node (vs. an anonymous token).
    pub named: bool,
    /// Whether the node is a MISSING node — a zero-width node the parser
    /// inserts to recover from a syntax error (e.g. an absent closing brace).
    /// A MISSING node has a normal `kind`, so this flag is the only way to
    /// distinguish it from a genuine token; parse-health diagnostics surface
    /// it alongside ERROR-kind nodes (SPEC §10.5).
    pub missing: bool,
    /// Direct child nodes in source order.
    pub children: Vec<Node>,
}

impl Node {
    /// Visits the node and every descendant in depth-first source order.
    pub fn walk(&self, f: &mut impl FnMut(&Node)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }
}

/// A parsed concrete syntax tree together with the source bytes it was
/// parsed from.
///
/// `root` and `source` are fully materialized and engine-independent.
/// `native` is the engine's tree, retained so the adapter can reuse it for
/// incremental reparsing and changed-range queries; it is `None` for
/// grammar-free text trees. Dropping the `Tree` frees the native tree.
#[derive(Debug)]
pub struct Tree {
    /// Root node of the tree.
    pub root: Node,
    /// The byte slice the tree was parsed from.
    pub source: Vec<u8>,
    /// Engine-owned native tree, when one exists.
    native: Option<ts::Tree>,
}

impl Tree {
    /// Whether the tree carries an engine tree. `false` identifies the
    /// grammar-free text fallback — the unambiguous discriminator (some real
    /// grammars also use a "document" root, so root kind is not one).
    pub fn has_native(&self) -> bool {
        self.native.is_some()
    }
}

/// Describes a single text edit for incremental reparsing, in the shape
/// tree-sitter expects: byte offsets plus the corresponding points. Offsets
/// are `usize`, so the Go side's negative-offset validation is
/// unrepresentable here.
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
}

/// Errors from the parsing port.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("treesitter: unsupported language: {lang}")]
    UnsupportedLang { lang: Lang },
    #[error("treesitter: set language {lang}: {message}")]
    SetLanguage { lang: Lang, message: String },
    #[error("treesitter: parse {lang} returned no tree")]
    NoTree { lang: Lang },
    #[error("treesitter: old tree has no native handle")]
    NoNativeHandle,
}

/// The engine-agnostic contract for parsing source into a [`Tree`] and
/// reparsing incrementally. The rest of Båge depends only on this trait.
pub trait ParserPort: Send + Sync {
    /// Parses `src` under `lang` into a fresh tree.
    fn parse(&self, lang: Lang, src: &[u8]) -> Result<Tree, ParseError>;
    /// Reparses `src` under `lang`, reusing `old` after applying `edit`.
    fn parse_incremental(
        &self,
        lang: Lang,
        src: &[u8],
        old: &mut Tree,
        edit: InputEdit,
    ) -> Result<Tree, ParseError>;
    /// Reports the byte ranges that differ between `old` and `new`; empty
    /// when either tree has no native handle (callers fall back to a full
    /// reparse).
    fn changed_ranges(&self, old: &Tree, new: &Tree) -> Vec<ByteRange>;
}

/// A [`ParserPort`] backed by the native tree-sitter bindings. It caches one
/// `ts::Language` per supported language for its lifetime.
pub struct Adapter {
    langs: HashMap<Lang, ts::Language>,
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter {
    /// Constructs an adapter with every resolved grammar registered, making
    /// it polyglot. TypeScript and TSX both come from the
    /// tree-sitter-typescript crate via its two distinct language constants.
    pub fn new() -> Adapter {
        let langs: [(Lang, ts::Language); 20] = [
            (Lang::Go, tree_sitter_go::LANGUAGE.into()),
            (
                Lang::TypeScript,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ),
            (Lang::Tsx, tree_sitter_typescript::LANGUAGE_TSX.into()),
            (Lang::JavaScript, tree_sitter_javascript::LANGUAGE.into()),
            (Lang::Python, tree_sitter_python::LANGUAGE.into()),
            (Lang::Rust, tree_sitter_rust::LANGUAGE.into()),
            (Lang::Java, tree_sitter_java::LANGUAGE.into()),
            (Lang::C, tree_sitter_c::LANGUAGE.into()),
            (Lang::Cpp, tree_sitter_cpp::LANGUAGE.into()),
            (Lang::CSharp, tree_sitter_c_sharp::LANGUAGE.into()),
            (Lang::Ruby, tree_sitter_ruby::LANGUAGE.into()),
            (Lang::Json, tree_sitter_json::LANGUAGE.into()),
            (Lang::Html, tree_sitter_html::LANGUAGE.into()),
            (Lang::Css, tree_sitter_css::LANGUAGE.into()),
            (Lang::Yaml, tree_sitter_yaml::LANGUAGE.into()),
            (Lang::Toml, tree_sitter_toml_ng::LANGUAGE.into()),
            (Lang::Xml, tree_sitter_xml::LANGUAGE_XML.into()),
            (Lang::Makefile, tree_sitter_make::LANGUAGE.into()),
            (Lang::Bash, tree_sitter_bash::LANGUAGE.into()),
            (Lang::Markdown, tree_sitter_md::LANGUAGE.into()),
        ];
        Adapter {
            langs: langs.into_iter().collect(),
        }
    }

    /// Returns the cached grammar for `lang`; `Lang::Text` has none.
    fn language(&self, lang: Lang) -> Option<&ts::Language> {
        self.langs.get(&lang)
    }

    fn new_parser(&self, lang: Lang) -> Result<ts::Parser, ParseError> {
        let l = self
            .language(lang)
            .ok_or(ParseError::UnsupportedLang { lang })?;
        let mut p = ts::Parser::new();
        p.set_language(l).map_err(|e| ParseError::SetLanguage {
            lang,
            message: e.to_string(),
        })?;
        Ok(p)
    }
}

impl ParserPort for Adapter {
    fn parse(&self, lang: Lang, src: &[u8]) -> Result<Tree, ParseError> {
        if lang == Lang::Text {
            return Ok(text_tree(src));
        }
        let mut p = self.new_parser(lang)?;
        let tree = p.parse(src, None).ok_or(ParseError::NoTree { lang })?;
        Ok(Tree {
            root: convert(tree.root_node()),
            source: src.to_vec(),
            native: Some(tree),
        })
    }

    fn parse_incremental(
        &self,
        lang: Lang,
        src: &[u8],
        old: &mut Tree,
        edit: InputEdit,
    ) -> Result<Tree, ParseError> {
        if lang == Lang::Text {
            // No native tree to reuse; a fresh whole-file node is correct
            // (just not incrementally optimized) for grammar-free text.
            return Ok(text_tree(src));
        }
        let mut p = self.new_parser(lang)?;
        let old_ts = old.native.as_mut().ok_or(ParseError::NoNativeHandle)?;
        old_ts.edit(&to_ts_input_edit(edit));
        let tree = p
            .parse(src, Some(old_ts))
            .ok_or(ParseError::NoTree { lang })?;
        Ok(Tree {
            root: convert(tree.root_node()),
            source: src.to_vec(),
            native: Some(tree),
        })
    }

    fn changed_ranges(&self, old: &Tree, new: &Tree) -> Vec<ByteRange> {
        match (&old.native, &new.native) {
            (Some(o), Some(n)) => o
                .changed_ranges(n)
                .map(|r| ByteRange {
                    start: r.start_byte,
                    end: r.end_byte,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Builds a grammar-free tree: a named "document" node spanning the whole
/// source, with one named "line" child per source line. Each line node
/// includes its terminating `\n` (the newline is kept WITH the line it ends),
/// so concatenating the children's byte ranges reproduces the source EXACTLY
/// — gap-free and overlap-free. This gives coarse-but-useful, byte-anchorable
/// nodes for any file type with no registered grammar while still
/// round-tripping losslessly. An empty file yields a document node with no
/// children. `native` is `None`, so `changed_ranges` degrades to empty
/// (callers fall back to a full reparse) — correct, not optimized.
fn text_tree(src: &[u8]) -> Tree {
    Tree {
        root: Node {
            kind: "document".to_string(),
            start_byte: 0,
            end_byte: src.len(),
            start_point: Point { row: 0, col: 0 },
            end_point: text_end_point(src),
            named: true,
            missing: false,
            children: text_lines(src),
        },
        source: src.to_vec(),
        native: None,
    }
}

/// Splits `src` into one "line" node per source line. A line spans from its
/// first byte through and INCLUDING its terminating `\n`; trailing content
/// with no final newline becomes a final line node without one. A source that
/// ends on `\n` (or is empty) produces NO phantom empty trailing line. The
/// returned nodes partition `[0, src.len())` contiguously, so concatenating
/// their byte ranges is lossless.
fn text_lines(src: &[u8]) -> Vec<Node> {
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut row = 0;
    for (i, &b) in src.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        lines.push(Node {
            kind: "line".to_string(),
            start_byte: line_start,
            end_byte: i + 1,
            start_point: Point { row, col: 0 },
            end_point: Point {
                row: row + 1,
                col: 0,
            },
            named: true,
            missing: false,
            children: Vec::new(),
        });
        line_start = i + 1;
        row += 1;
    }
    if line_start < src.len() {
        lines.push(Node {
            kind: "line".to_string(),
            start_byte: line_start,
            end_byte: src.len(),
            start_point: Point { row, col: 0 },
            end_point: Point {
                row,
                col: src.len() - line_start,
            },
            named: true,
            missing: false,
            children: Vec::new(),
        });
    }
    lines
}

/// Returns the row/col one past the last byte of `src`, matching tree-sitter
/// point semantics (row = newline count, col = bytes after the last newline).
fn text_end_point(src: &[u8]) -> Point {
    let mut p = Point { row: 0, col: 0 };
    for &b in src {
        if b == b'\n' {
            p.row += 1;
            p.col = 0;
        } else {
            p.col += 1;
        }
    }
    p
}

/// Eagerly materializes a native node and its descendants into the
/// engine-independent [`Node`] DTO so the result outlives the native tree. It
/// walks with an explicit stack rather than recursion, so a deeply nested
/// tree cannot exhaust the thread stack.
fn convert(root: ts::Node<'_>) -> Node {
    // Pass 1: flatten to a preorder arena of (node, parent index) with an
    // explicit stack; children are pushed in reverse so they pop — and land
    // in the arena — in source order. Every descendant therefore sits at a
    // higher index than its parent.
    let mut arena: Vec<(Node, Option<usize>)> = Vec::new();
    let mut stack: Vec<(ts::Node<'_>, Option<usize>)> = vec![(root, None)];
    while let Some((n, parent)) = stack.pop() {
        let idx = arena.len();
        arena.push((new_node(&n), parent));
        for i in (0..n.child_count()).rev() {
            if let Some(c) = n.child(i as u32) {
                stack.push((c, Some(idx)));
            }
        }
    }
    // Pass 2: stitch bottom-up. Sweeping indices in reverse hands each node
    // to its parent's accumulator only after the node's own children are
    // attached; the reverse sweep collects each parent's children in reverse
    // source order, so one iterative Vec::reverse per node restores order —
    // no recursion anywhere, so tree depth cannot exhaust the thread stack.
    let mut acc: Vec<Vec<Node>> = arena.iter().map(|_| Vec::new()).collect();
    for i in (0..arena.len()).rev() {
        let (node_slot, parent) = &mut arena[i];
        let mut node = std::mem::take(node_slot);
        let mut kids = std::mem::take(&mut acc[i]);
        kids.reverse();
        node.children = kids;
        match parent {
            Some(p) => {
                let p = *p;
                acc[p].push(node);
            }
            None => return node,
        }
    }
    unreachable!("arena root returns from the reverse sweep")
}

/// Builds a childless [`Node`] DTO from a native node.
fn new_node(n: &ts::Node<'_>) -> Node {
    Node {
        kind: n.kind().to_string(),
        start_byte: n.start_byte(),
        end_byte: n.end_byte(),
        start_point: to_point(n.start_position()),
        end_point: to_point(n.end_position()),
        named: n.is_named(),
        missing: n.is_missing(),
        children: Vec::new(),
    }
}

/// Converts a native point to the DTO point.
fn to_point(p: ts::Point) -> Point {
    Point {
        row: p.row,
        col: p.column,
    }
}

/// Converts the DTO edit into the native edit shape.
fn to_ts_input_edit(e: InputEdit) -> ts::InputEdit {
    ts::InputEdit {
        start_byte: e.start_byte,
        old_end_byte: e.old_end_byte,
        new_end_byte: e.new_end_byte,
        start_position: ts::Point {
            row: e.start_point.row,
            column: e.start_point.col,
        },
        old_end_position: ts::Point {
            row: e.old_end_point.row,
            column: e.old_end_point.col,
        },
        new_end_position: ts::Point {
            row: e.new_end_point.row,
            column: e.new_end_point.col,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_for_path_is_total() {
        let cases: &[(&str, Lang)] = &[
            ("main.go", Lang::Go),
            ("src/app.ts", Lang::TypeScript),
            ("src/App.tsx", Lang::Tsx),
            ("a.mjs", Lang::JavaScript),
            ("x.PY", Lang::Python),
            ("lib.rs", Lang::Rust),
            ("A.java", Lang::Java),
            ("h.h", Lang::C),
            ("v.hpp", Lang::Cpp),
            ("p.cs", Lang::CSharp),
            ("r.rb", Lang::Ruby),
            ("data.json", Lang::Json),
            ("index.html", Lang::Html),
            ("style.css", Lang::Css),
            ("c.yml", Lang::Yaml),
            ("Cargo.toml", Lang::Toml),
            ("pom.xml", Lang::Xml),
            ("Makefile", Lang::Makefile),
            ("rules.mk", Lang::Makefile),
            ("run.sh", Lang::Bash),
            ("README.md", Lang::Markdown),
            // Fallback-to-text cases: dotfiles, no extension, no grammar.
            (".env", Lang::Text),
            ("Dockerfile", Lang::Text),
            ("Justfile", Lang::Text),
            ("notes.txt", Lang::Text),
            ("component.mdx", Lang::Text),
            ("style.scss", Lang::Text),
            ("noext", Lang::Text),
            ("", Lang::Text),
            ("dir/sub/file.go", Lang::Go),
        ];
        for (path, want) in cases {
            assert_eq!(Lang::for_path(path), *want, "path {path:?}");
        }
    }

    #[test]
    fn parse_polyglot_all_grammars() {
        let snippets: &[(Lang, &[u8])] = &[
            (Lang::Go, b"package main\n\nfunc main() {}\n"),
            (Lang::TypeScript, b"const x: number = 1;\n"),
            (Lang::Tsx, b"const e = <div a={1} />;\n"),
            (Lang::JavaScript, b"function f() { return 1; }\n"),
            (Lang::Python, b"def f():\n    return 1\n"),
            (Lang::Rust, b"fn main() { println!(\"hi\"); }\n"),
            (Lang::Java, b"class A { void m() {} }\n"),
            (Lang::C, b"int main(void) { return 0; }\n"),
            (Lang::Cpp, b"int main() { return 0; }\n"),
            (Lang::CSharp, b"class A { void M() {} }\n"),
            (Lang::Ruby, b"def f\n  1\nend\n"),
            (Lang::Json, b"{\"a\": [1, 2]}\n"),
            (Lang::Html, b"<html><body>hi</body></html>\n"),
            (Lang::Css, b"body { color: red; }\n"),
            (Lang::Yaml, b"a: 1\nb:\n  - x\n"),
            (Lang::Toml, b"[a]\nb = 1\n"),
            (Lang::Xml, b"<a><b>x</b></a>\n"),
            (Lang::Makefile, b"all:\n\techo hi\n"),
            (Lang::Bash, b"echo hi\n"),
            (Lang::Markdown, b"# Title\n\nbody\n"),
        ];
        let a = Adapter::new();
        for (lang, src) in snippets {
            let tree = a
                .parse(*lang, src)
                .unwrap_or_else(|e| panic!("{lang}: {e}"));
            assert_eq!(tree.root.start_byte, 0, "{lang}");
            assert_eq!(tree.root.end_byte, src.len(), "{lang} root spans source");
            assert!(!tree.root.children.is_empty(), "{lang} has children");
            assert_eq!(tree.source, *src, "{lang}");
        }
    }

    #[test]
    fn text_fallback_lossless() {
        let a = Adapter::new();
        let inputs: &[&[u8]] = &[
            b"",
            b"one line no newline",
            b"a\nb\n",
            b"a\nb",
            b"\n\n\n",
            b"\xEF\xBB\xBF mixed \r\n bytes \xff\x00\n tail",
        ];
        for src in inputs {
            let tree = a.parse(Lang::Text, src).unwrap();
            assert_eq!(tree.root.kind, "document");
            assert_eq!(tree.root.end_byte, src.len());
            // Children partition [0, len) contiguously — losslessness.
            let mut pos = 0;
            for line in &tree.root.children {
                assert_eq!(line.kind, "line");
                assert_eq!(line.start_byte, pos, "gap-free for {src:?}");
                assert!(line.end_byte > line.start_byte);
                pos = line.end_byte;
            }
            assert_eq!(pos, src.len(), "covers source for {src:?}");
            if src.is_empty() {
                assert!(tree.root.children.is_empty());
            }
        }
    }

    #[test]
    fn incremental_reparse_matches_full() {
        let a = Adapter::new();
        let old_src = b"package main\n\nfunc main() {}\n";
        let new_src = b"package main\n\nfunc mainX() {}\n";
        let mut old = a.parse(Lang::Go, old_src).unwrap();
        // Insert "X" at byte 23 (end of the "main" identifier).
        let edit = InputEdit {
            start_byte: 23,
            old_end_byte: 23,
            new_end_byte: 24,
            start_point: Point { row: 2, col: 9 },
            old_end_point: Point { row: 2, col: 9 },
            new_end_point: Point { row: 2, col: 10 },
        };
        let incr = a
            .parse_incremental(Lang::Go, new_src, &mut old, edit)
            .unwrap();
        let full = a.parse(Lang::Go, new_src).unwrap();
        assert_eq!(incr.root, full.root);
        // A token-only rename may legitimately report no structural
        // changed ranges; just exercise the call.
        let _ = a.changed_ranges(&old, &incr);
    }

    #[test]
    fn error_and_missing_nodes_are_surfaced() {
        let a = Adapter::new();
        let tree = a
            .parse(Lang::Go, b"package main\n\nfunc main() {\n")
            .unwrap();
        let mut found = false;
        tree.root.walk(&mut |n| {
            if n.kind == "ERROR" || n.missing {
                found = true;
            }
        });
        assert!(found, "broken source surfaces ERROR/MISSING nodes");
    }

    #[test]
    fn deep_tree_does_not_overflow() {
        let a = Adapter::new();
        let mut src = Vec::new();
        src.extend_from_slice(b"const x = ");
        src.extend(std::iter::repeat_n(b'(', 2000));
        src.push(b'1');
        src.extend(std::iter::repeat_n(b')', 2000));
        let tree = a.parse(Lang::JavaScript, &src).unwrap();
        assert_eq!(tree.root.end_byte, src.len());
    }
}
