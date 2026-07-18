//! Structured import/export FACT extraction — the cross-file linkage substrate
//! Hylla joins into edges (module A imports symbol S from module B; module B
//! re-exports S). It is a sibling read-only surface over an
//! [`crate::inspect::OpenedFile`], parallel to the tier-2 analysis substrate:
//! the declaration outline stays the stable anchor floor, facts are a
//! DISPOSABLE derived layer (re-derived per parse, never anchored).
//!
//! WHY tree-sitter QUERIES (not a hand walk like [`crate::tier2`]): imports and
//! exports are dense, deeply-fielded grammar constructs (nested Rust use-trees,
//! TS `export … from`, Python `from … import … as`) whose shape is pinned by
//! per-language `field:` labels. Queries locate the entry nodes at ANY depth
//! (a `pub use` inside a `mod`), then field-addressed native-node walking pulls
//! the typed parts — no text scanning, no cross-language leakage.
//!
//! NEVER-GUESS invariant: a language with no registered query set yields a
//! typed [`Facts::Unsupported`] — never a fabricated fact; an unrecognized
//! construct within a supported language is silently skipped, never coerced
//! into a wrong-shaped fact. Only Rust, TS/TSX/JS, Python, and Go carry query
//! sets (the first-drop set); every other [`Lang`] is Unsupported.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tree_sitter::{Node as TsNode, Query, QueryCursor, StreamingIterator};

use crate::inspect::OpenedFile;
use crate::parser::Lang;

/// One name brought in by an import: the imported symbol plus an optional local
/// alias. `name` is the symbol as named at the SOURCE (`HashMap`, `default`,
/// `*` for a namespace/dot import); `alias` is the LOCAL rebinding when present
/// (`use x::y as z` → alias `z`, `import * as ns` → alias `ns`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedItem {
    /// The symbol name at the source module (`*` marks a namespace/dot bind).
    pub name: String,
    /// The local alias, when the import renames the symbol; else `None`.
    pub alias: Option<String>,
}

/// A single import construct, normalized across languages. `source_specifier`
/// is the module path/string the names come from (quotes stripped);
/// `items` are the named symbols (empty for a bare module/side-effect import or
/// a `glob`); `glob` marks a wildcard/dot import (`use a::*`, `from a import *`,
/// Go `. "pkg"`); `re_export` marks a Rust `pub use` (import that is ALSO a
/// public re-export — TS `export … from` is modeled as an [`ExportFact`],
/// never here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportFact {
    /// The source module path/specifier (quotes stripped).
    pub source_specifier: String,
    /// The named imported symbols; empty for module/side-effect/glob imports.
    pub items: Vec<ImportedItem>,
    /// Whether this is a wildcard/dot import bringing all names into scope.
    pub glob: bool,
    /// Whether this import is also a public re-export (Rust `pub use`).
    pub re_export: bool,
}

/// The syntactic category of an export. `Reexport` = the export forwards a
/// symbol from another module (`re_exported_from` populated); `Default` = a
/// default export; `Other` = a named export whose underlying kind is not
/// resolvable from syntax alone (never guessed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    Function,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    TypeAlias,
    Const,
    Variable,
    Module,
    Reexport,
    Default,
    Other,
}

/// A single export construct, normalized across languages. `name` is the
/// exported symbol (`*`/`default` for glob/default exports); `kind` is its
/// syntactic category; `re_exported_from` carries the source module when the
/// export forwards another module's symbol (Rust `pub use`, TS `export … from`);
/// `alias` is the exported-as name when renamed (`export { x as y }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportFact {
    /// The exported symbol name (`*` glob, `default` default export).
    pub name: String,
    /// The syntactic category of the export.
    pub kind: ExportKind,
    /// The source module when this export re-exports another module's symbol.
    pub re_exported_from: Option<String>,
    /// The exported-as alias when the export renames the symbol.
    pub alias: Option<String>,
}

/// The result of fact extraction over one file. `Unsupported` is the typed
/// NEVER-GUESS opt-out for a language with no query set — callers must branch
/// on it, never treat an empty `Supported` and an unsupported language alike.
// NOTE: no serde derive — `Facts::Unsupported` carries a typed [`Lang`], which
// is intentionally not serde in the parser port. The fact PAYLOADS
// ([`ImportFact`]/[`ExportFact`]/…) are serde-friendly for golden fixtures;
// the outer result is a live typed branch, not a wire type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Facts {
    /// The language carries import/export query sets; facts were extracted
    /// (either list may be empty when the file has no imports/exports).
    Supported {
        imports: Vec<ImportFact>,
        exports: Vec<ExportFact>,
    },
    /// No query set for this language — no facts are guessed.
    Unsupported { lang: Lang },
}

/// Extracts import/export [`Facts`] from a parsed file. Returns
/// [`Facts::Unsupported`] for any language outside the first-drop set (Rust,
/// TS/TSX/JS, Python, Go), never a guessed fact.
///
/// It RE-PARSES `opened`'s source bytes with a private tree-sitter parser: the
/// declaration outline's native tree is intentionally not exposed by the parser
/// port, and queries require native nodes. The re-parse is over the identical
/// bytes, so facts are consistent with the outline the host anchors on.
pub fn extract_facts(opened: &OpenedFile) -> Facts {
    let lang = opened.lang;
    let Some(ts_lang) = grammar(lang) else {
        return Facts::Unsupported { lang };
    };
    let src = opened.tree.source.as_slice();
    let mut parser = tree_sitter::Parser::new();
    // A grammar we registered above must set cleanly; a failure here is a build
    // bug, not runtime input — fail loud.
    parser
        .set_language(&ts_lang)
        .expect("registered grammar sets");
    let Some(tree) = parser.parse(src, None) else {
        // A total parse failure over already-parsed bytes is unreachable in
        // practice; degrade to empty rather than fabricate.
        return Facts::Supported {
            imports: Vec::new(),
            exports: Vec::new(),
        };
    };
    let root = tree.root_node();
    let (imports, exports) = match lang {
        Lang::Rust => (
            rust_imports(root, src, &ts_lang),
            rust_exports(root, src, &ts_lang),
        ),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => (
            ts_imports(root, src, &ts_lang),
            ts_exports(root, src, &ts_lang),
        ),
        Lang::Python => (python_imports(root, src, &ts_lang), Vec::new()),
        Lang::Go => (go_imports(root, src, &ts_lang), Vec::new()),
        _ => (Vec::new(), Vec::new()),
    };
    Facts::Supported { imports, exports }
}

/// The tree-sitter grammar for a supported fact language; `None` is the
/// NEVER-GUESS opt-out (every non-first-drop language).
fn grammar(lang: Lang) -> Option<tree_sitter::Language> {
    Some(match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        _ => return None,
    })
}

/// Runs a single-capture query and returns every captured node, in match
/// order, at any depth. `query_src` is an authored constant — a compile
/// failure is a programmer bug, surfaced by panic (fail-loud on our own code).
fn query_nodes<'t>(
    query_src: &str,
    lang: &tree_sitter::Language,
    root: TsNode<'t>,
    src: &[u8],
) -> Vec<TsNode<'t>> {
    let query = Query::new(lang, query_src).expect("authored query compiles");
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut it = cursor.matches(&query, root, src);
    while let Some(m) = it.next() {
        for cap in m.captures {
            out.push(cap.node);
        }
    }
    out
}

/// The UTF-8 text of a node, lossy on the rare invalid-UTF-8 slice (matching
/// the outline's best-effort text policy); empty on an out-of-range node.
fn ntext(n: TsNode, src: &[u8]) -> String {
    n.utf8_text(src).map(str::to_owned).unwrap_or_default()
}

/// The inner text of a string-specifier node with the surrounding quotes
/// removed: TS `(string (string_fragment))` and Go
/// `(interpreted_string_literal (…content))` both wrap the payload in a single
/// named child, so the child's text is the specifier; otherwise the raw text is
/// stripped of a matching leading/trailing quote pair.
fn string_inner(n: TsNode, src: &[u8]) -> String {
    if let Some(child) = n.named_child(0) {
        return ntext(child, src);
    }
    let raw = ntext(n, src);
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let (f, l) = (bytes[0], bytes[bytes.len() - 1]);
        if (f == b'"' || f == b'\'' || f == b'`') && f == l {
            return raw[1..raw.len() - 1].to_string();
        }
    }
    raw
}

// ---------------------------------------------------------------- Rust ------

/// Rust imports: every `use_declaration` at any depth (module-nested `use`s
/// included), classified by its `argument` shape. A `pub use` sets `re_export`.
fn rust_imports(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> Vec<ImportFact> {
    let mut out = Vec::new();
    for u in query_nodes("(use_declaration) @u", lang, root, src) {
        let re_export = has_pub_visibility(u, src);
        let Some(arg) = u.child_by_field_name("argument") else {
            continue;
        };
        let (source_specifier, items, glob) = rust_use_arg(arg, src);
        let source_specifier = strip_crate_root(source_specifier);
        out.push(ImportFact {
            source_specifier,
            items,
            glob,
            re_export,
        });
    }
    out
}

/// Decomposes a Rust `use_declaration` `argument` into
/// `(source_specifier, items, glob)`. Handles the full argument grammar:
/// `scoped_identifier` (single symbol), `use_wildcard` (glob), `use_as_clause`
/// (aliased single symbol), `scoped_use_list` (brace group), and a bare
/// `identifier` (module import). Unknown shapes yield an empty module import
/// rather than a guess.
fn rust_use_arg(arg: TsNode, src: &[u8]) -> (String, Vec<ImportedItem>, bool) {
    match arg.kind() {
        "scoped_identifier" => {
            let path = arg
                .child_by_field_name("path")
                .map(|p| ntext(p, src))
                .unwrap_or_default();
            let name = arg
                .child_by_field_name("name")
                .map(|n| ntext(n, src))
                .unwrap_or_default();
            (path, vec![ImportedItem { name, alias: None }], false)
        }
        "identifier" => (ntext(arg, src), Vec::new(), false),
        "use_wildcard" => {
            // `use a::b::*` — the sole named child is the path before `*`.
            let inner = arg
                .named_child(0)
                .map(|n| ntext(n, src))
                .unwrap_or_default();
            (inner, Vec::new(), true)
        }
        "use_as_clause" => {
            let alias = arg.child_by_field_name("alias").map(|a| ntext(a, src));
            let path = arg.child_by_field_name("path");
            match path {
                Some(p) if p.kind() == "scoped_identifier" => {
                    let source = p
                        .child_by_field_name("path")
                        .map(|x| ntext(x, src))
                        .unwrap_or_default();
                    let name = p
                        .child_by_field_name("name")
                        .map(|x| ntext(x, src))
                        .unwrap_or_default();
                    (source, vec![ImportedItem { name, alias }], false)
                }
                Some(p) => {
                    let name = ntext(p, src);
                    (String::new(), vec![ImportedItem { name, alias }], false)
                }
                None => (String::new(), Vec::new(), false),
            }
        }
        "scoped_use_list" => {
            let source = arg
                .child_by_field_name("path")
                .map(|p| ntext(p, src))
                .unwrap_or_default();
            let items = arg
                .child_by_field_name("list")
                .map(|list| rust_use_list_items(list, src))
                .unwrap_or_default();
            (source, items, false)
        }
        // `use {a, b}` (no path prefix): a brace list at the root.
        "use_list" => (String::new(), rust_use_list_items(arg, src), false),
        _ => (ntext(arg, src), Vec::new(), false),
    }
}

/// The items of a Rust `use_list` brace group, flattened to ONE
/// [`ImportedItem`] per LEAF symbol. Nested `scoped_use_list`/`use_list`
/// children are RECURSED into, the child's path prefixed onto each descended
/// item's `name`, so a leaf's true source path stays reconstructable from
/// `source_specifier + name` (`use a::{b::{c::D, e::F}}` → source `a`, items
/// `b::c::D`, `b::e::F`). A nested `use_wildcard` is emitted as a glob-marked
/// item whose `name` ends in `*` (`use tokio::{sync::*, task}` → items
/// `sync::*` and `task`); the enclosing `ImportFact.glob` stays false while
/// sibling named items remain. `identifier`/`self`/scoped paths map to their
/// text; `use_as_clause` maps to an aliased item (alias unprefixed — local
/// rebind). An UNRECOGNIZED child is SKIPPED (never a phantom composite name),
/// per the never-guess invariant.
fn rust_use_list_items(list: TsNode, src: &[u8]) -> Vec<ImportedItem> {
    let mut items = Vec::new();
    collect_use_list_items(list, "", src, &mut items);
    items
}

/// Recursive worker for [`rust_use_list_items`]. `prefix` is the accumulated
/// path from enclosing nested groups (empty at the top list); every leaf's
/// `name` is `prefix`-qualified so descent never loses the intermediate path.
fn collect_use_list_items(list: TsNode, prefix: &str, src: &[u8], out: &mut Vec<ImportedItem>) {
    for i in 0..list.named_child_count() {
        let Some(c) = list.named_child(i as u32) else {
            continue;
        };
        match c.kind() {
            "identifier" | "self" | "scoped_identifier" | "type_identifier" => {
                out.push(ImportedItem {
                    name: qualify(prefix, &ntext(c, src)),
                    alias: None,
                })
            }
            "use_as_clause" => {
                let name = c
                    .child_by_field_name("path")
                    .map(|p| ntext(p, src))
                    .unwrap_or_default();
                let alias = c.child_by_field_name("alias").map(|a| ntext(a, src));
                out.push(ImportedItem {
                    name: qualify(prefix, &name),
                    alias,
                });
            }
            "use_wildcard" => {
                // `sync::*` inside a group: glob signal carried as an item whose
                // name ends in `*` (per-item marker; the fact's `glob` bool stays
                // false because named siblings coexist).
                let inner = c.named_child(0).map(|n| ntext(n, src)).unwrap_or_default();
                let base = qualify(prefix, &inner);
                let name = if base.is_empty() {
                    "*".to_string()
                } else {
                    format!("{base}::*")
                };
                out.push(ImportedItem { name, alias: None });
            }
            // Nested `path::{…}` group: descend, extending the prefix by its path.
            "scoped_use_list" => {
                let sub = c
                    .child_by_field_name("path")
                    .map(|p| ntext(p, src))
                    .unwrap_or_default();
                if let Some(inner) = c.child_by_field_name("list") {
                    collect_use_list_items(inner, &qualify(prefix, &sub), src, out);
                }
            }
            // Bare nested `{…}` group (no path prefix): descend unchanged.
            "use_list" => collect_use_list_items(c, prefix, src, out),
            // Unrecognized child: SKIP (never-guess — no phantom composite name).
            _ => {}
        }
    }
}

/// Joins a nested-group `prefix` onto a leaf `name` with `::`, tolerating an
/// empty side (top-level list → bare name; pathless descent → bare prefix).
fn qualify(prefix: &str, name: &str) -> String {
    match (prefix.is_empty(), name.is_empty()) {
        (true, _) => name.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}::{name}"),
    }
}

/// Strips a leading `::` crate-root anchor from a Rust `use` source specifier
/// (`use ::std::io` → source `std::io`); other paths pass through unchanged.
fn strip_crate_root(source: String) -> String {
    source
        .strip_prefix("::")
        .map(str::to_owned)
        .unwrap_or(source)
}

/// Rust exports: `pub` item declarations plus `pub use` re-exports. A `pub use`
/// yields one [`ExportFact`] per re-exported item (kind [`ExportKind::Reexport`],
/// `re_exported_from` = the use source); a `pub` fn/struct/enum/trait/const/
/// static/mod/type/union yields a kind-tagged export. Restricted visibilities
/// (`pub(crate)`) are NOT public exports.
fn rust_exports(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> Vec<ExportFact> {
    let mut out = Vec::new();
    // `pub use` re-exports (query use_declarations; keep only public ones).
    for u in query_nodes("(use_declaration) @u", lang, root, src) {
        if !has_pub_visibility(u, src) {
            continue;
        }
        let Some(arg) = u.child_by_field_name("argument") else {
            continue;
        };
        let (source, items, glob) = rust_use_arg(arg, src);
        let source = strip_crate_root(source);
        if glob {
            out.push(ExportFact {
                name: "*".to_string(),
                kind: ExportKind::Reexport,
                re_exported_from: Some(source),
                alias: None,
            });
        } else {
            for it in items {
                out.push(ExportFact {
                    name: it.name,
                    kind: ExportKind::Reexport,
                    re_exported_from: Some(source.clone()),
                    alias: it.alias,
                });
            }
        }
    }
    // `pub` item declarations.
    let q = "[(function_item) (struct_item) (enum_item) (trait_item) (const_item) (static_item) (mod_item) (type_item) (union_item)] @item";
    for item in query_nodes(q, lang, root, src) {
        if !has_pub_visibility(item, src) {
            continue;
        }
        let name = item
            .child_by_field_name("name")
            .map(|n| ntext(n, src))
            .unwrap_or_default();
        out.push(ExportFact {
            name,
            kind: rust_item_kind(item.kind()),
            re_exported_from: None,
            alias: None,
        });
    }
    out
}

/// Maps a Rust item node kind to an [`ExportKind`].
fn rust_item_kind(kind: &str) -> ExportKind {
    match kind {
        "function_item" => ExportKind::Function,
        "struct_item" | "union_item" => ExportKind::Struct,
        "enum_item" => ExportKind::Enum,
        "trait_item" => ExportKind::Trait,
        "const_item" | "static_item" => ExportKind::Const,
        "mod_item" => ExportKind::Module,
        "type_item" => ExportKind::TypeAlias,
        _ => ExportKind::Other,
    }
}

/// Whether a Rust declaration carries a fully-public `pub` visibility modifier
/// (a bare `pub`, not `pub(crate)`/`pub(super)` — those are not public exports).
fn has_pub_visibility(n: TsNode, src: &[u8]) -> bool {
    for i in 0..n.named_child_count() {
        if let Some(c) = n.named_child(i as u32)
            && c.kind() == "visibility_modifier"
            && ntext(c, src) == "pub"
        {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------ TS/TSX/JS -----

/// TS/JS imports: every `import_statement`. The `import_clause` (when present)
/// carries a default binding, `named_imports`, or a `namespace_import` (glob);
/// a clause-less statement is a side-effect import (empty items).
fn ts_imports(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> Vec<ImportFact> {
    let mut out = Vec::new();
    for stmt in query_nodes("(import_statement) @i", lang, root, src) {
        let source_specifier = stmt
            .child_by_field_name("source")
            .map(|s| string_inner(s, src))
            .unwrap_or_default();
        let mut items = Vec::new();
        let mut glob = false;
        if let Some(clause) = child_of_kind(stmt, "import_clause") {
            for i in 0..clause.named_child_count() {
                let Some(c) = clause.named_child(i as u32) else {
                    continue;
                };
                match c.kind() {
                    // `import x from 'm'` — default binding.
                    "identifier" => items.push(ImportedItem {
                        name: "default".to_string(),
                        alias: Some(ntext(c, src)),
                    }),
                    "named_imports" => ts_named_imports(c, src, &mut items),
                    // `import * as ns from 'm'` — namespace (glob) binding.
                    "namespace_import" => {
                        glob = true;
                        let ns = c.named_child(0).map(|n| ntext(n, src));
                        items.push(ImportedItem {
                            name: "*".to_string(),
                            alias: ns,
                        });
                    }
                    _ => {}
                }
            }
        }
        out.push(ImportFact {
            source_specifier,
            items,
            glob,
            re_export: false,
        });
    }
    out
}

/// Appends each `import_specifier` of a `named_imports` group as an item
/// (`{ a, b as c }` → `a`, `b`/alias `c`).
fn ts_named_imports(group: TsNode, src: &[u8], items: &mut Vec<ImportedItem>) {
    for i in 0..group.named_child_count() {
        let Some(spec) = group.named_child(i as u32) else {
            continue;
        };
        if spec.kind() != "import_specifier" {
            continue;
        }
        let name = spec
            .child_by_field_name("name")
            .map(|n| ntext(n, src))
            .unwrap_or_default();
        let alias = spec.child_by_field_name("alias").map(|a| ntext(a, src));
        items.push(ImportedItem { name, alias });
    }
}

/// TS/JS exports: every `export_statement`. Covers `export { … }` (optionally
/// `from` another module → re-export), `export * from`, `export default …`,
/// and `export`-prefixed declarations (const/function/class/interface/type/
/// enum). `export … from` is modeled here as a re-export, never as an import.
fn ts_exports(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> Vec<ExportFact> {
    let mut out = Vec::new();
    for stmt in query_nodes("(export_statement) @e", lang, root, src) {
        let source = stmt
            .child_by_field_name("source")
            .map(|s| string_inner(s, src));
        if let Some(clause) = child_of_kind(stmt, "export_clause") {
            // `export { a, b as d }` and `export { x } from 'm'`.
            for i in 0..clause.named_child_count() {
                let Some(spec) = clause.named_child(i as u32) else {
                    continue;
                };
                if spec.kind() != "export_specifier" {
                    continue;
                }
                let name = spec
                    .child_by_field_name("name")
                    .map(|n| ntext(n, src))
                    .unwrap_or_default();
                let alias = spec.child_by_field_name("alias").map(|a| ntext(a, src));
                out.push(ExportFact {
                    kind: if source.is_some() {
                        ExportKind::Reexport
                    } else {
                        ExportKind::Other
                    },
                    name,
                    re_exported_from: source.clone(),
                    alias,
                });
            }
        } else if let Some(decl) = stmt.child_by_field_name("declaration") {
            ts_decl_exports(decl, src, &mut out);
        } else if stmt.child_by_field_name("value").is_some() {
            // `export default <expr>`.
            out.push(ExportFact {
                name: "default".to_string(),
                kind: ExportKind::Default,
                re_exported_from: None,
                alias: None,
            });
        } else if let Some(from) = source {
            // `export * from 'm'` — a glob re-export (no clause/decl/value).
            out.push(ExportFact {
                name: "*".to_string(),
                kind: ExportKind::Reexport,
                re_exported_from: Some(from),
                alias: None,
            });
        }
    }
    out
}

/// Emits export facts for an `export`-prefixed declaration
/// (`export const K = …`, `export function g`, `export class C`, …). Variable
/// declarations emit one fact per declarator.
fn ts_decl_exports(decl: TsNode, src: &[u8], out: &mut Vec<ExportFact>) {
    match decl.kind() {
        "function_declaration" | "generator_function_declaration" => {
            push_named(decl, ExportKind::Function, src, out)
        }
        "class_declaration" | "abstract_class_declaration" => {
            push_named(decl, ExportKind::Class, src, out)
        }
        "interface_declaration" => push_named(decl, ExportKind::Interface, src, out),
        "enum_declaration" => push_named(decl, ExportKind::Enum, src, out),
        "type_alias_declaration" => push_named(decl, ExportKind::TypeAlias, src, out),
        "lexical_declaration" | "variable_declaration" => {
            for i in 0..decl.named_child_count() {
                let Some(d) = decl.named_child(i as u32) else {
                    continue;
                };
                if d.kind() != "variable_declarator" {
                    continue;
                }
                let name = d
                    .child_by_field_name("name")
                    .map(|n| ntext(n, src))
                    .unwrap_or_default();
                out.push(ExportFact {
                    name,
                    kind: ExportKind::Variable,
                    re_exported_from: None,
                    alias: None,
                });
            }
        }
        _ => {}
    }
}

/// Pushes a single export fact whose name is `decl`'s `name` field.
fn push_named(decl: TsNode, kind: ExportKind, src: &[u8], out: &mut Vec<ExportFact>) {
    let name = decl
        .child_by_field_name("name")
        .map(|n| ntext(n, src))
        .unwrap_or_default();
    out.push(ExportFact {
        name,
        kind,
        re_exported_from: None,
        alias: None,
    });
}

/// The first direct child of `n` whose kind is `kind` (for grammar children
/// that carry no `field:` label, e.g. `import_clause`/`export_clause`).
fn child_of_kind<'t>(n: TsNode<'t>, kind: &str) -> Option<TsNode<'t>> {
    for i in 0..n.named_child_count() {
        if let Some(c) = n.named_child(i as u32)
            && c.kind() == kind
        {
            return Some(c);
        }
    }
    None
}

// -------------------------------------------------------------- Python ------

/// Python imports: `import_statement` (one fact per module named) and
/// `import_from_statement` (`from M import …`, with `*` as glob). Aliases
/// (`import x as y`, `from m import a as b`) are preserved.
fn python_imports(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> Vec<ImportFact> {
    let mut out = Vec::new();
    let q = "[(import_statement) (import_from_statement)] @i";
    for stmt in query_nodes(q, lang, root, src) {
        if stmt.kind() == "import_statement" {
            for i in 0..stmt.named_child_count() {
                let Some(c) = stmt.named_child(i as u32) else {
                    continue;
                };
                match c.kind() {
                    "dotted_name" => out.push(ImportFact {
                        source_specifier: ntext(c, src),
                        items: Vec::new(),
                        glob: false,
                        re_export: false,
                    }),
                    "aliased_import" => {
                        let name = c
                            .child_by_field_name("name")
                            .map(|n| ntext(n, src))
                            .unwrap_or_default();
                        let alias = c.child_by_field_name("alias").map(|a| ntext(a, src));
                        out.push(ImportFact {
                            source_specifier: name.clone(),
                            items: vec![ImportedItem { name, alias }],
                            glob: false,
                            re_export: false,
                        });
                    }
                    _ => {}
                }
            }
        } else {
            // import_from_statement
            let source_specifier = stmt
                .child_by_field_name("module_name")
                .map(|m| ntext(m, src))
                .unwrap_or_default();
            let mut items = Vec::new();
            let mut glob = false;
            for i in 0..stmt.named_child_count() {
                let Some(c) = stmt.named_child(i as u32) else {
                    continue;
                };
                match c.kind() {
                    "wildcard_import" => glob = true,
                    "dotted_name" if Some(c) != stmt.child_by_field_name("module_name") => {
                        items.push(ImportedItem {
                            name: ntext(c, src),
                            alias: None,
                        });
                    }
                    "aliased_import" => {
                        let name = c
                            .child_by_field_name("name")
                            .map(|n| ntext(n, src))
                            .unwrap_or_default();
                        let alias = c.child_by_field_name("alias").map(|a| ntext(a, src));
                        items.push(ImportedItem { name, alias });
                    }
                    _ => {}
                }
            }
            out.push(ImportFact {
                source_specifier,
                items,
                glob,
                re_export: false,
            });
        }
    }
    out
}

// ---------------------------------------------------------------- Go --------

/// Go imports: each `import_spec` (single `import "x"` or a parenthesized
/// block). A `package_identifier` name is an alias; a `.` name is a dot import
/// (`glob`); a `_` blank name is a side-effect import (alias `_`).
fn go_imports(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> Vec<ImportFact> {
    let mut out = Vec::new();
    for spec in query_nodes("(import_spec) @s", lang, root, src) {
        let source_specifier = spec
            .child_by_field_name("path")
            .map(|p| string_inner(p, src))
            .unwrap_or_default();
        let name_node = spec.child_by_field_name("name");
        let (items, glob) = match name_node.map(|n| (n.kind(), n)) {
            Some(("dot", _)) => (Vec::new(), true),
            Some((_, n)) => (
                vec![ImportedItem {
                    name: source_specifier.clone(),
                    alias: Some(ntext(n, src)),
                }],
                false,
            ),
            None => (Vec::new(), false),
        };
        out.push(ImportFact {
            source_specifier,
            items,
            glob,
            re_export: false,
        });
    }
    out
}

// ============================================================================
// XB2 — scope tree + binding extraction (lexical resolution substrate)
// ============================================================================
//
// A third sibling read-only surface over an [`OpenedFile`], parallel to
// [`extract_facts`] (imports/exports) and [`crate::tier2`] (analysis sites):
// the lexical SCOPE tree, the local BINDINGS each scope introduces, and every
// value-identifier OCCURRENCE tagged with its enclosing scope and a typed
// [`Resolution`]. Hylla joins occurrences to their binding/import to draw
// use→def edges without an LSP.
//
// NEVER-GUESS invariant (RISKY unit): resolution is [`Resolution::Undecidable`]
// unless a lexical fact is certain — a local binding is visible in the scope
// chain (→ [`Resolution::LocalBinding`]) or the name matches a definite local
// import name AND no local binding shadows it (→ [`Resolution::ImportedName`]).
// A glob/wildcard import contributes NO specific names (its members are
// unknown), so a name reachable only through a glob stays Undecidable — never
// coerced to ImportedName. Local bindings ALWAYS win over imports (shadowing),
// so an inner `let x`/`const x` masking an imported `x` resolves LocalBinding.
//
// POSITION MATTERS for SEQUENTIAL bindings ([`Binding::hoisted`] = false): a
// `let`/`const`/`:=` name is visible only at/after its own `start_byte`, so a
// use lexically BEFORE the declaration falls through to the import/Undecidable
// (never a false LocalBinding for a pre-declaration use). Hoisted names
// (items, params, JS `var`/function decls, pattern binds) stay position-free.
// The governing rule of this whole surface: a DEFINITE variant is emitted ONLY
// when structurally certain; every residual ambiguity degrades to Undecidable —
// a false Undecidable is acceptable, a false definite is the bug class.
//
// Like facts, scopes are DISPOSABLE: re-derived per parse over the SAME bytes
// the outline anchors on (a private native re-parse — the parser port does not
// expose native nodes), never anchored, never mutating the declaration outline.
// Binding recognition is precise for Rust (the shadowing acceptance language)
// and best-effort for TS/JS, Python, and Go; an unrecognized construct is
// SKIPPED, never a fabricated binding.

/// The lexical category of a [`Scope`]. `Module` = file root or a Rust `mod` /
/// Python `class` namespace; `Function` = a function/method/closure/lambda
/// body (parameters bind here); `Block` = a nested brace/statement block
/// (Rust/Go/TS block scoping; Python has none, so Python never emits `Block`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// File root, a Rust `mod`, or a Python `class` namespace.
    Module,
    /// A function/method/closure/lambda body; parameters bind in this scope.
    Function,
    /// A nested brace/statement block (Rust/Go/TS); Python emits none.
    Block,
}

/// One local name introduced in a [`Scope`], carrying the byte range of the
/// binding IDENTIFIER (not the whole declaration) so a host can anchor the def
/// site. Best-effort across languages: only definite value bindings are
/// recorded (see the module note on the never-guess invariant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// The bound name as written at the declaration site.
    pub name: String,
    /// Inclusive start byte of the binding identifier.
    pub start_byte: usize,
    /// Exclusive end byte of the binding identifier.
    pub end_byte: usize,
    /// Position-free visibility. `true` = the binding is visible everywhere in
    /// its scope regardless of source order (hoisted: item/fn/struct/class/type
    /// names, parameters, JS `var`, pattern binds); `false` = SEQUENTIAL — the
    /// binding is visible only AT or AFTER its own `start_byte` (Rust `let`,
    /// Go `:=`, TS `let`/`const`), so a same-name use lexically BEFORE it falls
    /// through to an outer scope / import (F5 positional rule — never a false
    /// definite for a pre-declaration use).
    pub hoisted: bool,
}

/// One node of the lexical scope tree. `parent` is the enclosing scope's arena
/// index (`None` only for the module root at index 0); `bindings` are the local
/// names this scope introduces (parameters live on the function scope, `let`/
/// `const`/`:=` on the block/module scope that contains them). Byte/line ranges
/// span the scope-introducing grammar node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Arena index of the enclosing scope; `None` for the module root only.
    pub parent: Option<usize>,
    /// The lexical category of this scope.
    pub kind: ScopeKind,
    /// Inclusive start byte of the scope's grammar node.
    pub start_byte: usize,
    /// Exclusive end byte of the scope's grammar node.
    pub end_byte: usize,
    /// 1-based start line of the scope's grammar node.
    pub start_line: usize,
    /// 1-based end line of the scope's grammar node.
    pub end_line: usize,
    /// Local names introduced directly in this scope.
    pub bindings: Vec<Binding>,
    /// POISONED names (G7 backstop): names an UNKNOWN/unclassifiable pattern in
    /// this scope MIGHT bind — we cannot prove they are bound OR free, so any
    /// occurrence of such a name in this scope degrades to
    /// [`Resolution::Undecidable`] (never a false `ImportedName`, never a false
    /// `LocalBinding`). Resolution-only; NOT part of the wire contract
    /// (`#[serde(skip)]`) — a disposable conservative marker, not durable state.
    /// Sourced from: any pattern-position node kind outside the exhaustive
    /// per-grammar binding table (macro-expanded Rust patterns, Python `match`
    /// case patterns, walrus targets, future grammar additions).
    #[serde(skip)]
    poison: HashSet<String>,
}

/// The typed resolution of an identifier [`Occurrence`]. Exactly one variant
/// per occurrence (the property XB2 locks). `LocalBinding` carries the arena
/// index of the nearest enclosing scope whose bindings contain the name (the
/// shadowing winner); `ImportedName` = a definite local import name with no
/// shadowing local binding; `Undecidable` = neither could be established — the
/// honest, never-coerced default (unknown globals, glob-import members, dynamic
/// names, attribute/field references).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Resolution {
    /// Resolves to a local binding visible in the scope chain at `scope`.
    LocalBinding {
        /// Arena index of the scope that owns the winning binding.
        scope: usize,
    },
    /// Resolves to a definite local import name (no shadowing local binding).
    ImportedName,
    /// Neither a visible local binding nor a definite import — never coerced.
    Undecidable,
}

/// One value-identifier occurrence: the name, its enclosing [`Scope`] index,
/// its byte/line position, and the typed [`Resolution`]. Every plain
/// `identifier` node is an occurrence, INCLUDING binding sites (a binding site
/// resolves to its own `LocalBinding`) — so the resolution property holds over
/// the whole identifier stream with no special-casing. EXCEPTION (never-guess):
/// non-referencing identifier positions are DROPPED rather than mis-resolved
/// (see [`is_non_occurrence`]) — buffering one coerces a false `ImportedName`
/// (the XB2/XH3 false-definite bug class). Only a WHOLE bare callee/use stays an
/// occurrence; import binding-sites and qualified-path segments never do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    /// The identifier text.
    pub name: String,
    /// Arena index of the enclosing scope.
    pub scope: usize,
    /// Inclusive start byte of the identifier.
    pub start_byte: usize,
    /// Exclusive end byte of the identifier.
    pub end_byte: usize,
    /// 1-based line of the identifier.
    pub line: usize,
    /// The typed, never-guessed resolution.
    pub resolution: Resolution,
}

/// The scope + binding + occurrence extraction over one file. `scopes[0]` is
/// always the module root; every [`Occurrence::scope`] and
/// [`Resolution::LocalBinding::scope`] indexes into `scopes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeTree {
    /// Arena of scopes; index 0 = module root, `parent` links form the tree.
    pub scopes: Vec<Scope>,
    /// Every value-identifier occurrence, in source order.
    pub occurrences: Vec<Occurrence>,
}

/// Result of scope extraction. `Unsupported` is the typed NEVER-GUESS opt-out
/// for a language with no scope model — callers must branch on it, never treat
/// it as an empty tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scopes {
    /// The language has a scope model; the tree was built.
    Supported(ScopeTree),
    /// No scope model for this language — nothing is guessed.
    Unsupported {
        /// The unsupported language.
        lang: Lang,
    },
}

/// Extracts the lexical [`Scopes`] of a parsed file. Returns
/// [`Scopes::Unsupported`] for any language outside the first-drop set (Rust,
/// TS/TSX/JS, Python, Go). Re-parses `opened`'s bytes with a private native
/// parser (queries/native walking need native nodes the port hides), so scopes
/// are consistent with the outline the host anchors on.
///
/// Two passes: (1) a scope-aware walk builds the scope arena, records each
/// scope's bindings, and buffers every identifier occurrence with its enclosing
/// scope; (2) each occurrence is resolved against the scope chain then the
/// definite-import name set. Local bindings take precedence over imports.
pub fn extract_scopes(opened: &OpenedFile) -> Scopes {
    let lang = opened.lang;
    let Some(ts_lang) = grammar(lang) else {
        return Scopes::Unsupported { lang };
    };
    let src = opened.tree.source.as_slice();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_lang)
        .expect("registered grammar sets");
    let module_scope = |root: TsNode| Scope {
        parent: None,
        kind: ScopeKind::Module,
        start_byte: root.start_byte(),
        end_byte: root.end_byte(),
        start_line: root.start_position().row + 1,
        end_line: root.end_position().row + 1,
        bindings: Vec::new(),
        poison: HashSet::new(),
    };
    let Some(tree) = parser.parse(src, None) else {
        // Total parse failure over already-parsed bytes is unreachable in
        // practice; degrade to a bare module scope rather than fabricate.
        return Scopes::Supported(ScopeTree {
            scopes: vec![Scope {
                parent: None,
                kind: ScopeKind::Module,
                start_byte: 0,
                end_byte: src.len(),
                start_line: 1,
                end_line: 1,
                bindings: Vec::new(),
                poison: HashSet::new(),
            }],
            occurrences: Vec::new(),
        });
    };
    let root = tree.root_node();
    // Names declared `global`/`nonlocal` anywhere (Python): such names never
    // create a function-scope binding (F6) — an assignment to them targets the
    // module namespace, so a local binding would be a false definite. Computed
    // once, conservatively corpus-wide (over-exclusion only ever demotes to
    // Undecidable, never fabricates a definite).
    let globals = if matches!(lang, Lang::Python) {
        python_global_names(root, src, &ts_lang)
    } else {
        HashSet::new()
    };
    let mut scopes = vec![module_scope(root)];
    // Buffered occurrences: (name, enclosing_scope, start_byte, end_byte, line).
    let mut raw: Vec<(String, usize, usize, usize, usize)> = Vec::new();
    for i in 0..root.named_child_count() {
        if let Some(c) = root.named_child(i as u32) {
            walk_scopes(lang, c, 0, src, &mut scopes, &mut raw, &globals);
        }
    }
    let imports = imported_local_names(opened);
    let occurrences = raw
        .into_iter()
        .map(|(name, scope, start_byte, end_byte, line)| {
            let resolution = resolve_name(&name, scope, start_byte, &scopes, &imports);
            Occurrence {
                name,
                scope,
                start_byte,
                end_byte,
                line,
                resolution,
            }
        })
        .collect();
    Scopes::Supported(ScopeTree {
        scopes,
        occurrences,
    })
}

/// Scope-aware descent. Adds `node`'s enclosing-scope bindings (declaration
/// names + `let`/`assignment`/`:=` locals) to `cur`, opens a child scope when
/// `node` introduces one (binding its parameters there), buffers `node` as an
/// occurrence when it is a value identifier, then recurses over named children
/// under the (possibly new) scope.
fn walk_scopes(
    lang: Lang,
    node: TsNode,
    cur: usize,
    src: &[u8],
    scopes: &mut Vec<Scope>,
    raw: &mut Vec<(String, usize, usize, usize, usize)>,
    globals: &HashSet<String>,
) {
    let kind = node.kind();
    // Names that bind in the ENCLOSING scope, or — for JS `function`/`class`
    // declarations — HOIST to the nearest enclosing Function/Module scope (F2).
    if let Some(b) = decl_name_binding(lang, node, src) {
        let target = decl_hoist_target(lang, scopes, cur);
        scopes[target].bindings.push(b);
    }
    // Simple locals (`let`, `x =`, `x :=`, `var`): each routed to its owning
    // scope with the correct positional/hoisted flag (`var` hoists, F2).
    collect_local_bindings(lang, node, src, cur, scopes, globals);
    // Open a child scope when this node introduces one; params + pattern binds
    // (F1) land THERE.
    let child = match scope_kind_of(lang, kind) {
        Some(k) => {
            let id = scopes.len();
            // G5: a Python FUNCTION scope's lexical parent SKIPS enclosing class
            // scopes — Python class-body names are invisible to methods (they
            // need `self.`/`ClassName.`). Re-parenting to the nearest non-class
            // ancestor keeps class vars out of the method's resolution chain
            // while preserving module/import + outer-function visibility.
            let parent = if matches!(lang, Lang::Python) && k == ScopeKind::Function {
                python_nonclass_ancestor(scopes, cur)
            } else {
                cur
            };
            scopes.push(Scope {
                parent: Some(parent),
                kind: k,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                bindings: Vec::new(),
                poison: HashSet::new(),
            });
            collect_param_bindings(lang, node, src, id, scopes);
            collect_pattern_scope_bindings(lang, node, src, id, scopes);
            collect_self_name_binding(lang, node, src, id, scopes);
            id
        }
        None => cur,
    };
    // A value identifier is an occurrence in its enclosing (`cur`) scope, UNLESS
    // it is a non-referencing position (import/use span, qualified-path segment,
    // Python attribute/keyword/global-nonlocal, TSX intrinsic tag, TS
    // index-signature param — F4/F6/XH3), which is dropped so it can never coerce
    // a false definite.
    if kind == "identifier" && !is_non_occurrence(lang, node, src) {
        raw.push((
            ntext(node, src),
            cur,
            node.start_byte(),
            node.end_byte(),
            node.start_position().row + 1,
        ));
    }
    for i in 0..node.named_child_count() {
        if let Some(c) = node.named_child(i as u32) {
            // Rust `if let` else/else-if branch: the let-binding does NOT reach
            // the alternative, so recurse it under the ENCLOSING scope (never
            // the if-scope) — prevents a false LocalBinding in the else arm.
            let into = if is_rust_if_alternative(lang, node, c) {
                cur
            } else {
                child
            };
            walk_scopes(lang, c, into, src, scopes, raw, globals);
        }
    }
}

/// Whether `child` is the `alternative` (else / else-if) branch of a Rust
/// `if_expression` — the one child of an if-scope that must NOT see the
/// `if let` pattern binding.
fn is_rust_if_alternative(lang: Lang, node: TsNode, child: TsNode) -> bool {
    matches!(lang, Lang::Rust)
        && node.kind() == "if_expression"
        && node.child_by_field_name("alternative") == Some(child)
}

/// The scope a declaration NAME binds into. JS `function`/`class` declarations
/// hoist to the nearest enclosing Function/Module scope (F2); every other
/// declaration name binds in the enclosing (`cur`) scope.
fn decl_hoist_target(lang: Lang, scopes: &[Scope], cur: usize) -> usize {
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => hoist_target(scopes, cur),
        _ => cur,
    }
}

/// Walks up from `cur` to the nearest Function or Module scope (skipping
/// Blocks) — the hoist destination for JS `var` and `function`/`class` decls.
fn hoist_target(scopes: &[Scope], mut cur: usize) -> usize {
    loop {
        match scopes[cur].kind {
            ScopeKind::Function | ScopeKind::Module => return cur,
            ScopeKind::Block => match scopes[cur].parent {
                Some(p) => cur = p,
                None => return cur,
            },
        }
    }
}

/// The nearest ancestor of `cur` that is NOT a Python class scope — the lexical
/// parent a Python function/lambda/comprehension scope must link to (G5). A
/// Python `class` body opens a [`ScopeKind::Module`] scope with a `Some` parent
/// (the real module root is the only `Module` with `parent == None`); such class
/// scopes are SKIPPED so their bindings never enter a nested method's resolution
/// chain, while the enclosing module + any enclosing function stay visible.
/// Returns `cur` unchanged when `cur` is not itself a class scope.
fn python_nonclass_ancestor(scopes: &[Scope], mut cur: usize) -> usize {
    while scopes[cur].kind == ScopeKind::Module && scopes[cur].parent.is_some() {
        // A `Module`-kind scope with a parent is a class body; skip it.
        cur = scopes[cur].parent.expect("class scope has a parent");
    }
    cur
}

/// Python `global`/`nonlocal` declaration names, gathered corpus-wide. A name
/// here is never turned into a function-scope binding (F6).
fn python_global_names(root: TsNode, src: &[u8], lang: &tree_sitter::Language) -> HashSet<String> {
    let mut set = HashSet::new();
    let q = "[(global_statement (identifier) @n) (nonlocal_statement (identifier) @n)]";
    for n in query_nodes(q, lang, root, src) {
        set.insert(ntext(n, src));
    }
    set
}

/// Whether an `identifier` node is a NON-referencing position that must NOT
/// become an [`Occurrence`] — a name that, if buffered, would coerce a false
/// [`Resolution::ImportedName`] on a same-spelling import collision (the XB2/XH3
/// false-definite bug class). FIVE drop classes, all four grammars, extending
/// the F4 attribute-drop precedent (partitioning matches contract §8d):
///
/// 1. **import/use span** — ANY identifier inside an import/use/from-import
///    declaration (Rust `use_declaration` incl. a fn-body-scoped
///    `use crate::x::h;` AND `extern_crate_declaration`; TS/JS `import_statement`
///    PLUS a re-export-`from` specifier/namespace name; Python `import_*`; Go
///    `import_declaration`/`import_spec`): an import path/name/alias identifier
///    is a binding-site, never a body use — buffering it drew a self/wrong-target
///    Extracted edge to a same-named import.
/// 2. **qualified-path TRAILING segment** — a Rust `scoped_identifier` `name`
///    (`crate::internal::helper()` → `internal`,`helper`) or a Python
///    `dotted_name` segment (`import a.b.c`): a trailing path segment cannot
///    resolve lexically, so it is dropped rather than coerced. The path HEAD is a
///    real reference and STAYS — Rust `T::default()`/`a::b` head (`path` field, a
///    generic param / module), Python attribute-chain head `a` in `a.b.c` (but a
///    `match`/`case a.b:` value-pattern parses as `dotted_name`, so its head is
///    conservatively dropped too — no false definite; documented in §8d.1).
/// 3. **Python `attribute` field** (`obj.json` → `json`) — a trailing attribute
///    name is a member access, never a value reference (F4).
/// 4. **Python `keyword_argument` name + `global`/`nonlocal` names**
///    (`f(json=1)` → `json`; `global x`) — argument labels / declaration names,
///    not uses (F4/F6).
/// 5. **TSX intrinsic tag + TS index-signature parameter** — a lowercase JSX
///    element name (`<div>`/`</div>`) is an HTML/SVG intrinsic (an uppercase
///    component tag `<Foo/>` IS a reference and stays); `[key: string]: T`'s
///    `key` is a type-position placeholder that binds no value.
fn is_non_occurrence(lang: Lang, node: TsNode, src: &[u8]) -> bool {
    // (1) Anything inside an import/use declaration span — all grammars.
    if within_import_decl(lang, node) {
        return true;
    }
    let Some(p) = node.parent() else {
        return false;
    };
    match lang {
        // (2a) Rust qualified-path TRAILING segment: the `name` field of a
        // `scoped_identifier` (`crate::internal::helper` → `internal`,`helper`
        // are each a `name`). The path HEAD (`T` in `T::default()`, `a` in
        // `a::b`) is the `path` field — a real reference (generic param / module)
        // that STAYS, mirroring the Python attribute-head rule.
        Lang::Rust => {
            p.kind() == "scoped_identifier" && p.child_by_field_name("name") == Some(node)
        }
        // (2b) Python attribute field / keyword-arg name / global-nonlocal
        // (F4/F6) PLUS `dotted_name` import-path segments.
        Lang::Python => match p.kind() {
            "attribute" => p.child_by_field_name("attribute") == Some(node),
            "keyword_argument" => p.child_by_field_name("name") == Some(node),
            "global_statement" | "nonlocal_statement" => true,
            "dotted_name" => true,
            _ => false,
        },
        // (2c) TS/JS re-export-`from` specifier/namespace name + TSX intrinsic
        // tag name + TS index-signature parameter name.
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            within_reexport_from(node)
                || is_tsx_intrinsic_tag(p, node, src)
                || is_ts_index_signature_param(p, node)
        }
        // Go qualified access is a `selector_expression` whose `field` is a
        // `field_identifier` (never an occurrence); the `operand` head is a real
        // bare use and stays. Nothing extra beyond the import span.
        _ => false,
    }
}

/// Whether `node` lies anywhere inside an import/use/from-import declaration.
/// Climbs ancestors (import decls are statements whose subtree holds only path/
/// list/alias nodes, so an import identifier reaches its decl in a few hops
/// before any non-import identifier climbs to the root).
fn within_import_decl(lang: Lang, node: TsNode) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if is_import_decl_kind(lang, n.kind()) {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// The per-grammar import/use declaration node kinds whose contained
/// identifiers are binding-sites, never body uses.
fn is_import_decl_kind(lang: Lang, kind: &str) -> bool {
    match lang {
        // `extern_crate_declaration` (`extern crate alloc;` / `… as a;`) is a
        // crate-binding site, never a body use — its `name`/`alias` idents drop.
        Lang::Rust => matches!(kind, "use_declaration" | "extern_crate_declaration"),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => kind == "import_statement",
        Lang::Python => matches!(
            kind,
            "import_statement" | "import_from_statement" | "future_import_statement"
        ),
        Lang::Go => matches!(kind, "import_declaration" | "import_spec"),
        _ => false,
    }
}

/// Whether `node` lies inside a TS/JS re-export-`from` statement — an
/// `export_statement` carrying a `source` field (`export {x} from './m'`,
/// `export * as ns from './m'`, `export {default as x} from './m'`). Such
/// `export_clause` specifier / `namespace_export` identifiers NAME another
/// module's binding, never a local value use, so buffering one coerced a false
/// `ImportedName` on a same-spelling import collision (the XH3 residual class).
/// A SOURCELESS `export {x}` re-exports a LOCAL binding, so its `x` IS a real
/// reference and stays (this helper returns `false` — no `source` field). Climbs
/// ancestors; export statements do not nest, so the FIRST `export_statement`
/// hit decides.
fn within_reexport_from(node: TsNode) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "export_statement" {
            return n.child_by_field_name("source").is_some();
        }
        cur = n.parent();
    }
    false
}

/// Whether `node` (parent `p`) is a lowercase JSX element name — an HTML/SVG
/// INTRINSIC tag (`<div>`/`</div>`/`<span/>`), not a value reference. Uppercase
/// component tags (`<Foo/>`) and member tags (`<a.b/>`, a `member_expression`,
/// not an `identifier`) are real references and return `false`.
fn is_tsx_intrinsic_tag(p: TsNode, node: TsNode, src: &[u8]) -> bool {
    if !matches!(
        p.kind(),
        "jsx_opening_element" | "jsx_closing_element" | "jsx_self_closing_element"
    ) {
        return false;
    }
    if p.child_by_field_name("name") != Some(node) {
        return false;
    }
    // Intrinsic iff the tag spelling starts with an ASCII lowercase letter (the
    // JSX/React convention; uppercase or `_`/`$` = component reference).
    src.get(node.start_byte())
        .is_some_and(u8::is_ascii_lowercase)
}

/// Whether `node` (parent `p`) is the parameter NAME of a TS `index_signature`
/// (`[key: string]: T`) — a type-position placeholder that binds no value.
fn is_ts_index_signature_param(p: TsNode, node: TsNode) -> bool {
    p.kind() == "index_signature" && p.child_by_field_name("name") == Some(node)
}

/// The [`ScopeKind`] a grammar node opens, or `None` when it opens no scope.
/// Per-language tables; Python has no block scoping so only function/lambda and
/// module/class open scopes there.
fn scope_kind_of(lang: Lang, kind: &str) -> Option<ScopeKind> {
    match lang {
        Lang::Rust => match kind {
            "mod_item" => Some(ScopeKind::Module),
            "function_item" | "closure_expression" => Some(ScopeKind::Function),
            // GENERAL RULE (L2): EVERY generic-carrying item opens its own Block
            // scope so its type/const generic params (`collect_param_bindings`)
            // bind for the WHOLE item body and nowhere else — uniform across
            // `impl` / `struct` / `enum` / `trait` / `union`, matching `fn`. A
            // param use in a method/default body / discriminant / const default
            // resolves to the param, never a same-spelling import; the item's own
            // name still binds in the ENCLOSING scope (`decl_name_binding`, run
            // before the scope opens).
            "impl_item" | "struct_item" | "enum_item" | "trait_item" | "union_item" => {
                Some(ScopeKind::Block)
            }
            // Pattern-introducing constructs each open a Block scope so their
            // bound names (F1) are visible in the arm/body and nowhere else.
            "block" | "match_arm" | "for_expression" | "if_expression" | "while_expression" => {
                Some(ScopeKind::Block)
            }
            _ => None,
        },
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match kind {
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition" => Some(ScopeKind::Function),
            "statement_block" | "class_body" => Some(ScopeKind::Block),
            _ => None,
        },
        Lang::Python => match kind {
            "function_definition" | "lambda" => Some(ScopeKind::Function),
            // Comprehensions/generators open a Function-like scope; their
            // `for_in_clause` targets bind there, not in the outer scope (F3).
            "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression" => Some(ScopeKind::Function),
            "class_definition" => Some(ScopeKind::Module),
            _ => None,
        },
        Lang::Go => match kind {
            "function_declaration" | "method_declaration" | "func_literal" => {
                Some(ScopeKind::Function)
            }
            // GENERAL RULE (Go spec: "each if/for/switch is its own implicit
            // block"): EVERY statement kind carrying a header/init `:=` binding
            // position opens its own Block scope, so that init var scopes to the
            // statement — a post-statement / sibling-branch use of the same
            // spelling resolves to the outer/import binding, never the header var
            // (which would be a false `LocalBinding`). Uniform across:
            //   if_statement                — `if x := …; cond {}` init
            //   expression_switch_statement — `switch x := …; x {}` init
            //   type_switch_statement       — `switch x := y.(type) {}` alias
            //   for_statement               — `range`/`for_clause` `:=` var
            //   communication_case          — `case v := <-ch` receive alias
            //     (per-case: invisible to sibling cases)
            // COMPANION RULE — each switch/select CLAUSE is itself an implicit
            // block (Go spec): a `:=` in a case/default BODY scopes to that
            // clause, so it is invisible to sibling clauses AND after the
            // switch/select (a same-spelling use elsewhere resolves to the
            // outer/import binding, never the clause-body var):
            //   expression_case             — `case 1: x := …` body binding
            //   type_case                   — `case int: x := …` body binding
            //   default_case                — `default: x := …` body binding
            //     (expression/type switch AND select share `default_case`)
            "block"
            | "if_statement"
            | "expression_switch_statement"
            | "type_switch_statement"
            | "for_statement"
            | "communication_case"
            | "expression_case"
            | "type_case"
            | "default_case" => Some(ScopeKind::Block),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a Rust `function_item`/`const_item` is an ASSOCIATED item — a direct
/// member of an `impl`/`trait` body (a `declaration_list` whose parent is an
/// `impl_item`/`trait_item`). Assoc fn/const names are path-only (`Self::`/
/// `Type::`), so `decl_name_binding` suppresses their value binding. A
/// module-level item (`declaration_list` under `mod_item`, or `source_file`)
/// returns `false` — it binds by bare name.
fn is_rust_assoc_item(node: TsNode) -> bool {
    node.parent()
        .filter(|p| p.kind() == "declaration_list")
        .and_then(|p| p.parent())
        .is_some_and(|gp| matches!(gp.kind(), "impl_item" | "trait_item"))
}

/// A declaration whose NAME binds in the enclosing scope (a `fn`/`struct`/
/// `class`/`type` name is visible to siblings), or `None`. Parameters and
/// `let`/`:=` locals are handled elsewhere.
fn decl_name_binding(lang: Lang, node: TsNode, src: &[u8]) -> Option<Binding> {
    let binds = match lang {
        // Associated `fn`/`const` NAMES (direct members of an `impl`/`trait`
        // body) are reachable ONLY through `Self::`/`Type::` paths — a bare use
        // in a sibling/default method body resolves to the import/outer binding,
        // never the assoc item — so they must NOT bind into the item's L2 Block
        // scope (that would be a false `LocalBinding`). Struct/enum/union GENERIC
        // params still bind there via `collect_param_bindings` (unaffected).
        // Module-level `fn`/`const` (under `source_file`/`mod_item`) DO bind by
        // bare name.
        Lang::Rust => {
            matches!(
                node.kind(),
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "const_item"
                    | "static_item"
                    | "type_item"
                    | "mod_item"
                    | "union_item"
            ) && !(matches!(node.kind(), "function_item" | "const_item")
                && is_rust_assoc_item(node))
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => matches!(
            node.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
        ),
        Lang::Python => matches!(node.kind(), "function_definition" | "class_definition"),
        Lang::Go => matches!(
            node.kind(),
            "function_declaration" | "method_declaration" | "type_spec"
        ),
        _ => false,
    };
    if !binds {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    // Declaration names are order-independent within their scope (an item is
    // visible to siblings above its definition) — always hoisted.
    Some(binding_of(name, src, true))
}

/// Adds the `let`/`assignment`/`var`/`:=`-style locals `node` introduces to the
/// owning scope. Only the binding-position field is scanned so a right-hand-side
/// use is never mistaken for a binding. Routing + positional semantics:
/// - Rust `let` → CURRENT scope, SEQUENTIAL (visible at/after its site, F5).
/// - JS `var` → nearest Function/Module scope, HOISTED; JS `let`/`const` →
///   current scope, SEQUENTIAL (F2/F5).
/// - Python `assignment`/`for`/`for_in_clause` targets → current scope, HOISTED
///   (Python has no TDZ), MINUS any name declared `global`/`nonlocal` (F6). A
///   walrus `named_expression` target (`:=`, G6) and a `match` `case_clause`
///   pattern (G7) are POISONED (→ `Undecidable`) in the current scope, never a
///   false definite: walrus binding-vs-comprehension-outer scoping (PEP 572) and
///   structural-pattern capture-vs-value are both unclassifiable from syntax.
/// - Go `:=`/`var`/`const` → current scope, SEQUENTIAL (F5).
fn collect_local_bindings(
    lang: Lang,
    node: TsNode,
    src: &[u8],
    cur: usize,
    scopes: &mut [Scope],
    globals: &HashSet<String>,
) {
    let mut tmp = Vec::new();
    let mut poison = Vec::new();
    let mut target = cur;
    match lang {
        Lang::Rust => {
            // H1: route the `let` PATTERN through the exhaustive Rust pattern
            // collector, never a naive ident harvest — `let E(v) = g()` binds
            // only `v` (the `E` tuple_struct_pattern `type` path is excluded),
            // `let P { y, .. }` binds the shorthand `y`. A future/unknown pattern
            // kind poisons (never a naive false binding).
            if node.kind() == "let_declaration"
                && let Some(pat) = node.child_by_field_name("pattern")
            {
                collect_rust_pattern_idents(pat, src, &mut tmp, &mut poison, false);
            }
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            if node.kind() == "variable_declarator" {
                // `var` (parent `variable_declaration`) hoists + is position-free;
                // `let`/`const` (parent `lexical_declaration`) stays sequential.
                // G3: the `name` target may DESTRUCTURE (`const { x } = o`,
                // `const [a] = xs`) — route through the exhaustive TS pattern
                // collector so shorthand-property + rest + default-left names bind
                // (a naive harvest would miss shorthand-property idents entirely).
                let is_var = node
                    .parent()
                    .is_some_and(|p| p.kind() == "variable_declaration");
                if let Some(name) = node.child_by_field_name("name") {
                    collect_ts_pattern_idents(name, src, &mut tmp, &mut poison, is_var);
                }
                if is_var {
                    target = hoist_target(scopes, cur);
                }
            }
        }
        Lang::Python => match node.kind() {
            // H2: assignment/for targets through the exhaustive Python target
            // collector — `obj.attr = 1` and `d[key] = 1` bind NOTHING (attribute
            // + subscript targets skipped), tuple/list/starred leaves bind, an
            // unknown target kind poisons.
            "assignment" => {
                if let Some(l) = node.child_by_field_name("left") {
                    collect_python_target_idents(l, src, &mut tmp, &mut poison);
                }
            }
            "for_statement" | "for_in_clause" => {
                if let Some(l) = node.child_by_field_name("left") {
                    collect_python_target_idents(l, src, &mut tmp, &mut poison);
                }
            }
            // GENERAL RULE (K1, unified): `x += 1` (augmented_assignment) binds its
            // bare-name target in EVERY scope where plain `assignment` binds —
            // function, class, and module — with NO scope special-case, mirroring
            // the `"assignment"` arm above. CPython compiles a bare-name augmented
            // target to STORE_FAST (function → whole-function-local, `UnboundLocal-
            // Error` proves it is not the outer/import) or STORE_NAME (class body →
            // a class attribute; module → the module global) — all three CREATE the
            // name, so a subsequent same-scope read resolves to it, never a false
            // `ImportedName`. Route the `left` through the H2 target collector: a
            // bare name binds (HOISTED); an `obj.x` / `d[k]` target binds nothing;
            // an unknown target poisons.
            "augmented_assignment" => {
                if let Some(l) = node.child_by_field_name("left") {
                    collect_python_target_idents(l, src, &mut tmp, &mut poison);
                }
            }
            // H5: `except X as e` / `with open(p) as f` — ONLY the `as` alias
            // binds; the bare exception type / context expression is a USE, never
            // a binding. Route the `as_pattern` alias through the H2 collector.
            "except_clause" | "except_group_clause" => {
                if let Some(v) = node.child_by_field_name("value")
                    && v.kind() == "as_pattern"
                {
                    collect_python_target_idents(v, src, &mut tmp, &mut poison);
                }
            }
            "with_statement" => collect_python_with_targets(node, src, &mut tmp, &mut poison),
            // J3: PEP 695 `type X = …` alias binds `X` at module/class scope
            // (HOISTED, order-independent). The `left` field is a `type` wrapper
            // whose LEADING identifier is the alias name (`type X[T] = …` → `X`;
            // a following `[T]` type-param / RHS are uses, not the name). The RHS
            // `right` type is walked normally as uses.
            "type_alias_statement" => {
                if let Some(l) = node.child_by_field_name("left")
                    && let Some(name) = first_identifier(l)
                {
                    tmp.push(binding_of(name, src, true));
                }
            }
            // G6: walrus `(name := expr)` — degrade the target to Undecidable
            // (poison) so a later use is NEVER a false `ImportedName`; the exact
            // PEP-572 scope (comprehension binds in the enclosing function) is
            // beyond what syntax alone proves.
            "named_expression" => {
                if let Some(n) = node.child_by_field_name("name") {
                    poison.push(ntext(n, src));
                }
            }
            // G7: `match`/`case` structural patterns mix captures, value matches,
            // and class patterns — unclassifiable from syntax, so poison every
            // name in the case pattern (the pattern children, NOT the guard/body).
            "case_clause" => {
                let guard = node.child_by_field_name("guard");
                let consequence = node.child_by_field_name("consequence");
                for i in 0..node.named_child_count() {
                    if let Some(c) = node.named_child(i as u32)
                        && Some(c) != guard
                        && Some(c) != consequence
                    {
                        poison_pattern_idents(c, src, &mut poison);
                    }
                }
            }
            _ => {}
        },
        Lang::Go => match node.kind() {
            "short_var_declaration" => {
                if let Some(l) = node.child_by_field_name("left") {
                    collect_go_target_idents(l, src, &mut tmp, &mut poison, false);
                }
            }
            // H3/L1: `switch x := v.(type)` guard alias — bound in the type-switch's
            // OWN block scope by `collect_param_bindings` (targeting the new scope,
            // not the enclosing `cur`), so it is invisible after the switch. Handled
            // there because `collect_local_bindings` runs against the enclosing
            // scope before the child scope opens; see `scope_kind_of`.
            // H3/K2: `for i := range xs` binds `i` (SEQUENTIAL); `for i = range xs`
            // (operator `=`) is a REASSIGNMENT and binds nothing — distinguished
            // by the `:=` token between the `left` and `right` fields. K2: the
            // enclosing `for_statement` opens a Block scope, so `cur` is the LOOP
            // scope — `i` is invisible after the loop (a post-loop use of the same
            // spelling resolves to the outer/import binding, never the loop var).
            "range_clause" => {
                if left_assign_is_define(node, src)
                    && let Some(l) = node.child_by_field_name("left")
                {
                    collect_go_target_idents(l, src, &mut tmp, &mut poison, false);
                }
            }
            // J1/K2: `select { case v := <-ch: … }` — a `receive_statement` under
            // a `communication_case`. The `:=` form binds its `left`; the `=` form
            // REASSIGNS (binds nothing) and a `send_statement` (`ch <- v`) binds
            // nothing. Same `:=`-token discriminator as the range clause (both
            // carry `left`/`right` fields). K2: `communication_case` opens its own
            // Block scope (`scope_kind_of`), so `cur` here is the PER-CASE scope —
            // the alias is invisible to sibling cases (a case-2 use of case-1's
            // spelling resolves to the package/import, never case-1's binding).
            "receive_statement" => {
                if left_assign_is_define(node, src)
                    && let Some(l) = node.child_by_field_name("left")
                {
                    collect_go_target_idents(l, src, &mut tmp, &mut poison, false);
                }
            }
            "var_spec" | "const_spec" => collect_go_spec_names(node, src, &mut tmp),
            _ => {}
        },
        _ => {}
    }
    if matches!(lang, Lang::Python) && !globals.is_empty() {
        tmp.retain(|b| !globals.contains(&b.name));
    }
    scopes[target].bindings.extend(tmp);
    scopes[cur].poison.extend(poison);
}

/// Go `var`/`const` spec names: the leading identifier(s), excluding the `type`
/// and `value` fields (so `var x = y` binds `x`, never the RHS `y`). SEQUENTIAL
/// (a use before the spec falls through — F5).
fn collect_go_spec_names(node: TsNode, src: &[u8], out: &mut Vec<Binding>) {
    let typ = node.child_by_field_name("type");
    let value = node.child_by_field_name("value");
    for i in 0..node.named_child_count() {
        let Some(c) = node.named_child(i as u32) else {
            continue;
        };
        if c.kind() == "identifier" && Some(c) != typ && Some(c) != value {
            out.push(binding_of(c, src, false));
        }
    }
}

/// Binds the pattern names of a freshly opened Rust pattern-scope (F1) into
/// scope `id`: a `match_arm`'s `pattern`, a `for_expression`'s `pattern`, or an
/// `if`/`while let`'s `let_condition` pattern. Pattern binds are HOISTED within
/// their arm/body scope (the pattern precedes every use). Non-Rust languages and
/// non-pattern scopes add nothing.
fn collect_pattern_scope_bindings(
    lang: Lang,
    node: TsNode,
    src: &[u8],
    id: usize,
    scopes: &mut [Scope],
) {
    if !matches!(lang, Lang::Rust) {
        return;
    }
    let pat = match node.kind() {
        "match_arm" | "for_expression" => node.child_by_field_name("pattern"),
        "if_expression" | "while_expression" => node
            .child_by_field_name("condition")
            .filter(|c| c.kind() == "let_condition")
            .and_then(|c| c.child_by_field_name("pattern")),
        _ => None,
    };
    if let Some(pat) = pat {
        let mut out = Vec::new();
        let mut poison = Vec::new();
        collect_rust_pattern_idents(pat, src, &mut out, &mut poison, true);
        scopes[id].bindings.extend(out);
        scopes[id].poison.extend(poison);
    }
}

/// J2: a NAMED function/class EXPRESSION binds its OWN name in its INNER scope
/// only — `const f = function foo() { foo() }` and `const C = class Bar { m() {
/// Bar } }` let the body reference the name recursively, but the name is
/// INVISIBLE to the enclosing scope (unlike a declaration, which
/// `decl_name_binding` hoists outward). TS/JS only; other langs add nothing.
///
/// - `function_expression`/`generator_function` open their OWN `Function` scope
///   `id`; the self-name binds there (visible in the body block child).
/// - a class EXPRESSION (`class`, NOT `class_declaration`) opens NO scope of its
///   own — its `class_body` child opens the `Block` scope `id`, so the self-name
///   binds there. An ANONYMOUS expression (no `name` field) binds nothing.
///
/// HOISTED (the self-name is visible throughout the body). A false positive is
/// impossible: the name is the construct's own binding identifier.
fn collect_self_name_binding(
    lang: Lang,
    node: TsNode,
    src: &[u8],
    id: usize,
    scopes: &mut [Scope],
) {
    if !matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript) {
        return;
    }
    let named = match node.kind() {
        "function_expression" | "generator_function" => Some(node),
        // The class-expression self-name binds in the class_body scope; a
        // `class_declaration`/`abstract_class_declaration` is EXCLUDED (its name
        // is already bound in the enclosing scope by `decl_name_binding`).
        "class_body" => node.parent().filter(|p| p.kind() == "class"),
        _ => None,
    };
    if let Some(n) = named
        && let Some(name) = n.child_by_field_name("name")
    {
        scopes[id].bindings.push(binding_of(name, src, true));
    }
}

/// Collects the BINDING identifiers of a Rust pattern (F1/G1/G7), EXHAUSTIVE
/// against the tree-sitter-rust `pattern` supertype (node-types.json, 0.24.2:
/// `_`, `_literal_pattern`, `captured_pattern`, `const_block`, `generic_pattern`,
/// `identifier`, `macro_invocation`, `mut_pattern`, `or_pattern`, `range_pattern`,
/// `ref_pattern`, `reference_pattern`, `remaining_field_pattern`,
/// `scoped_identifier`, `slice_pattern`, `struct_pattern`, `tuple_pattern`,
/// `tuple_struct_pattern`; plus `match_pattern`, `field_pattern`,
/// `shorthand_field_identifier`).
///
/// - BIND leaves: `identifier`; `shorthand_field_identifier` (G1 — `match p { P
///   { y, .. } }` binds `y`; grammar wraps it in a `field_pattern` whose `name`
///   is the shorthand, no `pattern` field).
/// - NON-binding leaves (SKIP): wildcard `_`, literals, path segments
///   (`scoped_identifier`/`type_identifier`/`field_identifier`), `range_pattern`,
///   `remaining_field_pattern` (`..`), `generic_pattern` (a `Path::<T>` head),
///   `const_block` (a `const { … }` value match — its inner idents are USES,
///   recorded by the main walk, never binds).
/// - RECURSE composites (constructor `type` field and `match_pattern` `condition`
///   guard EXCLUDED — a variant/type path and a guard expression bind nothing):
///   `tuple_pattern`, `tuple_struct_pattern`, `struct_pattern`, `field_pattern`,
///   `or_pattern`, `ref_pattern`, `reference_pattern`, `mut_pattern`,
///   `slice_pattern`, `captured_pattern` (`x @ sub`), `match_pattern`.
/// - UNKNOWN (G7 poison): any other kind (`macro_invocation` = a macro-expanded
///   pattern, future grammar) is unclassifiable — its contained identifiers are
///   POISONED (→ `Undecidable`), NEVER silently unbound (which would leak to a
///   false import). Converts every future gap from false-definite to conservative.
///
/// `hoisted` = the bound names' positional visibility: `true` for pattern-scope
/// / param binds (position-free within their arm/body), `false` for a Rust
/// `let` pattern (SEQUENTIAL — F5: visible only at/after its own site).
fn collect_rust_pattern_idents(
    pat: TsNode,
    src: &[u8],
    out: &mut Vec<Binding>,
    poison: &mut Vec<String>,
    hoisted: bool,
) {
    match pat.kind() {
        "identifier" | "shorthand_field_identifier" => out.push(binding_of(pat, src, hoisted)),
        // Wildcards, literals, path heads, ranges, `..`, const-value matches:
        // bind nothing (contained idents, if any, are uses tracked elsewhere).
        "_"
        | "_literal_pattern"
        | "integer_literal"
        | "float_literal"
        | "string_literal"
        | "char_literal"
        | "boolean_literal"
        | "negative_literal"
        | "scoped_identifier"
        | "scoped_type_identifier"
        | "type_identifier"
        | "field_identifier"
        | "range_pattern"
        | "remaining_field_pattern"
        | "generic_pattern"
        | "const_block"
        | "self"
        | "crate"
        | "super"
        | "metavariable"
        | "mutable_specifier" => {}
        // Composite patterns: recurse the sub-patterns; exclude the constructor
        // `type` path and the `match_pattern` guard `condition` (never binds).
        "tuple_pattern"
        | "tuple_struct_pattern"
        | "struct_pattern"
        | "field_pattern"
        | "or_pattern"
        | "ref_pattern"
        | "reference_pattern"
        | "mut_pattern"
        | "slice_pattern"
        | "captured_pattern"
        | "match_pattern" => {
            let type_field = pat.child_by_field_name("type");
            let cond_field = pat.child_by_field_name("condition");
            for i in 0..pat.named_child_count() {
                if let Some(c) = pat.named_child(i as u32)
                    && Some(c) != type_field
                    && Some(c) != cond_field
                {
                    collect_rust_pattern_idents(c, src, out, poison, hoisted);
                }
            }
        }
        // G7 backstop: unknown pattern kind → poison every contained identifier.
        _ => poison_pattern_idents(pat, src, poison),
    }
}

/// G7 poison harvester: records EVERY `identifier`/`shorthand_field_identifier`
/// text at or below `node` into `poison`. Used when a pattern-position node kind
/// is outside the exhaustive per-grammar table — we cannot tell binds from
/// path/guard uses, so every name is degraded to [`Resolution::Undecidable`]
/// (conservative: a false `Undecidable`, never a false definite).
fn poison_pattern_idents(node: TsNode, src: &[u8], poison: &mut Vec<String>) {
    if matches!(
        node.kind(),
        "identifier"
            | "shorthand_field_identifier"
            | "shorthand_property_identifier_pattern"
            | "shorthand_property_identifier"
    ) {
        poison.push(ntext(node, src));
        return;
    }
    for i in 0..node.named_child_count() {
        if let Some(c) = node.named_child(i as u32) {
            poison_pattern_idents(c, src, poison);
        }
    }
}

/// Collects the BINDING identifiers of a TS/JS destructuring pattern (G3),
/// EXHAUSTIVE against the tree-sitter-typescript/javascript `pattern` supertype
/// (`array_pattern`, `identifier`, `member_expression`, `object_pattern`,
/// `rest_pattern`, `subscript_expression`, `undefined`, `non_null_expression`)
/// plus `object_pattern`/`array_pattern` children (`pair_pattern`,
/// `shorthand_property_identifier_pattern`, `object_assignment_pattern`,
/// `assignment_pattern`).
///
/// - BIND leaves: `identifier`; `shorthand_property_identifier_pattern` (G3 —
///   `const { x } = o` / `function f({ x })` binds `x`).
/// - RECURSE containers: `object_pattern`, `array_pattern`, `rest_pattern`
///   (`...rest`).
/// - `pair_pattern` (`{ k: v }`): recurse the `value` field ONLY — the `key`
///   (`property_identifier`/string/number) names a property, NOT a binding (G3).
/// - `assignment_pattern` / `object_assignment_pattern` (`{ x = 1 }`, `[a = 1]`):
///   recurse the `left` field ONLY — the `right` default is an EXPRESSION (a
///   USE), never a binding (G3).
/// - NON-binding leaves (SKIP): `member_expression`/`subscript_expression`/
///   `non_null_expression` (assignment-destructuring targets, not new binds),
///   `property_identifier`/`shorthand_property_identifier`/keys, `this`,
///   `undefined`, literals.
/// - UNKNOWN (G7 poison): any other kind degrades its contained identifiers to
///   [`Resolution::Undecidable`], never silently unbound.
///
/// `hoisted` sets each bound name's positional visibility: `true` for
/// parameters (position-free), `false` for `let`/`const` destructuring targets
/// (SEQUENTIAL — F5; `var` passes `true`).
fn collect_ts_pattern_idents(
    pat: TsNode,
    src: &[u8],
    out: &mut Vec<Binding>,
    poison: &mut Vec<String>,
    hoisted: bool,
) {
    match pat.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            out.push(binding_of(pat, src, hoisted))
        }
        "object_pattern" | "array_pattern" | "rest_pattern" => {
            for i in 0..pat.named_child_count() {
                if let Some(c) = pat.named_child(i as u32) {
                    collect_ts_pattern_idents(c, src, out, poison, hoisted);
                }
            }
        }
        // `{ key: value }` — only the value binds.
        "pair_pattern" => {
            if let Some(v) = pat.child_by_field_name("value") {
                collect_ts_pattern_idents(v, src, out, poison, hoisted);
            }
        }
        // Defaulted target `left = right` — only the left binds; `right` = a use.
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(l) = pat.child_by_field_name("left") {
                collect_ts_pattern_idents(l, src, out, poison, hoisted);
            }
        }
        // Assignment-destructuring targets, property keys, literals: bind nothing.
        "member_expression"
        | "subscript_expression"
        | "non_null_expression"
        | "undefined"
        | "this"
        | "property_identifier"
        | "shorthand_property_identifier"
        | "private_property_identifier"
        | "computed_property_name"
        | "number"
        | "string" => {}
        // G7 backstop: unknown pattern kind → poison contained identifiers.
        _ => poison_pattern_idents(pat, src, poison),
    }
}

/// Binds a scope-introducing node's PARAMETERS into the freshly created scope
/// `id`. Rust scans each `parameter`'s `pattern`; other languages scan the
/// parameter container conservatively, taking only definite name positions.
fn collect_param_bindings(lang: Lang, node: TsNode, src: &[u8], id: usize, scopes: &mut [Scope]) {
    let mut out = Vec::new();
    let mut poison = Vec::new();
    match lang {
        Lang::Rust => {
            // `parameters` (fn) OR `closure_parameters` (closure) — the field is
            // named `parameters` for both. Each child is a typed `parameter`
            // (with a `pattern` field) OR — for closures — a BARE pattern
            // (`|x|`, `|(a,b)|`, `|Point{x,y}|`; G2). Route BOTH through the
            // exhaustive Rust pattern collector so composite untyped closure
            // params bind (F1 leaves + G1 shorthand + G7 poison), never dropped.
            if let Some(params) = node.child_by_field_name("parameters") {
                for i in 0..params.named_child_count() {
                    let Some(p) = params.named_child(i as u32) else {
                        continue;
                    };
                    match p.kind() {
                        "parameter" => {
                            if let Some(pat) = p.child_by_field_name("pattern") {
                                collect_rust_pattern_idents(pat, src, &mut out, &mut poison, true);
                            }
                        }
                        // Bare closure pattern (identifier or composite).
                        _ => collect_rust_pattern_idents(p, src, &mut out, &mut poison, true),
                    }
                }
            }
            // GENERAL RULE (L2): EVERY generic parameter with an identifier
            // surface binds HOISTED in its item's own scope — TYPE params
            // (`type_parameter`, name = `type_identifier`) AND CONST params
            // (`const_parameter`, name = `identifier`), uniformly at fn / impl /
            // struct / enum / trait / union level (every item carrying a
            // `type_parameters` field — `scope_kind_of` opens a scope for each).
            // Binding is strictly SAFER than the falsified "no collision surface"
            // claim: a type param used in VALUE position (`T::default()` path head,
            // an associated-fn/const call) IS a tracked `identifier` occurrence and
            // would mis-resolve to a same-spelling import without the binding.
            // `lifetime_parameter` / `metavariable` / `attribute_item` carry no
            // identifier surface and are skipped (lifetimes excluded per L2).
            if let Some(tps) = node.child_by_field_name("type_parameters") {
                for i in 0..tps.named_child_count() {
                    if let Some(c) = tps.named_child(i as u32)
                        && matches!(c.kind(), "type_parameter" | "const_parameter")
                        && let Some(name) = c.child_by_field_name("name")
                    {
                        out.push(binding_of(name, src, true));
                    }
                }
            }
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            // H4: `catch (e) { … }` — the catch parameter binds in the handler's
            // `statement_block` scope (THIS scope), shadowing an imported `e`.
            // Route through the TS pattern collector so a destructuring catch
            // (`catch ({ code })`) binds too. HOISTED within the handler block.
            if node.kind() == "statement_block"
                && let Some(parent) = node.parent()
                && parent.kind() == "catch_clause"
                && let Some(param) = parent.child_by_field_name("parameter")
            {
                collect_ts_pattern_idents(param, src, &mut out, &mut poison, true);
            }
            // `formal_parameters` children are TS `required/optional_parameter`
            // (typed wrapper, `pattern` field) OR — for JS + destructuring — a
            // BARE pattern subtype (`object_pattern`/`array_pattern`/`rest_pattern`/
            // `assignment_pattern`/`identifier`). Route BOTH through the
            // exhaustive TS/JS pattern collector (G3: shorthand + rest + default
            // left + pair value), never a naive harvest (which misses shorthand
            // property idents and over-binds default RHS uses).
            let params = node
                .child_by_field_name("parameters")
                .or_else(|| node.child_by_field_name("parameter"));
            if let Some(params) = params {
                for i in 0..params.named_child_count() {
                    let Some(p) = params.named_child(i as u32) else {
                        continue;
                    };
                    match p.kind() {
                        "required_parameter" | "optional_parameter" => {
                            if let Some(pat) = p.child_by_field_name("pattern") {
                                collect_ts_pattern_idents(pat, src, &mut out, &mut poison, true);
                            }
                        }
                        _ => collect_ts_pattern_idents(p, src, &mut out, &mut poison, true),
                    }
                }
            }
        }
        Lang::Python => {
            // J3: PEP 695 `def f[T]` / `class C[T]` — the `type_parameters` field
            // holds a `type_parameter` node whose `type` children each lead with
            // the parameter NAME (`T`, `T: bound`→`T`, `*Ts`→`Ts`, `T = def`→`T`;
            // the leading identifier is always the name, a following bound/default
            // is a use). Bound in the definition scope (HOISTED — visible across
            // signature + body).
            if let Some(tp) = node.child_by_field_name("type_parameters") {
                for i in 0..tp.named_child_count() {
                    if let Some(c) = tp.named_child(i as u32)
                        && let Some(name) = first_identifier(c)
                    {
                        out.push(binding_of(name, src, true));
                    }
                }
            }
            if let Some(params) = node.child_by_field_name("parameters") {
                for i in 0..params.named_child_count() {
                    let Some(p) = params.named_child(i as u32) else {
                        continue;
                    };
                    match p.kind() {
                        "identifier" => out.push(binding_of(p, src, true)),
                        "default_parameter" => {
                            if let Some(n) = p.child_by_field_name("name") {
                                collect_python_target_idents(n, src, &mut out, &mut poison);
                            }
                        }
                        "typed_parameter"
                        | "typed_default_parameter"
                        | "list_splat_pattern"
                        | "dictionary_splat_pattern" => {
                            if let Some(inner) = p.named_child(0)
                                && inner.kind() == "identifier"
                            {
                                out.push(binding_of(inner, src, true));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Lang::Go => {
            // G4: a Go function scope binds THREE `parameter_list`s — the method
            // `receiver` (`func (m T) …` binds `m`), the ordinary `parameters`,
            // AND NAMED `result`s (`func f() (r int)` binds `r`). All three are
            // `parameter_list`s of `parameter_declaration`s; a bare-type result
            // (`_simple_type`) binds nothing. Named returns are visible
            // throughout the body → HOISTED, like params.
            for field in ["receiver", "parameters", "result"] {
                if let Some(list) = node.child_by_field_name(field)
                    && list.kind() == "parameter_list"
                {
                    collect_go_param_list(list, src, &mut out);
                }
            }
            // L1: `switch fmt := x.(type) {}` — the guard alias binds in the
            // type-switch's OWN block scope (`scope_kind_of` now opens one), NOT
            // the enclosing scope. The alias is collected HERE (targeting `id`),
            // like a param of the new scope, so a post-switch use of the same
            // spelling resolves to the outer/import binding, never the alias. The
            // `alias` field is present ONLY for the `:=` guard form.
            if node.kind() == "type_switch_statement"
                && let Some(a) = node.child_by_field_name("alias")
            {
                collect_go_target_idents(a, src, &mut out, &mut poison, false);
            }
        }
        _ => {}
    }
    scopes[id].bindings.extend(out);
    scopes[id].poison.extend(poison);
}

/// Binds the identifier names of a Go `parameter_list` (receiver / params /
/// named results) into `out`, excluding each declaration's `type` field so
/// `func f(a, b int)` binds `a`,`b` and never the type `int`. HOISTED (params
/// and named returns are visible throughout the function body).
fn collect_go_param_list(list: TsNode, src: &[u8], out: &mut Vec<Binding>) {
    for i in 0..list.named_child_count() {
        let Some(p) = list.named_child(i as u32) else {
            continue;
        };
        if !matches!(
            p.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        ) {
            continue;
        }
        let typ = p.child_by_field_name("type");
        for j in 0..p.named_child_count() {
            if let Some(n) = p.named_child(j as u32)
                && n.kind() == "identifier"
                && Some(n) != typ
            {
                out.push(binding_of(n, src, true));
            }
        }
    }
}

/// A [`Binding`] from an identifier node with the given positional/`hoisted`
/// visibility (see [`Binding::hoisted`]).
fn binding_of(n: TsNode, src: &[u8], hoisted: bool) -> Binding {
    Binding {
        name: ntext(n, src),
        start_byte: n.start_byte(),
        end_byte: n.end_byte(),
        hoisted,
    }
}

/// The LEADING `identifier` at or below `node` in source order (pre-order,
/// leftmost-first descent), or `None`. J3 uses it to pull the declared NAME out
/// of a PEP 695 `type_parameter` `type` child / type-alias `left` `type`
/// wrapper, where the name is ALWAYS the leading token and any following
/// `: bound` / `= default` / `[params]` are uses — so the leftmost identifier
/// is precisely the binding, never the bound.
fn first_identifier(node: TsNode) -> Option<TsNode> {
    if node.kind() == "identifier" {
        return Some(node);
    }
    for i in 0..node.named_child_count() {
        if let Some(c) = node.named_child(i as u32)
            && let Some(found) = first_identifier(c)
        {
            return Some(found);
        }
    }
    None
}

/// Collects the BINDING identifiers of a Python assignment/for/with/except
/// TARGET (H2/H5), EXHAUSTIVE against the tree-sitter-python target grammar.
/// Every Python local binding routes HERE — there is no naive "harvest every
/// identifier" path (H0), so a non-binding target position can never fabricate
/// a false definite and a future/unknown target kind poisons conservatively.
/// All Python locals are HOISTED (no TDZ), so bindings are position-free.
///
/// - BIND leaves: `identifier` (a plain `_` blank included — harmless).
/// - RECURSE containers: `tuple`/`tuple_pattern`/`list`/`list_pattern`/
///   `pattern_list`/`expression_list` (`a, b = …`, `[a, b] = …`) and the splat
///   `list_splat`/`list_splat_pattern`/`starred` (`*rest`).
/// - `as_pattern` (`… as e`, H5): recurse ONLY the `alias` (`as_pattern_target`)
///   — the aliased expression (`open(p)`, an exception type) is a USE, not a
///   binding.
/// - SKIP (bind NOTHING): `attribute` (`obj.attr = 1` binds nothing — `obj` is a
///   use, `attr` a field) and `subscript` (`d[key] = 1` binds nothing — `d`/`key`
///   are uses). A false binding here was the H2 bug class.
/// - UNKNOWN → poison (degrade contained idents to `Undecidable`, never unbound).
fn collect_python_target_idents(
    node: TsNode,
    src: &[u8],
    out: &mut Vec<Binding>,
    poison: &mut Vec<String>,
) {
    match node.kind() {
        "identifier" => out.push(binding_of(node, src, true)),
        "tuple" | "tuple_pattern" | "list" | "list_pattern" | "pattern_list"
        | "expression_list" | "list_splat" | "list_splat_pattern" | "starred"
        | "as_pattern_target" => {
            for i in 0..node.named_child_count() {
                if let Some(c) = node.named_child(i as u32) {
                    collect_python_target_idents(c, src, out, poison);
                }
            }
        }
        // `… as e`: only the alias binds; the aliased expression is a use.
        "as_pattern" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                collect_python_target_idents(alias, src, out, poison);
            }
        }
        // Attribute/subscript targets introduce NO new local name.
        "attribute" | "subscript" => {}
        // Unknown target kind: conservative poison, never a naive false binding.
        _ => poison_pattern_idents(node, src, poison),
    }
}

/// Binds the `as` targets of a Python `with_statement` (H5): each
/// `with_clause` → `with_item` whose `value` is an `as_pattern` binds its alias
/// (`with open(p) as f` binds `f`); a bare `with open(p):` binds nothing. Routes
/// through [`collect_python_target_idents`] so tuple aliases bind too.
fn collect_python_with_targets(
    node: TsNode,
    src: &[u8],
    out: &mut Vec<Binding>,
    poison: &mut Vec<String>,
) {
    for i in 0..node.named_child_count() {
        let Some(clause) = node.named_child(i as u32) else {
            continue;
        };
        if clause.kind() != "with_clause" {
            continue;
        }
        for j in 0..clause.named_child_count() {
            let Some(item) = clause.named_child(j as u32) else {
                continue;
            };
            if item.kind() != "with_item" {
                continue;
            }
            if let Some(v) = item.child_by_field_name("value")
                && v.kind() == "as_pattern"
            {
                collect_python_target_idents(v, src, out, poison);
            }
        }
    }
}

/// Collects the BINDING identifiers of a Go `:=`/range/type-switch TARGET
/// (H3), EXHAUSTIVE against the Go left-hand grammar. Every Go short-var local
/// routes HERE — no naive harvest path (H0). `hoisted` = false (Go `:=` is
/// SEQUENTIAL).
///
/// - BIND leaves: `identifier` (blank `_` included — harmless).
/// - RECURSE: `expression_list` (`a, b := …`).
/// - SKIP (bind NOTHING): `selector_expression` (`x.y`) and `index_expression`
///   (`x[i]`) — assignable targets that introduce no new local name.
/// - UNKNOWN → poison, never a naive false binding.
fn collect_go_target_idents(
    node: TsNode,
    src: &[u8],
    out: &mut Vec<Binding>,
    poison: &mut Vec<String>,
    hoisted: bool,
) {
    match node.kind() {
        "identifier" => out.push(binding_of(node, src, hoisted)),
        "expression_list" => {
            for i in 0..node.named_child_count() {
                if let Some(c) = node.named_child(i as u32) {
                    collect_go_target_idents(c, src, out, poison, hoisted);
                }
            }
        }
        "selector_expression" | "index_expression" => {}
        _ => poison_pattern_idents(node, src, poison),
    }
}

/// Whether a Go `left := right`-shaped clause DEFINES (`:=`) rather than
/// REASSIGNS (`=`) — used for both `range_clause` (`for i := range xs`) and
/// `receive_statement` (`case v := <-ch`, J1). The `:=` vs `=` token sits
/// between the `left` and `right` fields; its absence means reassignment
/// (binds nothing).
fn left_assign_is_define(node: TsNode, src: &[u8]) -> bool {
    let (Some(l), Some(r)) = (
        node.child_by_field_name("left"),
        node.child_by_field_name("right"),
    ) else {
        return false;
    };
    src.get(l.end_byte()..r.start_byte())
        .and_then(|s| std::str::from_utf8(s).ok())
        .is_some_and(|s| s.contains(":="))
}

/// The set of DEFINITE local import names for `opened` — the names an import
/// binds into the local namespace (`alias` when renamed, else the last path
/// segment of the imported symbol/module). Glob/wildcard imports contribute
/// NOTHING (their members are unknown), upholding the never-guess invariant.
fn imported_local_names(opened: &OpenedFile) -> HashSet<String> {
    let mut set = HashSet::new();
    let Facts::Supported { imports, .. } = extract_facts(opened) else {
        return set;
    };
    for imp in imports {
        if imp.glob {
            // A wildcard brings unknown names; never fabricate specific ones.
            continue;
        }
        if imp.items.is_empty() {
            // A bare module import binds a local module/package name.
            if let Some(seg) = last_segment(&imp.source_specifier) {
                set.insert(seg);
            }
            continue;
        }
        for it in imp.items {
            let local = it.alias.unwrap_or(it.name);
            if local == "*" || local.ends_with("::*") || local.ends_with("*") {
                continue;
            }
            if let Some(seg) = last_segment(&local) {
                set.insert(seg);
            }
        }
    }
    set
}

/// The final path segment of an import name/specifier (`a::b::C` → `C`,
/// `a/b/c` → `c`, `os.path` → `path`), the local name it binds; `None` when
/// empty.
fn last_segment(s: &str) -> Option<String> {
    let seg = s.rsplit(['/', '.', ':']).next().unwrap_or(s);
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

/// Resolves the occurrence of `name` at byte `use_byte` in scope `scope` to a
/// typed [`Resolution`]: the nearest enclosing scope with a matching VISIBLE
/// binding wins (shadowing); otherwise a definite local import name; otherwise
/// [`Resolution::Undecidable`]. A binding is visible iff it is [hoisted] OR its
/// `start_byte` is at/before `use_byte` (F5 positional rule: a sequential
/// `let`/`const`/`:=` is invisible to a use lexically before it, which then
/// falls through — never a false LocalBinding). Local bindings ALWAYS take
/// precedence over imports.
///
/// [hoisted]: Binding::hoisted
///
/// G7 POISON: at each scope in the chain, a POISONED `name` (an unknown/
/// unclassifiable pattern in that scope MIGHT bind it) short-circuits to
/// [`Resolution::Undecidable`] BEFORE the binding + import checks — poison wins
/// over both a same-scope binding (genuinely ambiguous) and any outer import
/// (conservative: a false `Undecidable`, never a false definite). Poison is
/// checked per-level so an inner CERTAIN binding still shadows an outer poison.
fn resolve_name(
    name: &str,
    scope: usize,
    use_byte: usize,
    scopes: &[Scope],
    imports: &HashSet<String>,
) -> Resolution {
    let mut cur = Some(scope);
    while let Some(id) = cur {
        if scopes[id].poison.contains(name) {
            return Resolution::Undecidable;
        }
        if scopes[id]
            .bindings
            .iter()
            .any(|b| b.name == name && (b.hoisted || b.start_byte <= use_byte))
        {
            return Resolution::LocalBinding { scope: id };
        }
        cur = scopes[id].parent;
    }
    if imports.contains(name) {
        Resolution::ImportedName
    } else {
        Resolution::Undecidable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::open_file;

    fn facts_of(name: &str, content: &[u8]) -> Facts {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        extract_facts(&open_file(p.to_str().unwrap()).unwrap())
    }

    fn supported(f: Facts) -> (Vec<ImportFact>, Vec<ExportFact>) {
        match f {
            Facts::Supported { imports, exports } => (imports, exports),
            Facts::Unsupported { lang } => panic!("expected Supported, got Unsupported({lang})"),
        }
    }

    fn find_import<'a>(imports: &'a [ImportFact], source: &str) -> &'a ImportFact {
        imports
            .iter()
            .find(|i| i.source_specifier == source)
            .unwrap_or_else(|| panic!("no import for {source} in {imports:?}"))
    }

    #[test]
    fn rust_use_paths_alias_glob_and_pub_reexport() {
        let src = b"use std::collections::HashMap;\nuse a::b::{C, D as E};\nuse foo::*;\nuse x::y as z;\npub use crate::m::Thing;\n";
        let (imports, exports) = supported(facts_of("m.rs", src));
        // Path segments + single item.
        let hm = find_import(&imports, "std::collections");
        assert_eq!(
            hm.items,
            vec![ImportedItem {
                name: "HashMap".into(),
                alias: None
            }]
        );
        assert!(!hm.glob && !hm.re_export);
        // Brace list with an alias.
        let list = find_import(&imports, "a::b");
        assert!(list.items.contains(&ImportedItem {
            name: "C".into(),
            alias: None
        }));
        assert!(list.items.contains(&ImportedItem {
            name: "D".into(),
            alias: Some("E".into())
        }));
        // Glob.
        assert!(find_import(&imports, "foo").glob);
        // Aliased single symbol.
        let aliased = find_import(&imports, "x");
        assert_eq!(
            aliased.items,
            vec![ImportedItem {
                name: "y".into(),
                alias: Some("z".into())
            }]
        );
        // pub use → import.re_export AND an ExportFact re_exported_from.
        let re = imports
            .iter()
            .find(|i| i.re_export)
            .expect("pub use is re_export");
        assert_eq!(re.source_specifier, "crate::m");
        let ex = exports
            .iter()
            .find(|e| e.kind == ExportKind::Reexport && e.name == "Thing")
            .expect("pub use export");
        assert_eq!(ex.re_exported_from.as_deref(), Some("crate::m"));
    }

    #[test]
    fn rust_pub_items_exported_restricted_not() {
        let src = b"pub fn f() {}\npub struct S;\npub enum E {}\npub const K: u8 = 1;\npub mod sub {}\npub trait T {}\npub type A = u8;\nfn priv_fn() {}\npub(crate) fn restricted() {}\n";
        let (_imports, exports) = supported(facts_of("m.rs", src));
        let by = |n: &str| exports.iter().find(|e| e.name == n);
        assert_eq!(by("f").unwrap().kind, ExportKind::Function);
        assert_eq!(by("S").unwrap().kind, ExportKind::Struct);
        assert_eq!(by("E").unwrap().kind, ExportKind::Enum);
        assert_eq!(by("K").unwrap().kind, ExportKind::Const);
        assert_eq!(by("sub").unwrap().kind, ExportKind::Module);
        assert_eq!(by("T").unwrap().kind, ExportKind::Trait);
        assert_eq!(by("A").unwrap().kind, ExportKind::TypeAlias);
        // Non-public visibilities never export.
        assert!(by("priv_fn").is_none(), "private fn not exported");
        assert!(
            by("restricted").is_none(),
            "pub(crate) is not a public export"
        );
    }

    #[test]
    fn ts_imports_default_named_namespace_sideeffect() {
        let src = b"import x from 'm';\nimport { a, b as c } from 'm2';\nimport * as ns from 'm3';\nimport 'side';\n";
        let (imports, _e) = supported(facts_of("m.ts", src));
        let m = find_import(&imports, "m");
        assert_eq!(
            m.items,
            vec![ImportedItem {
                name: "default".into(),
                alias: Some("x".into())
            }]
        );
        let m2 = find_import(&imports, "m2");
        assert!(m2.items.contains(&ImportedItem {
            name: "a".into(),
            alias: None
        }));
        assert!(m2.items.contains(&ImportedItem {
            name: "b".into(),
            alias: Some("c".into())
        }));
        let m3 = find_import(&imports, "m3");
        assert!(m3.glob);
        assert_eq!(
            m3.items,
            vec![ImportedItem {
                name: "*".into(),
                alias: Some("ns".into())
            }]
        );
        let side = find_import(&imports, "side");
        assert!(side.items.is_empty() && !side.glob);
    }

    #[test]
    fn ts_exports_named_from_star_default_and_decls() {
        let src = b"export { a, b as d };\nexport { x as y } from 'm4';\nexport * from 'm5';\nexport default foo;\nexport const K = 1;\nexport function g() {}\nexport class C {}\n";
        let (_i, exports) = supported(facts_of("m.ts", src));
        // Local named export.
        assert!(
            exports
                .iter()
                .any(|e| e.name == "a" && e.re_exported_from.is_none())
        );
        assert!(
            exports
                .iter()
                .any(|e| e.name == "b" && e.alias.as_deref() == Some("d"))
        );
        // Re-export from another module.
        let reexp = exports.iter().find(|e| e.name == "x").unwrap();
        assert_eq!(reexp.kind, ExportKind::Reexport);
        assert_eq!(reexp.re_exported_from.as_deref(), Some("m4"));
        assert_eq!(reexp.alias.as_deref(), Some("y"));
        // Star re-export.
        let star = exports.iter().find(|e| e.name == "*").unwrap();
        assert_eq!(star.re_exported_from.as_deref(), Some("m5"));
        // Default + typed declarations.
        assert!(
            exports
                .iter()
                .any(|e| e.kind == ExportKind::Default && e.name == "default")
        );
        assert_eq!(
            exports.iter().find(|e| e.name == "K").unwrap().kind,
            ExportKind::Variable
        );
        assert_eq!(
            exports.iter().find(|e| e.name == "g").unwrap().kind,
            ExportKind::Function
        );
        assert_eq!(
            exports.iter().find(|e| e.name == "C").unwrap().kind,
            ExportKind::Class
        );
    }

    #[test]
    fn python_import_from_alias_wildcard_relative() {
        let src = b"import os\nimport os.path as p\nfrom a.b import c, d as e\nfrom a import *\nfrom . import x\n";
        let (imports, exports) = supported(facts_of("m.py", src));
        assert!(exports.is_empty(), "python has no export syntax");
        // Plain module import.
        let os = find_import(&imports, "os");
        assert!(os.items.is_empty() && !os.glob);
        // Aliased module import.
        let osp = find_import(&imports, "os.path");
        assert_eq!(
            osp.items,
            vec![ImportedItem {
                name: "os.path".into(),
                alias: Some("p".into())
            }]
        );
        // from-import with alias.
        let ab = find_import(&imports, "a.b");
        assert!(ab.items.contains(&ImportedItem {
            name: "c".into(),
            alias: None
        }));
        assert!(ab.items.contains(&ImportedItem {
            name: "d".into(),
            alias: Some("e".into())
        }));
        // Wildcard from-import.
        let a = imports
            .iter()
            .find(|i| i.source_specifier == "a" && i.glob)
            .expect("from a import *");
        assert!(a.glob);
        // Relative import.
        let rel = find_import(&imports, ".");
        assert!(rel.items.contains(&ImportedItem {
            name: "x".into(),
            alias: None
        }));
    }

    #[test]
    fn go_import_block_single_alias_dot_blank() {
        let src = b"package main\nimport \"fmt\"\nimport (\n  \"os\"\n  a \"b/c\"\n  . \"dot\"\n  _ \"blank\"\n)\n";
        let (imports, exports) = supported(facts_of("m.go", src));
        assert!(exports.is_empty(), "go has no export syntax");
        // Single + block plain imports.
        assert!(
            imports
                .iter()
                .any(|i| i.source_specifier == "fmt" && i.items.is_empty() && !i.glob)
        );
        assert!(imports.iter().any(|i| i.source_specifier == "os"));
        // Aliased import.
        let bc = find_import(&imports, "b/c");
        assert_eq!(
            bc.items,
            vec![ImportedItem {
                name: "b/c".into(),
                alias: Some("a".into())
            }]
        );
        // Dot import = glob.
        assert!(find_import(&imports, "dot").glob);
        // Blank (side-effect) import.
        let blank = find_import(&imports, "blank");
        assert_eq!(
            blank.items,
            vec![ImportedItem {
                name: "blank".into(),
                alias: Some("_".into())
            }]
        );
    }

    #[test]
    fn unsupported_language_is_typed_never_guessed() {
        for (name, body) in [
            ("a.css", &b".x { color: red; }\n"[..]),
            ("n.txt", &b"import x from 'y'\n"[..]),
        ] {
            match facts_of(name, body) {
                Facts::Unsupported { .. } => {}
                Facts::Supported { .. } => panic!("{name} must be Unsupported"),
            }
        }
    }

    #[test]
    fn no_imports_yields_empty_supported_not_unsupported() {
        // A supported language with nothing to extract is Supported (empty),
        // distinct from the Unsupported opt-out.
        let (imports, exports) = supported(facts_of("m.rs", b"fn main() {}\n"));
        assert!(imports.is_empty());
        assert!(exports.is_empty());
    }

    #[test]
    fn nested_module_use_is_found_at_depth() {
        // Queries locate a `use` nested inside a module, not just at file root.
        let src = b"mod inner {\n    use std::io::Write;\n    pub use crate::x::Y;\n}\n";
        let (imports, exports) = supported(facts_of("m.rs", src));
        assert!(imports.iter().any(|i| i.source_specifier == "std::io"));
        assert!(
            exports
                .iter()
                .any(|e| e.name == "Y" && e.kind == ExportKind::Reexport)
        );
    }

    #[test]
    fn rust_nested_use_group_flattens_to_leaves() {
        // Two-level nesting: every LEAF is one item, path-qualified below the
        // group source — NEVER a phantom composite `b::{c::D, e::F}` item.
        let (imports, _e) = supported(facts_of("m.rs", b"use a::{b::{c::D, e::F}};\n"));
        let a = find_import(&imports, "a");
        assert_eq!(
            a.items,
            vec![
                ImportedItem {
                    name: "b::c::D".into(),
                    alias: None
                },
                ImportedItem {
                    name: "b::e::F".into(),
                    alias: None
                },
            ]
        );
        assert!(!a.glob);
        // No raw-subtree phantom leaked in.
        assert!(
            !a.items.iter().any(|it| it.name.contains('{')),
            "no composite subtree name: {:?}",
            a.items
        );
    }

    #[test]
    fn rust_mixed_use_group_bare_and_scoped_leaves() {
        // `{HashMap, hash_map::Entry}`: a bare leaf + a scoped leaf, both under
        // the same group source.
        let (imports, _e) = supported(facts_of(
            "m.rs",
            b"use std::collections::{HashMap, hash_map::Entry};\n",
        ));
        let c = find_import(&imports, "std::collections");
        assert!(c.items.contains(&ImportedItem {
            name: "HashMap".into(),
            alias: None
        }));
        assert!(c.items.contains(&ImportedItem {
            name: "hash_map::Entry".into(),
            alias: None
        }));
    }

    #[test]
    fn rust_nested_glob_in_group_emits_glob_item() {
        // `use tokio::{sync::*, task}`: the nested wildcard survives as a
        // glob-marked item (`sync::*`) alongside the named `task`; the fact's
        // `glob` bool stays false (named siblings coexist).
        let (imports, _e) = supported(facts_of("m.rs", b"use tokio::{sync::*, task};\n"));
        let t = find_import(&imports, "tokio");
        assert!(
            !t.glob,
            "group with a named sibling is not a whole-fact glob"
        );
        assert!(
            t.items.contains(&ImportedItem {
                name: "sync::*".into(),
                alias: None
            }),
            "nested glob signal present: {:?}",
            t.items
        );
        assert!(t.items.contains(&ImportedItem {
            name: "task".into(),
            alias: None
        }));
    }

    #[test]
    fn rust_pub_use_group_reexports_each_leaf() {
        // `pub use crate::{a::B, c::D}`: one Reexport ExportFact per leaf,
        // never a composite — and the import side flags re_export.
        let (imports, exports) = supported(facts_of("m.rs", b"pub use crate::{a::B, c::D};\n"));
        let re = imports.iter().find(|i| i.re_export).expect("pub use");
        assert_eq!(re.source_specifier, "crate");
        for name in ["a::B", "c::D"] {
            assert!(
                exports.iter().any(|e| e.kind == ExportKind::Reexport
                    && e.name == name
                    && e.re_exported_from.as_deref() == Some("crate")),
                "reexport {name} in {exports:?}"
            );
        }
        assert!(
            !exports.iter().any(|e| e.name.contains('{')),
            "no composite reexport name"
        );
    }

    #[test]
    fn rust_use_strips_leading_crate_root_anchor() {
        // `use ::std::io` — the leading `::` global-root anchor is stripped
        // from the source specifier.
        let (imports, _e) = supported(facts_of("m.rs", b"use ::std::io;\n"));
        assert!(
            imports.iter().any(|i| i.source_specifier == "std"
                && i.items
                    == vec![ImportedItem {
                        name: "io".into(),
                        alias: None
                    }]),
            "stripped ::std::io -> source std item io: {imports:?}"
        );
        assert!(
            !imports.iter().any(|i| i.source_specifier.starts_with("::")),
            "no leading :: leaks into a source specifier"
        );
    }

    // -------------------------------------------------- XB2 scopes/bindings --

    fn scopes_of(name: &str, content: &[u8]) -> ScopeTree {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        match extract_scopes(&open_file(p.to_str().unwrap()).unwrap()) {
            Scopes::Supported(t) => t,
            Scopes::Unsupported { lang } => panic!("expected Supported, got Unsupported({lang})"),
        }
    }

    /// All occurrences of `name` whose resolution is a `LocalBinding`.
    fn local_occurrences<'a>(t: &'a ScopeTree, name: &str) -> Vec<&'a Occurrence> {
        t.occurrences
            .iter()
            .filter(|o| o.name == name && matches!(o.resolution, Resolution::LocalBinding { .. }))
            .collect()
    }

    #[test]
    fn rust_shadowing_local_binding_beats_import() {
        // `bar` is imported at module scope but re-bound by `let bar` inside the
        // function block: the inner use MUST resolve LocalBinding, never import.
        let src = b"use a::bar;\nfn f() {\n    let bar = 2;\n    bar;\n}\n";
        let t = scopes_of("m.rs", src);
        // The inner use of `bar` (last occurrence) resolves to a local binding.
        let inner = t
            .occurrences
            .iter()
            .rfind(|o| o.name == "bar")
            .expect("a `bar` occurrence");
        match inner.resolution {
            Resolution::LocalBinding { scope } => {
                assert_eq!(
                    scopes_kind(&t, scope),
                    ScopeKind::Block,
                    "bound in the block"
                )
            }
            other => panic!("inner `bar` must be LocalBinding, got {other:?}"),
        }
        // No `bar` occurrence resolves ImportedName inside the block scope.
        assert!(
            !local_occurrences(&t, "bar").is_empty(),
            "a local `bar` resolution exists"
        );
    }

    /// The [`ScopeKind`] of scope `id`.
    fn scopes_kind(t: &ScopeTree, id: usize) -> ScopeKind {
        t.scopes[id].kind
    }

    #[test]
    fn rust_scope_tree_nests_module_function_block() {
        let t = scopes_of("m.rs", b"fn f() {\n    let x = 1;\n    x;\n}\n");
        assert_eq!(t.scopes[0].kind, ScopeKind::Module);
        assert_eq!(t.scopes[0].parent, None);
        // A Function scope under the module and a Block under the function.
        let func = t
            .scopes
            .iter()
            .position(|s| s.kind == ScopeKind::Function)
            .expect("function scope");
        assert_eq!(t.scopes[func].parent, Some(0));
        let block = t
            .scopes
            .iter()
            .position(|s| s.kind == ScopeKind::Block)
            .expect("block scope");
        assert_eq!(t.scopes[block].parent, Some(func));
        // `x` binds in the block and its use resolves there.
        assert!(t.scopes[block].bindings.iter().any(|b| b.name == "x"));
    }

    #[test]
    fn ts_module_import_resolves_imported_name() {
        // `foo` is imported and used at module scope with no local binding: the
        // use resolves ImportedName (definite import, not Undecidable).
        let t = scopes_of("m.ts", b"import { foo } from 'm';\nfoo();\n");
        assert!(
            t.occurrences
                .iter()
                .any(|o| o.name == "foo" && o.resolution == Resolution::ImportedName),
            "`foo` resolves ImportedName: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "foo" && matches!(o.resolution, Resolution::LocalBinding { .. })),
            "no local binding shadows the import"
        );
    }

    #[test]
    fn ts_shadowing_const_beats_import() {
        let t = scopes_of(
            "m.ts",
            b"import { foo } from 'm';\nfunction f() {\n  const foo = 1;\n  foo;\n}\n",
        );
        // Inside the function/block, `foo` resolves LocalBinding.
        assert!(
            !local_occurrences(&t, "foo").is_empty(),
            "inner `foo` is a local binding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn python_scope_nests_and_no_block_scope() {
        let t = scopes_of("m.py", b"import os\ndef f(x):\n    y = x\n    return y\n");
        // Function scope under module; Python emits NO Block scope.
        assert!(t.scopes.iter().any(|s| s.kind == ScopeKind::Function));
        assert!(
            !t.scopes.iter().any(|s| s.kind == ScopeKind::Block),
            "python has no block scoping"
        );
        // Param `x` binds on the function scope; `y` is a local there too.
        let func = t
            .scopes
            .iter()
            .find(|s| s.kind == ScopeKind::Function)
            .unwrap();
        assert!(func.bindings.iter().any(|b| b.name == "x"), "param x bound");
        assert!(func.bindings.iter().any(|b| b.name == "y"), "local y bound");
    }

    #[test]
    fn go_scope_nests_function_and_block() {
        let t = scopes_of(
            "m.go",
            b"package main\nfunc f(a int) int {\n\tb := a\n\treturn b\n}\n",
        );
        let func = t
            .scopes
            .iter()
            .position(|s| s.kind == ScopeKind::Function)
            .expect("function scope");
        assert_eq!(t.scopes[func].parent, Some(0));
        assert!(
            t.scopes.iter().any(|s| s.kind == ScopeKind::Block),
            "go func body is a block scope"
        );
        // Param `a` on the function scope; `b := a` local in the block.
        assert!(t.scopes[func].bindings.iter().any(|b| b.name == "a"));
        assert!(
            t.scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Block && s.bindings.iter().any(|b| b.name == "b")),
            "b bound in block"
        );
    }

    #[test]
    fn every_occurrence_has_exactly_one_resolution_variant() {
        // Property: for a mixed fixture, every occurrence maps to exactly one
        // Resolution variant (exhaustive match, no panics, non-empty stream).
        for (name, body) in [
            (
                "m.rs",
                &b"use a::b;\nfn f(x: u8) {\n    let y = x;\n    b(y);\n}\n"[..],
            ),
            (
                "m.ts",
                &b"import { g } from 'm';\nfunction h(p) {\n  const q = p;\n  g(q);\n}\n"[..],
            ),
            (
                "m.py",
                &b"import os\ndef f(a):\n    c = a\n    return os\n"[..],
            ),
            (
                "m.go",
                &b"package main\nimport \"fmt\"\nfunc f(a int) {\n\tb := a\n\tfmt.Println(b)\n}\n"
                    [..],
            ),
        ] {
            let t = scopes_of(name, body);
            assert!(!t.occurrences.is_empty(), "{name}: occurrences present");
            for o in &t.occurrences {
                // Every scope index is in range; every LocalBinding target too.
                assert!(o.scope < t.scopes.len(), "{name}: occ scope in range");
                match o.resolution {
                    Resolution::LocalBinding { scope } => {
                        assert!(scope < t.scopes.len(), "{name}: binding scope in range");
                        assert!(
                            t.scopes[scope].bindings.iter().any(|b| b.name == o.name),
                            "{name}: LocalBinding target actually holds `{}`",
                            o.name
                        );
                    }
                    Resolution::ImportedName | Resolution::Undecidable => {}
                }
            }
        }
    }

    #[test]
    fn glob_import_member_stays_undecidable_not_imported() {
        // A name reachable only through a glob import is NEVER coerced to
        // ImportedName (glob members are unknown) — Undecidable is honest.
        let t = scopes_of("m.rs", b"use a::*;\nfn f() {\n    something();\n}\n");
        let occ = t
            .occurrences
            .iter()
            .find(|o| o.name == "something")
            .expect("`something` occurrence");
        assert_eq!(
            occ.resolution,
            Resolution::Undecidable,
            "glob member is Undecidable, not ImportedName"
        );
    }

    #[test]
    fn unsupported_language_scopes_is_typed_never_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.css");
        std::fs::write(&p, b".x { color: red; }\n").unwrap();
        match extract_scopes(&open_file(p.to_str().unwrap()).unwrap()) {
            Scopes::Unsupported { .. } => {}
            Scopes::Supported(_) => panic!("css must be Unsupported"),
        }
    }

    // ------------------------------------ XB2 falsifier probes (F1–F6) --------
    // Each fixture is the resolved form of a falsifier probe that produced a
    // WRONG-definite resolution before the fix. The pre-fix failure is stated
    // per test (the old resolver ignored binding position, pattern binds,
    // hoisting, comprehensions, attribute fields, and global/nonlocal), so each
    // assertion below fails against the pre-fix code and locks the fix.

    /// The nth (0-based) occurrence of `name`, in source order.
    fn nth_occ<'a>(t: &'a ScopeTree, name: &str, n: usize) -> &'a Occurrence {
        t.occurrences
            .iter()
            .filter(|o| o.name == name)
            .nth(n)
            .unwrap_or_else(|| panic!("no #{n} `{name}` occurrence in {:?}", t.occurrences))
    }

    #[test]
    fn f5_rust_use_before_let_is_import_not_local() {
        // p1: `bar()` BEFORE `let bar = 2` (with `use a::bar`). The pre-let use
        // must resolve ImportedName; only the post-let use is LocalBinding.
        // PRE-FIX: the resolver ignored position → the pre-let `bar` resolved
        // LocalBinding (a false definite).
        let t = scopes_of(
            "m.rs",
            b"use a::bar;\nfn f() {\n    bar();\n    let bar = 2;\n    bar;\n}\n",
        );
        // XH3: the import-site `bar` (line 1, `use a::bar`) is dropped, so the
        // pre-let use `bar()` is now the FIRST `bar` occurrence (index 0).
        assert_eq!(
            nth_occ(&t, "bar", 0).resolution,
            Resolution::ImportedName,
            "pre-let use falls through to the import: {:?}",
            t.occurrences
        );
        let last = t.occurrences.iter().rfind(|o| o.name == "bar").unwrap();
        assert!(
            matches!(last.resolution, Resolution::LocalBinding { .. }),
            "post-let use is the local binding"
        );
    }

    #[test]
    fn f2_ts_var_hoists_over_import() {
        // p2: `var x` in an inner block, then `return x` at function level, with
        // `import { x }`. The return must NOT be ImportedName — `var` hoists to
        // the function scope and shadows the import.
        // PRE-FIX: `var` bound in the inner block only → `return x` fell through
        // to the import (false ImportedName).
        let t = scopes_of(
            "m.ts",
            b"import { x } from 'm';\nfunction f() {\n  { var x = 1; }\n  return x;\n}\n",
        );
        let ret = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert!(
            matches!(ret.resolution, Resolution::LocalBinding { .. }),
            "hoisted var shadows the import: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "x" && o.line >= 2 && o.resolution == Resolution::ImportedName),
            "no post-import `x` use resolves the import"
        );
    }

    #[test]
    fn f3_python_comprehension_binds_target() {
        // p4: `[json for json in items]` with `import json`. The comprehension
        // target `json` binds in the comprehension's own scope, so its body use
        // resolves LocalBinding — not the outer import.
        // PRE-FIX: comprehensions opened no scope and `for_in_clause` targets
        // were never collected → `json` resolved ImportedName (false definite).
        let t = scopes_of("m.py", b"import json\nx = [json for json in items]\n");
        assert!(
            !local_occurrences(&t, "json").is_empty(),
            "comprehension `json` is a local binding: {:?}",
            t.occurrences
        );
        // The comprehension opens a Function-like scope.
        assert!(
            t.scopes
                .iter()
                .filter(|s| s.kind == ScopeKind::Function)
                .count()
                >= 1
        );
    }

    #[test]
    fn f4_python_attribute_and_keyword_are_not_occurrences() {
        // p5: `obj.foo` (attribute) and `g(foo=2)` (keyword name) must never
        // emit an occurrence for `foo`, so an imported `foo` is not coerced.
        // PRE-FIX: both `foo` identifiers were occurrences → ImportedName (false
        // definite) on the attribute-access and keyword-argument sites.
        let t = scopes_of("m.py", b"from m import foo\nobj = 1\nobj.foo\ng(foo=2)\n");
        assert!(
            !t.occurrences.iter().any(|o| o.name == "foo" && o.line >= 3),
            "no `foo` occurrence on the attribute/keyword lines: {:?}",
            t.occurrences
        );
        // XH3: the import-site `foo` (line 1) is a binding-site, not a body use,
        // so it is DROPPED too — no `foo` occurrence anywhere in this fixture.
        assert!(
            !t.occurrences.iter().any(|o| o.name == "foo"),
            "import-span `foo` is not a body-use occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn f6_python_global_name_not_function_local() {
        // p6: `global k` then `k = 1` inside a function does NOT create a
        // function-scope binding, so a `k` use never resolves LocalBinding.
        // PRE-FIX: the assignment bound `k` on the function scope → `k`
        // resolved LocalBinding (false definite masking the module global).
        let t = scopes_of(
            "m.py",
            b"import k\ndef g():\n    global k\n    k = 1\n    print(k)\n",
        );
        assert!(
            local_occurrences(&t, "k").is_empty(),
            "no `k` resolves to a function-local binding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn f1_rust_match_arm_pattern_binds() {
        // p8: `match v { Some(a) => a, .. }` with `use m::a`. The arm value `a`
        // is the pattern binding, resolving LocalBinding — the constructor
        // `Some` is never mistaken for a binding.
        // PRE-FIX: match arms opened no scope and patterns were not collected →
        // the arm value `a` resolved ImportedName (false definite).
        let t = scopes_of(
            "m.rs",
            b"use m::a;\nfn f(v: Option<u8>) {\n    let r = match v {\n        Some(a) => a,\n        None => 0,\n    };\n    let _ = r;\n}\n",
        );
        let arm_val = t.occurrences.iter().rfind(|o| o.name == "a").unwrap();
        assert!(
            matches!(arm_val.resolution, Resolution::LocalBinding { .. }),
            "arm value `a` is the pattern binding: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "a" && o.line >= 2 && o.resolution == Resolution::ImportedName),
            "no post-import `a` use resolves the import"
        );
        // `Some` is a path, never a binding.
        assert!(
            t.scopes
                .iter()
                .all(|s| !s.bindings.iter().any(|b| b.name == "Some")),
            "constructor path is not bound"
        );
    }

    #[test]
    fn f1_rust_for_pattern_binds() {
        // p10: `for i in 0..3 { i }` with `use m::i`. The loop body `i` is the
        // pattern binding, resolving LocalBinding — not the import.
        // PRE-FIX: `for` opened no scope and the loop pattern was not collected
        // → the body `i` resolved ImportedName (false definite).
        let t = scopes_of(
            "m.rs",
            b"use m::i;\nfn f() {\n    for i in 0..3 {\n        let _ = i;\n    }\n}\n",
        );
        assert!(
            !local_occurrences(&t, "i").is_empty(),
            "loop var `i` is a local binding: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "i" && o.line >= 2 && o.resolution == Resolution::ImportedName),
            "no post-import `i` use resolves the import"
        );
    }

    #[test]
    fn f1_rust_if_let_binding_does_not_leak_to_else() {
        // if-let branch: the `x` binding reaches the consequence (LocalBinding)
        // but NOT the else arm, where `x` falls through to the import.
        // PRE-FIX (and the naive scope-on-if fix): the else `x` would resolve
        // LocalBinding — a false definite the alternative carve-out prevents.
        let t = scopes_of(
            "m.rs",
            b"use m::x;\nfn f(v: Option<u8>) {\n    if let Some(x) = v {\n        let _ = x;\n    } else {\n        x();\n    }\n}\n",
        );
        // Consequence use is the local binding.
        assert!(
            !local_occurrences(&t, "x").is_empty(),
            "consequence `x` is local: {:?}",
            t.occurrences
        );
        // Else use is the import (last `x` occurrence).
        let else_use = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert_eq!(
            else_use.resolution,
            Resolution::ImportedName,
            "else `x` is the import, not the let binding"
        );
    }

    #[test]
    fn f5_go_redeclare_is_sequential_shadowing() {
        // Go `:=` redeclare: `x := 1` then `x, z := 2, 3` creates TWO sequential
        // `x` bindings in the block; a use BEFORE the first `:=` is not local.
        // PRE-FIX: position was ignored, so the pre-`:=` `x` resolved
        // LocalBinding (false definite).
        let t = scopes_of(
            "m.go",
            b"package main\nfunc f() int {\n\t_ = x\n\tx := 1\n\tx, z := 2, 3\n\treturn x + z\n}\n",
        );
        // First `x` (before `:=`) is NOT a local binding.
        assert!(
            !matches!(
                nth_occ(&t, "x", 0).resolution,
                Resolution::LocalBinding { .. }
            ),
            "pre-`:=` `x` falls through: {:?}",
            t.occurrences
        );
        // The block holds two `x` bindings (redeclare) and one `z`.
        let block = t
            .scopes
            .iter()
            .find(|s| s.kind == ScopeKind::Block)
            .expect("go block scope");
        assert_eq!(
            block.bindings.iter().filter(|b| b.name == "x").count(),
            2,
            "redeclare yields two sequential `x` bindings: {:?}",
            block.bindings
        );
        let ret = t.occurrences.iter().rfind(|o| o.name == "z").unwrap();
        assert!(matches!(ret.resolution, Resolution::LocalBinding { .. }));
    }

    // ------------------------------ XB2 round-2 systematic close (G1–G7) ------
    // Each fixture is a falsifier round-2 probe: an incomplete binding-leaf /
    // scope table produced a wrong resolution. The exhaustive per-grammar tables
    // (built from node-types.json) + the G7 unknown-pattern poison backstop fix
    // the whole class. Every assertion fails against the pre-G1 code.

    #[test]
    fn g1_rust_struct_shorthand_field_pattern_binds() {
        // pA: `match p { P { y, .. } => y }` with `use m::y`. The shorthand field
        // `y` is a BINDING leaf (grammar: field_pattern.name = shorthand_field_
        // identifier, no pattern child), so the arm value resolves LocalBinding.
        // PRE-FIX: shorthand_field_identifier hit the recurse arm, bound nothing
        // → arm `y` resolved ImportedName (false definite).
        let t = scopes_of(
            "m.rs",
            b"use m::y;\nstruct P { y: u8 }\nfn f(p: P) {\n    let r = match p {\n        P { y, .. } => y,\n    };\n    let _ = r;\n}\n",
        );
        let arm_val = t.occurrences.iter().rfind(|o| o.name == "y").unwrap();
        assert!(
            matches!(arm_val.resolution, Resolution::LocalBinding { .. }),
            "shorthand field `y` binds in the arm: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "y" && o.line >= 3 && o.resolution == Resolution::ImportedName),
            "no post-import `y` use resolves the import"
        );
    }

    #[test]
    fn g2_rust_closure_composite_untyped_param_binds() {
        // pB2: `|(a, b)| a + b` with `use m::a`. An untyped COMPOSITE closure
        // param (a bare `tuple_pattern` child of `closure_parameters`, not a
        // `parameter`) binds a,b in the closure scope.
        // PRE-FIX: closure param handling only saw `identifier` + typed
        // `parameter` → composite patterns dropped → `a` resolved ImportedName.
        let t = scopes_of(
            "m.rs",
            b"use m::a;\nfn f() {\n    let g = |(a, b)| a + b;\n    let _ = g;\n}\n",
        );
        assert!(
            !local_occurrences(&t, "a").is_empty(),
            "closure tuple param `a` binds: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "a" && o.line >= 2 && o.resolution == Resolution::ImportedName),
            "no closure-body `a` use resolves the import"
        );
    }

    #[test]
    fn g3_ts_object_destructure_param_binds() {
        // pC: `function f({ x }) { return x; }` with `import { x }`. The shorthand
        // object-destructured PARAM binds x in the function scope.
        // PRE-FIX: `shorthand_property_identifier_pattern` is not kind
        // `identifier`, so `collect_idents` bound nothing → `x` = ImportedName.
        let t = scopes_of(
            "m.ts",
            b"import { x } from 'm';\nfunction f({ x }) {\n  return x;\n}\n",
        );
        assert!(
            !local_occurrences(&t, "x").is_empty(),
            "destructured param `x` binds: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "x" && o.line >= 2 && o.resolution == Resolution::ImportedName),
            "no body `x` use resolves the import"
        );
    }

    #[test]
    fn g3_ts_const_object_destructure_binds() {
        // pD: `const { x } = o` with `import { x }`. Shorthand const destructuring
        // binds x SEQUENTIALLY; the later use resolves LocalBinding, not import.
        // PRE-FIX: same shorthand miss → the post-const `x` = ImportedName.
        let t = scopes_of(
            "m.ts",
            b"import { x } from 'm';\nfunction f(o) {\n  const { x } = o;\n  return x;\n}\n",
        );
        let ret = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert!(
            matches!(ret.resolution, Resolution::LocalBinding { .. }),
            "const `{{ x }}` binds x: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn g3_ts_pair_value_binds_not_key_and_default_left_only() {
        // Precision: `const { a: b = 1 } = o` binds ONLY `b` (the pair VALUE,
        // through the default LEFT) — never the key `a`, never the default RHS.
        let t = scopes_of(
            "m.ts",
            b"function f(o) {\n  const { a: b = 1 } = o;\n  return b;\n}\n",
        );
        let block = t
            .scopes
            .iter()
            .find(|s| s.bindings.iter().any(|x| x.name == "b"));
        assert!(block.is_some(), "pair value `b` binds: {:?}", t.scopes);
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|x| x.name == "a")),
            "pair key `a` is not a binding: {:?}",
            t.scopes
        );
    }

    #[test]
    fn g4_go_receiver_and_named_return_bind() {
        // pE: method receiver `m` and NAMED return `r` both bind in the fn scope
        // (grammar: method_declaration.receiver / .result = parameter_list).
        // PRE-FIX: only the `parameters` field was scanned → receiver + named
        // returns dropped → `m` / `r` resolved import/Undecidable falsely.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"x\"\ntype T struct{}\nfunc (m T) f() (r int) {\n\tr = 1\n\treturn r + m.g()\n}\n",
        );
        assert!(
            !local_occurrences(&t, "r").is_empty(),
            "named return `r` binds: {:?}",
            t.occurrences
        );
        assert!(
            !local_occurrences(&t, "m").is_empty(),
            "receiver `m` binds: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn g5_python_class_var_not_visible_in_method() {
        // Python class-body var `x` is NOT in a method's lexical scope (needs
        // `self.`/`C.`). With `import x`, the method use of `x` must resolve the
        // IMPORT (module scope), never the class-scope LocalBinding.
        // PRE-FIX: the method's parent chain ran through the class scope → the
        // class `x = 1` binding won → LocalBinding (false definite).
        let t = scopes_of(
            "m.py",
            b"import x\nclass C:\n    x = 1\n    def m(self):\n        return x\n",
        );
        let ret = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert_eq!(
            ret.resolution,
            Resolution::ImportedName,
            "method `x` is the import, not the class var: {:?}",
            t.occurrences
        );
        // The class-body `x = 1` binding still resolves LocalBinding (class scope
        // sees itself) — only nested method scopes skip it.
        assert!(
            !local_occurrences(&t, "x").is_empty(),
            "class-body `x` still binds in the class scope"
        );
    }

    #[test]
    fn g6_python_walrus_target_is_not_false_import() {
        // G6: walrus `(n := get())` shadows an imported `n`; degraded to
        // Undecidable (poison) so NO post-walrus `n` use is a false ImportedName.
        // PRE-FIX: named_expression was unhandled → `return n` = ImportedName.
        let t = scopes_of(
            "m.py",
            b"import n\ndef f():\n    if (n := get()):\n        return n\n    return 0\n",
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "n" && o.line >= 3 && o.resolution == Resolution::ImportedName),
            "walrus target `n` is never the import: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn g7_rust_macro_pattern_poisons_to_undecidable() {
        // G7 backstop (Rust exotic): a macro-expanded match pattern `c!(z)` is an
        // UNKNOWN pattern kind (macro_invocation) — its names are poisoned, so the
        // arm value `z` degrades to Undecidable, NEVER a false ImportedName even
        // though `use m::z` is in scope.
        // PRE-FIX: the recurse arm silently bound nothing → `z` = ImportedName.
        let t = scopes_of(
            "m.rs",
            b"use m::z;\nfn f(v: u8) {\n    let _ = match v {\n        c!(z) => z,\n        _ => 0,\n    };\n}\n",
        );
        let arm_val = t.occurrences.iter().rfind(|o| o.name == "z").unwrap();
        assert_eq!(
            arm_val.resolution,
            Resolution::Undecidable,
            "macro-pattern `z` is poisoned to Undecidable: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn g7_python_match_case_pattern_poisons_to_undecidable() {
        // G7 backstop (Python exotic): a `match`/`case` structural pattern mixes
        // captures + value/class matches (unclassifiable from syntax) — its names
        // are poisoned, so `case [a, b]` names degrade to Undecidable, never a
        // false ImportedName despite `import a`.
        // PRE-FIX: match/case was unhandled → `return a` = ImportedName.
        let t = scopes_of(
            "m.py",
            b"import a\ndef f(x):\n    match x:\n        case [a, b]:\n            return a\n",
        );
        assert!(
            !t.occurrences
                .iter()
                .any(|o| o.name == "a" && o.line >= 4 && o.resolution == Resolution::ImportedName),
            "match-case `a` is never a false import: {:?}",
            t.occurrences
        );
        let ret = t.occurrences.iter().rfind(|o| o.name == "a").unwrap();
        assert_eq!(
            ret.resolution,
            Resolution::Undecidable,
            "case `a` is poisoned"
        );
    }

    #[test]
    fn g7_ts_and_go_poison_mechanism_holds() {
        // The poison backstop is grammar-agnostic: prove it demotes an otherwise-
        // importable name to Undecidable and NEVER a false definite. TS/JS + Go
        // declaration/param patterns are exhaustively tabled (no natural unknown
        // trigger), so this locks the shared resolver behavior the backstop feeds.
        // A poisoned name wins over an import at the poisoned scope.
        for (name, body) in [
            (
                "m.rs",
                &b"use m::q;\nfn f(v: u8) {\n    let _ = match v { c!(q) => q, _ => 0 };\n}\n"[..],
            ),
            (
                "m.py",
                &b"import q\ndef f(x):\n    match x:\n        case {'k': q}:\n            return q\n"[..],
            ),
        ] {
            let t = scopes_of(name, body);
            // The USE-site `q` (last occurrence, inside the poisoned scope) must
            // degrade to Undecidable — never the import (line-1 import-site `q` is
            // legitimately ImportedName and is excluded).
            let use_q = t.occurrences.iter().rfind(|o| o.name == "q").unwrap();
            assert_eq!(
                use_q.resolution,
                Resolution::Undecidable,
                "{name}: poisoned `q` use is Undecidable, never the import: {:?}",
                t.occurrences
            );
        }
    }

    // ------------------------------ XB2 round-3 bypass-proof close (H0–H5) ----
    // Round-2 left several statement arms routing binding positions through the
    // NAIVE harvest (`collect_idents`) or binding nothing. H0 deletes the naive
    // collector so every binding position MUST go through a grammar-exhaustive
    // collector (with the poison fallback); H1–H5 close the specific arms. Each
    // fixture fails against the pre-H0 code.

    #[test]
    fn h1_rust_let_tuple_struct_binds_inner_not_ctor() {
        // `let E(v) = g()` with `use m::E`: the constructor `E` is the tuple_
        // struct_pattern `type` PATH (never a binding); only `v` binds. A later
        // `let u = E` must still resolve the import `E`.
        // PRE-FIX: naive harvest bound BOTH `E` and `v` → `E` = false LocalBinding.
        let t = scopes_of(
            "m.rs",
            b"use m::E;\nfn f() {\n    let E(v) = g();\n    let _ = v;\n    let u = E;\n    let _ = u;\n}\n",
        );
        assert!(
            !local_occurrences(&t, "v").is_empty(),
            "inner `v` binds: {:?}",
            t.occurrences
        );
        let e_use = t.occurrences.iter().rfind(|o| o.name == "E").unwrap();
        assert_eq!(
            e_use.resolution,
            Resolution::ImportedName,
            "ctor `E` is the import, never a binding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h1_rust_let_struct_shorthand_binds_field() {
        // `let P { y, .. } = p` with `use m::y`: the shorthand field `y` binds
        // (SEQUENTIAL); a later `let u = y` resolves the LocalBinding.
        // PRE-FIX: naive harvest bound `P` (the type path) as a false LocalBinding.
        let t = scopes_of(
            "m.rs",
            b"use m::y;\nstruct P { y: u8 }\nfn f(p: P) {\n    let P { y, .. } = p;\n    let u = y;\n    let _ = u;\n}\n",
        );
        let y_use = t.occurrences.iter().rfind(|o| o.name == "y").unwrap();
        assert!(
            matches!(y_use.resolution, Resolution::LocalBinding { .. }),
            "shorthand `y` binds: {:?}",
            t.occurrences
        );
        // The let PATTERN path `P` is never bound in the let's block/fn scope
        // (the module-level `struct P` decl legitimately binds `P` at scope 0).
        assert!(
            !t.scopes
                .iter()
                .filter(|s| s.kind != ScopeKind::Module)
                .any(|s| s.bindings.iter().any(|b| b.name == "P")),
            "struct path `P` is never a let binding: {:?}",
            t.scopes
        );
    }

    #[test]
    fn h2_python_attribute_and_subscript_targets_bind_nothing() {
        // `obj.attr = 1` and `d[key] = 1` with `import attr`, `import key`: an
        // attribute/subscript TARGET introduces NO binding, so a later use of
        // `attr`/`key` resolves the import, never a false LocalBinding.
        // PRE-FIX: naive harvest bound `attr`/`key` (and `obj`/`d`) as targets.
        let t = scopes_of(
            "m.py",
            b"import attr\nimport key\nobj.attr = 1\nd[key] = 1\n",
        );
        assert!(
            local_occurrences(&t, "attr").is_empty(),
            "attribute field `attr` never binds: {:?}",
            t.occurrences
        );
        assert!(
            local_occurrences(&t, "key").is_empty(),
            "subscript index `key` never binds: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h2_python_tuple_and_starred_targets_bind() {
        // `a, *b = xs`: both `a` and the starred `b` bind (list_splat_pattern).
        let t = scopes_of("m.py", b"a, *b = xs\nprint(a, b)\n");
        assert!(
            !local_occurrences(&t, "a").is_empty() && !local_occurrences(&t, "b").is_empty(),
            "tuple + starred targets bind: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h3_go_type_switch_alias_binds() {
        // `switch x := v.(type)` with a shadowable `x`: the guard alias binds in
        // the switch body, so the case-body `x` resolves LocalBinding not import.
        // PRE-FIX: type_switch_statement was unhandled → `x` = ImportedName.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"x\"\nfunc f(v any) {\n\tswitch x := v.(type) {\n\tcase int:\n\t\t_ = x\n\t}\n}\n",
        );
        assert!(
            !local_occurrences(&t, "x").is_empty(),
            "type-switch alias `x` binds: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h3_go_range_define_binds_but_reassign_does_not() {
        // `for i := range xs` binds `i` (LocalBinding in the body); the naive
        // harvest was replaced by the exhaustive target collector.
        // PRE-FIX: range_clause was unhandled → body `i` = import/Undecidable.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"i\"\nfunc f(xs []int) {\n\tfor i := range xs {\n\t\t_ = i\n\t}\n}\n",
        );
        assert!(
            !local_occurrences(&t, "i").is_empty(),
            "range `:=` binds `i`: {:?}",
            t.occurrences
        );
        // `=` range form binds nothing (reassignment): the outer `i` is the import.
        let t2 = scopes_of(
            "m.go",
            b"package main\nimport \"i\"\nvar i int\nfunc f(xs []int) {\n\tfor i = range xs {\n\t\t_ = i\n\t}\n}\n",
        );
        assert!(
            !t2.scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Block && s.bindings.iter().any(|b| b.name == "i")),
            "range `=` binds nothing new in the loop body: {:?}",
            t2.scopes
        );
    }

    #[test]
    fn h4_ts_catch_param_shadows_import_in_block() {
        // `catch (e) { … }` with `import e`: the catch param binds in the handler
        // block, so the body `e` resolves LocalBinding, never the import.
        // PRE-FIX: catch_clause was unhandled → handler-body `e` = ImportedName.
        let t = scopes_of(
            "m.ts",
            b"import e from 'm';\ntry {} catch (e) {\n  console.log(e);\n}\n",
        );
        let body_e = t.occurrences.iter().rfind(|o| o.name == "e").unwrap();
        assert!(
            matches!(body_e.resolution, Resolution::LocalBinding { .. }),
            "catch param `e` shadows the import in the block: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h4_ts_catch_destructure_binds() {
        // `catch ({ code })`: the destructured shorthand `code` binds in the block.
        let t = scopes_of(
            "m.ts",
            b"try {} catch ({ code }) {\n  console.log(code);\n}\n",
        );
        assert!(
            !local_occurrences(&t, "code").is_empty(),
            "destructured catch `code` binds: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h5_python_except_as_binds_type_is_use() {
        // `except X as e` with `import e`, `import X`: the alias `e` binds; the
        // exception TYPE `X` is a USE, never a binding.
        // PRE-FIX: except_clause was unhandled → body `e` = ImportedName.
        let t = scopes_of(
            "m.py",
            b"import e\nimport X\ntry:\n    pass\nexcept X as e:\n    print(e)\n",
        );
        assert!(
            !local_occurrences(&t, "e").is_empty(),
            "except alias `e` binds: {:?}",
            t.occurrences
        );
        // `X` never becomes a binding (it is the matched exception type).
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "X")),
            "exception type `X` is never bound: {:?}",
            t.scopes
        );
    }

    #[test]
    fn h5_python_with_as_binds() {
        // `with open(p) as f` with `import f`: the alias `f` binds; the context
        // expression `open(p)` is a use.
        // PRE-FIX: with_statement was unhandled → body `f` = ImportedName.
        let t = scopes_of("m.py", b"import f\nwith open(p) as f:\n    print(f)\n");
        assert!(
            !local_occurrences(&t, "f").is_empty(),
            "with alias `f` binds: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn h0_bare_except_type_does_not_bind() {
        // `except ValueError:` (no `as`) binds NOTHING — the type is a use. Locks
        // the H5 restriction that only an `as_pattern` alias binds.
        let t = scopes_of(
            "m.py",
            b"import ValueError\ntry:\n    pass\nexcept ValueError:\n    pass\n",
        );
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "ValueError")),
            "bare except type never binds: {:?}",
            t.scopes
        );
    }

    #[test]
    fn j1_go_select_receive_define_binds_case_var() {
        // `select { case v := <-ch: … }` with `import v`: the `:=` receive binds
        // `v` in the case body, so the body `v` resolves LocalBinding, not import.
        // PRE-FIX: receive_statement was unhandled → body `v` = ImportedName.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"v\"\nfunc f(ch chan int) {\n\tselect {\n\tcase v := <-ch:\n\t\t_ = v\n\t}\n}\n",
        );
        assert!(
            !local_occurrences(&t, "v").is_empty(),
            "receive `:=` alias `v` binds: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn j1_go_select_receive_reassign_binds_nothing() {
        // `case v = <-ch` (operator `=`) REASSIGNS — binds nothing new; the outer
        // `v` stays the import. Locks the `:=`-token discriminator (J1 sibling of
        // the range `:=`/`=` split).
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"v\"\nvar v int\nfunc f(ch chan int) {\n\tselect {\n\tcase v = <-ch:\n\t\t_ = v\n\t}\n}\n",
        );
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Block && s.bindings.iter().any(|b| b.name == "v")),
            "receive `=` binds nothing new in the case body: {:?}",
            t.scopes
        );
    }

    #[test]
    fn j2_ts_named_function_expression_self_name_binds_inner_only() {
        // `const f = function foo() { foo(); }` with `import foo`: the self-name
        // `foo` is visible INSIDE the body (recursive call → LocalBinding) but
        // NOT in the enclosing scope. PRE-FIX: function_expression name unbound →
        // body `foo` = ImportedName.
        let t = scopes_of(
            "m.ts",
            b"import { foo } from 'm';\nconst f = function foo() {\n  return foo();\n};\n",
        );
        let body = t.occurrences.iter().rfind(|o| o.name == "foo").unwrap();
        assert!(
            matches!(body.resolution, Resolution::LocalBinding { .. }),
            "self-name `foo` binds inside the body: {:?}",
            t.occurrences
        );
        // The enclosing-scope module has no `foo` binding (self-name is inner-only).
        assert!(
            !t.scopes[0].bindings.iter().any(|b| b.name == "foo"),
            "self-name does not leak to the enclosing scope: {:?}",
            t.scopes[0]
        );
    }

    #[test]
    fn j2_ts_named_class_expression_self_name_binds_in_body() {
        // `const C = class Bar { m() { return Bar; } }` with `import Bar`: the
        // class-expression self-name `Bar` is visible inside the class body.
        // PRE-FIX: class-expression name unbound → body `Bar` = ImportedName.
        let t = scopes_of(
            "m.ts",
            b"import { Bar } from 'm';\nconst C = class Bar {\n  m() { return Bar; }\n};\n",
        );
        assert!(
            !local_occurrences(&t, "Bar").is_empty(),
            "class-expression self-name `Bar` binds in the body: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn j3_python_pep695_type_params_bind_in_definition() {
        // `def f[T](x: T) -> T` with `import T`: the type parameter `T` binds in
        // the definition scope, so annotation uses resolve LocalBinding, not the
        // import. PRE-FIX: type_parameters unhandled → `T` = ImportedName.
        let t = scopes_of("m.py", b"import T\ndef f[T](x: T) -> T:\n    return x\n");
        assert!(
            !local_occurrences(&t, "T").is_empty(),
            "PEP 695 type param `T` binds: {:?}",
            t.occurrences
        );
        // The bound in `def g[U: int]` binds ONLY `U`, never the bound `int`.
        let t2 = scopes_of("m.py", b"def g[U: int](x):\n    return x\n");
        assert!(
            t2.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "U")),
            "constrained type param `U` binds: {:?}",
            t2.scopes
        );
        assert!(
            !t2.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "int")),
            "the constraint bound `int` never binds: {:?}",
            t2.scopes
        );
    }

    #[test]
    fn j3_python_pep695_type_alias_binds_name() {
        // `type Alias = ...` with `import Alias`: the alias name binds at module
        // scope. PRE-FIX: type_alias_statement unhandled → `Alias` = ImportedName.
        let t = scopes_of("m.py", b"import Alias\ntype Alias = int\nx: Alias = 1\n");
        assert!(
            !local_occurrences(&t, "Alias").is_empty(),
            "type alias name `Alias` binds: {:?}",
            t.occurrences
        );
    }

    // --- §8d audit-table NON-BINDING spot-checks (one per grammar): a row the
    // audit marks NON-BINDING is proven to introduce no local name. ---

    #[test]
    fn audit_rust_const_item_value_expr_is_not_a_binding() {
        // Rust: a `const` value EXPRESSION introduces no binding — `const K = v`
        // binds `K` (decl name) but the RHS `v` is a use, never bound.
        let t = scopes_of("m.rs", b"use a::v;\nconst K: i32 = v;\n");
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "v")),
            "const RHS `v` is a use, never a binding: {:?}",
            t.scopes
        );
    }

    #[test]
    fn k3_rust_const_generic_param_binds_in_body() {
        // K3 adversarial (FUNCTION BODY — where the old NON-BINDING claim was
        // FALSE for CONST generics): `use a::N; fn f<const N: usize>() -> usize {
        // N }`. `N` is a VALUE name; its body use is an `identifier` occurrence
        // that MUST resolve to the const param (LocalBinding), never the import.
        // PRE-FIX: generic params unbound → body `N` = false ImportedName.
        let t = scopes_of(
            "m.rs",
            b"use a::N;\nfn f<const N: usize>() -> usize {\n    N\n}\n",
        );
        let body_n = t.occurrences.iter().rfind(|o| o.name == "N").unwrap();
        assert!(
            matches!(body_n.resolution, Resolution::LocalBinding { .. }),
            "const generic `N` binds in the fn scope; body `N` = LocalBinding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn l2_rust_type_generic_param_value_path_binds_in_body() {
        // L2 adversarial (the round-5 blind spot): a TYPE param used in VALUE
        // position — `T::default()` — has an `identifier` path-head occurrence `T`
        // that MUST resolve to the generic param (LocalBinding), never a
        // same-spelling import. `use a::T; fn f<T: Default>() -> T { T::default() }`.
        // Refutes the old "type params have NO collision surface" claim: the
        // scoped path head IS a tracked identifier occurrence.
        let t = scopes_of(
            "m.rs",
            b"use a::T;\nfn f<T: Default>() -> T {\n    T::default()\n}\n",
        );
        let body_t = t
            .occurrences
            .iter()
            .rfind(|o| o.name == "T")
            .expect("a `T` occurrence exists");
        assert!(
            matches!(body_t.resolution, Resolution::LocalBinding { .. }),
            "type generic `T` binds in the fn scope; `T::default()` head = LocalBinding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn l2_rust_impl_level_const_generic_binds_in_method_body() {
        // L2 adversarial (impl-level, round-5 blind spot): an `impl<const N: usize>`
        // const param used in a METHOD BODY value position must resolve to the impl
        // generic, never a same-spelling import. The impl item opens its own scope
        // (`scope_kind_of`) so `N` binds for the whole body incl. nested fn scopes.
        let t = scopes_of(
            "m.rs",
            b"use a::N;\nimpl Foo {\n    fn m(&self) -> usize {\n        N\n    }\n}\n",
        );
        // Sanity: without a const generic, body `N` is the import.
        let import_n = t
            .occurrences
            .iter()
            .rfind(|o| o.name == "N")
            .expect("a `N` occurrence exists");
        assert!(
            matches!(import_n.resolution, Resolution::ImportedName),
            "no generic ⇒ body `N` = ImportedName: {:?}",
            t.occurrences
        );
        let t2 = scopes_of(
            "m.rs",
            b"use a::N;\nimpl<const N: usize> Foo<N> {\n    fn m(&self) -> usize {\n        N\n    }\n}\n",
        );
        let body_n = t2
            .occurrences
            .iter()
            .rfind(|o| o.name == "N")
            .expect("a `N` occurrence exists");
        assert!(
            matches!(body_n.resolution, Resolution::LocalBinding { .. }),
            "impl const generic `N` binds; method body `N` = LocalBinding: {:?}",
            t2.occurrences
        );
    }

    #[test]
    fn audit_ts_send_labeled_statement_label_is_not_a_binding() {
        // TS/JS: a `labeled_statement` label introduces no value binding — `l:`
        // names a jump target, not a variable; an imported `l` is untouched.
        let t = scopes_of(
            "m.ts",
            b"import { l } from 'm';\nl: for (;;) { break l; }\n",
        );
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "l")),
            "loop label `l` is never a value binding: {:?}",
            t.scopes
        );
    }

    #[test]
    fn l3_python_module_scope_augmented_assignment_binds_local() {
        // L3 (GENERAL RULE — module scope, unified with class + function): `import
        // x; x += 1; y = x`. The augmented target REBINDS the name `x` to a fresh
        // value (STORE_NAME) — after it, `x` is no longer the imported module, so
        // the later read resolves to the local binding, NEVER a false ImportedName.
        // No scope special-case: aug-assign binds wherever plain `x = …` binds.
        let t = scopes_of("m.py", b"import x\nx += 1\ny = x\n");
        assert!(
            t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "x")),
            "module-scope augmented assignment binds the name locally: {:?}",
            t.scopes
        );
        let read_x = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert!(
            matches!(read_x.resolution, Resolution::LocalBinding { .. }),
            "module-scope `x += 1` makes the later `x` a LocalBinding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn k1_python_augmented_assignment_at_function_scope_binds_local() {
        // K1 adversarial (FUNCTION scope — where the old NON-BINDING claim was
        // FALSE): `import x; def f(): x += 1; return x`. CPython marks `x`
        // function-local for the WHOLE function (UnboundLocalError proof), so the
        // body `x` is a LocalBinding, NEVER the module import.
        // PRE-FIX: augmented target unhandled → body `x` = false ImportedName.
        let t = scopes_of("m.py", b"import x\ndef f():\n    x += 1\n    return x\n");
        assert!(
            t.scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Function && s.bindings.iter().any(|b| b.name == "x")),
            "augmented target `x` binds LocalBinding in the function scope: {:?}",
            t.scopes
        );
        let body_x = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert!(
            matches!(body_x.resolution, Resolution::LocalBinding { .. }),
            "function-scope `x += 1` makes body `x` a LocalBinding, not the import: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn k1_python_augmented_attribute_target_at_function_scope_binds_nothing() {
        // K1 guard: only a BARE-NAME augmented target binds — `self.x += 1` binds
        // nothing (attribute target skipped by the H2 collector), so no false
        // LocalBinding for `x` or `self` leaks from an augmented attribute.
        let t = scopes_of("m.py", b"def f(self):\n    self.count += 1\n");
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.bindings.iter().any(|b| b.name == "count")),
            "augmented attribute target binds nothing: {:?}",
            t.scopes
        );
    }

    #[test]
    fn audit_go_send_statement_is_not_a_binding() {
        // Go: a `send_statement` (`ch <- v`) in a select case introduces no
        // binding — only a `:=` receive does (J1). Neither `ch` nor `v` is bound.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"v\"\nfunc f(ch chan int, v int) {\n\tselect {\n\tcase ch <- v:\n\t}\n}\n",
        );
        assert!(
            !t.scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Block && s.bindings.iter().any(|b| b.name == "v")),
            "send statement binds nothing in the case: {:?}",
            t.scopes
        );
    }

    #[test]
    fn k2_go_range_var_invisible_after_loop() {
        // K2 adversarial (POST-LOOP scope — where the old over-visibility was a
        // false LocalBinding): `import fmt; for fmt := range xs {}; fmt.Println`.
        // The loop var `fmt` scopes to the `for_statement`, so the post-loop
        // `fmt` (operand of the selector) resolves to the import, NEVER the loop
        // var. PRE-FIX: range `:=` bound into the fn scope → post-loop `fmt` =
        // false LocalBinding.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f(xs []int) {\n\tfor fmt := range xs {\n\t\t_ = fmt\n\t}\n\tfmt.Println(1)\n}\n",
        );
        let post = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(post.resolution, Resolution::ImportedName),
            "post-loop `fmt` resolves to the import, not the loop var: {:?}",
            t.occurrences
        );
        // The in-loop use still resolves to the loop binding (precise, not lost).
        assert!(
            !local_occurrences(&t, "fmt").is_empty(),
            "in-loop `fmt` still resolves LocalBinding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn k2_go_select_case_alias_invisible_to_sibling_case() {
        // K2 adversarial (SIBLING-CASE scope — where the old over-visibility was a
        // false LocalBinding): case 1 aliases `v := <-a`; case 2's `v` MUST
        // resolve to the import (package), NEVER case 1's binding.
        // PRE-FIX: receive `:=` bound into the enclosing block → sibling-case `v`
        // = false LocalBinding.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"v\"\nfunc f(a chan int, b chan int) {\n\tselect {\n\tcase v := <-a:\n\t\t_ = v\n\tcase <-b:\n\t\t_ = v.X\n\t}\n}\n",
        );
        let sibling = t.occurrences.iter().rfind(|o| o.name == "v").unwrap();
        assert!(
            matches!(sibling.resolution, Resolution::ImportedName),
            "sibling-case `v` resolves to the import, not case-1's alias: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn l1_go_if_init_var_invisible_after_if() {
        // L1 adversarial (POST-IF scope — round-5 blind spot): `if fmt := 1; fmt >
        // 0 {}` then `fmt.Println`. The if-init `:=` var scopes to the
        // `if_statement` (its own implicit block per the Go spec), so the post-if
        // `fmt` selector operand resolves to the import, NEVER the init var.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f() {\n\tif fmt := 1; fmt > 0 {\n\t\t_ = fmt\n\t}\n\tfmt.Println(1)\n}\n",
        );
        let post = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(post.resolution, Resolution::ImportedName),
            "post-if `fmt` resolves to the import, not the if-init var: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn l1_go_switch_init_var_invisible_after_switch() {
        // L1 adversarial (POST-SWITCH scope): `switch fmt := 1; fmt {}` then
        // `fmt.Println`. The switch-init `:=` var scopes to the
        // `expression_switch_statement`; the post-switch `fmt` resolves to the
        // import, never the init var.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f() {\n\tswitch fmt := 1; fmt {\n\tcase 1:\n\t\t_ = fmt\n\t}\n\tfmt.Println(1)\n}\n",
        );
        let post = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(post.resolution, Resolution::ImportedName),
            "post-switch `fmt` resolves to the import, not the switch-init var: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn l1_go_type_switch_alias_invisible_after_switch() {
        // L1 adversarial (POST-TYPE-SWITCH scope): `switch fmt := x.(type) {}` then
        // `fmt.Println`. The type-switch alias scopes to the
        // `type_switch_statement`; the post-switch `fmt` resolves to the import,
        // never the alias.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f(x interface{}) {\n\tswitch fmt := x.(type) {\n\tcase int:\n\t\t_ = fmt\n\t}\n\tfmt.Println(1)\n}\n",
        );
        let post = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(post.resolution, Resolution::ImportedName),
            "post-type-switch `fmt` resolves to the import, not the alias: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn l3_python_class_scope_augmented_assignment_binds_attr() {
        // L3 adversarial (CLASS scope — round-5 blind spot): `import x; class C: x
        // += 1; y = x`. CPython compiles the class-body `x += 1` to a STORE_NAME
        // that CREATES a class attribute, so the later class-body read resolves to
        // it (LocalBinding), NEVER the module import — identical to how plain `x =
        // x + 1` binds. PRE-FIX: class-scope augmented target gated out → post-read
        // `x` = false ImportedName.
        let t = scopes_of("m.py", b"import x\nclass C:\n    x += 1\n    y = x\n");
        let read_x = t.occurrences.iter().rfind(|o| o.name == "x").unwrap();
        assert!(
            matches!(read_x.resolution, Resolution::LocalBinding { .. }),
            "class-scope `x += 1` binds a class attr; later `x` = LocalBinding: {:?}",
            t.occurrences
        );
    }

    // ------------------------------------ XB2 round-6 falsifier probes (M) -----
    // Companion rules the round-6 falsifier surfaced: (M1) each Go switch/select
    // CLAUSE is its own implicit block — a case/default body `:=` is invisible to
    // sibling clauses and after the switch/select; (M2) Rust assoc `fn`/`const`
    // NAMES do not bind into the item's L2 Block scope (path-only reach).

    #[test]
    fn m1_go_switch_case_body_var_invisible_to_sibling_case() {
        // Case-1 body `fmt := 2`; case-2 uses `fmt.Println`. Per the companion
        // rule `expression_case` opens its own block, so the case-1 body var is
        // invisible to case 2 — the sibling `fmt` resolves to the import.
        // PRE-FIX: the body `:=` bound into the switch block → sibling-case
        // `fmt` = false LocalBinding.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f(n int) {\n\tswitch n {\n\tcase 1:\n\t\tfmt := 2\n\t\t_ = fmt\n\tcase 2:\n\t\tfmt.Println(1)\n\t}\n}\n",
        );
        let sibling = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(sibling.resolution, Resolution::ImportedName),
            "sibling-case `fmt` resolves to the import, not case-1's body var: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn m1_go_type_switch_case_body_var_invisible_to_sibling_case() {
        // `type_case` companion rule: case-`int` body `fmt := 1`; the case-`string`
        // `fmt.Println` resolves to the import, never the sibling case-body var.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f(x interface{}) {\n\tswitch x.(type) {\n\tcase int:\n\t\tfmt := 1\n\t\t_ = fmt\n\tcase string:\n\t\tfmt.Println(1)\n\t}\n}\n",
        );
        let sibling = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(sibling.resolution, Resolution::ImportedName),
            "type-switch sibling-case `fmt` resolves to the import: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn m1_go_select_default_case_body_var_invisible_after_select() {
        // `default_case` companion rule: a `:=` in the select's default body
        // scopes to that clause, so the post-select `fmt.Println` resolves to the
        // import, never the default-case body var.
        let t = scopes_of(
            "m.go",
            b"package main\nimport \"fmt\"\nfunc f(a chan int) {\n\tselect {\n\tcase <-a:\n\t\t_ = 1\n\tdefault:\n\t\tfmt := 2\n\t\t_ = fmt\n\t}\n\tfmt.Println(1)\n}\n",
        );
        let post = t.occurrences.iter().rfind(|o| o.name == "fmt").unwrap();
        assert!(
            matches!(post.resolution, Resolution::ImportedName),
            "post-select `fmt` resolves to the import, not the default-case var: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn m2_rust_impl_assoc_fn_name_does_not_bind_into_item_block() {
        // `use a::helper; impl S { fn helper(){} fn go(){ helper(); } }`. The
        // assoc `fn helper` is reachable only via `S::helper`/`Self::helper`; a
        // BARE `helper()` in a sibling method body resolves to the import.
        // PRE-FIX: the assoc-fn name bound into the impl's L2 block → sibling
        // `helper()` = false LocalBinding.
        let t = scopes_of(
            "m.rs",
            b"use a::helper;\nstruct S;\nimpl S {\n    fn helper() {}\n    fn go() {\n        helper();\n    }\n}\n",
        );
        let call = t.occurrences.iter().rfind(|o| o.name == "helper").unwrap();
        assert!(
            matches!(call.resolution, Resolution::ImportedName),
            "bare `helper()` in sibling method = ImportedName, not the assoc fn: {:?}",
            t.occurrences
        );
        assert!(
            local_occurrences(&t, "helper").is_empty(),
            "no `helper` occurrence resolves LocalBinding: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn m2_rust_trait_assoc_const_name_does_not_bind_into_item_block() {
        // Trait assoc-const default-method body: `const helper` is path-only, so
        // a bare `helper` in the default method resolves to the import.
        let t = scopes_of(
            "m.rs",
            b"use a::helper;\ntrait T {\n    const helper: u8 = 0;\n    fn go(&self) {\n        let _ = helper;\n    }\n}\n",
        );
        let read = t.occurrences.iter().rfind(|o| o.name == "helper").unwrap();
        assert!(
            matches!(read.resolution, Resolution::ImportedName),
            "bare `helper` in default method = ImportedName, not the assoc const: {:?}",
            t.occurrences
        );
    }

    // ------------------------------ BF1 occurrence-stream purity (XH3) --------
    // Each fixture is a resolved falsifier probe: a path-position / import-span /
    // intrinsic-tag / index-signature identifier that PRE-FIX buffered as an
    // occurrence and false-resolved to a same-spelling import (a wrong-target
    // Extracted edge downstream). Post-fix the polluting identifier is DROPPED.

    #[test]
    fn bf1_rust_scoped_path_segment_not_occurrence() {
        // `crate::internal::helper()` with a colliding `use a::helper`. PRE-FIX
        // the `name`-segment `helper` buffered as an occurrence and resolved to
        // the import (false ImportedName). Post-fix: NO `helper` occurrence.
        let t = scopes_of(
            "m.rs",
            b"use a::helper;\nfn f() {\n    crate::internal::helper();\n}\n",
        );
        assert!(
            !t.occurrences.iter().any(|o| o.name == "helper"),
            "scoped-path `helper` segment is not an occurrence: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences.iter().any(|o| o.name == "internal"),
            "scoped-path `internal` segment is not an occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_rust_bare_call_still_resolves_import() {
        // REGRESSION: a WHOLE bare callee (`helper()`, not a path) stays an
        // occurrence and resolves the import — the fix must not over-drop.
        let t = scopes_of("m.rs", b"use a::helper;\nfn f() {\n    helper();\n}\n");
        assert!(
            t.occurrences
                .iter()
                .any(|o| o.name == "helper" && o.resolution == Resolution::ImportedName),
            "bare `helper()` call resolves ImportedName: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_rust_body_use_declaration_not_occurrence() {
        // A fn-body-scoped `use crate::x::h;` (unused): its path identifiers are
        // import binding-sites, never body uses. PRE-FIX `h` buffered as an
        // occurrence and self-resolved. Post-fix: NO `h` occurrence.
        let t = scopes_of("m.rs", b"fn f() {\n    use crate::x::h;\n}\n");
        assert!(
            !t.occurrences.iter().any(|o| o.name == "h"),
            "body `use` path `h` is not a body-use occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_python_dotted_import_segments_dropped_attr_head_stays() {
        // `import a.b.c` — every `dotted_name` segment is an import path element,
        // never a body use. The body `x = a.b.c` attribute-chain HEAD `a` IS a
        // real bare use and STAYS; trailing `.b`/`.c` fields stay dropped (F4).
        let t = scopes_of("m.py", b"import a.b.c\nx = a.b.c\n");
        assert!(
            !t.occurrences.iter().any(|o| o.name == "b"),
            "dotted segment `b` is not an occurrence: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences.iter().any(|o| o.name == "c"),
            "dotted segment `c` is not an occurrence: {:?}",
            t.occurrences
        );
        assert!(
            t.occurrences.iter().any(|o| o.name == "a" && o.line == 2),
            "attribute-chain head `a` (body) stays an occurrence: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences.iter().any(|o| o.name == "a" && o.line == 1),
            "import-path `a` segment (line 1) is dropped: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_tsx_intrinsic_tag_dropped_component_stays() {
        // Lowercase `<div>`/`</div>` are HTML intrinsics (dropped); an uppercase
        // component tag `<Foo/>` IS a value reference and stays an occurrence.
        let t = scopes_of("m.tsx", b"const x = <div><Foo /></div>;\n");
        assert!(
            !t.occurrences.iter().any(|o| o.name == "div"),
            "intrinsic tag `div` is not an occurrence: {:?}",
            t.occurrences
        );
        assert!(
            t.occurrences.iter().any(|o| o.name == "Foo"),
            "component tag `Foo` stays an occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_ts_reexport_from_specifiers_dropped_local_export_stays() {
        // `export {helper} from './b'` re-exports another module's binding: the
        // specifier `helper` is NOT a local value use. PRE-FIX it buffered as an
        // occurrence and false-resolved to the colliding `import {helper}` (a
        // wrong-target edge — './a' vs the real './b'). Post-fix: line-2 `helper`
        // is dropped; the import-site `helper` (line 1) is also dropped (import
        // span); a namespace re-export `export * as ns from …` `ns` drops too.
        let t = scopes_of(
            "m.ts",
            b"import {helper} from './a';\nexport {helper} from './b';\nexport * as ns from './c';\n",
        );
        assert!(
            !t.occurrences.iter().any(|o| o.name == "helper"),
            "re-export-from `helper` specifier is not an occurrence: {:?}",
            t.occurrences
        );
        assert!(
            !t.occurrences.iter().any(|o| o.name == "ns"),
            "namespace re-export `ns` is not an occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_ts_sourceless_local_reexport_stays() {
        // REGRESSION: a SOURCELESS `export {local}` re-exports a LOCAL binding —
        // its `local` IS a real reference and must NOT be dropped (only
        // `export … from 'src'` specifiers drop). Resolves to the `const local`.
        let t = scopes_of("m.ts", b"const local = 1;\nexport {local};\n");
        assert!(
            t.occurrences
                .iter()
                .any(|o| o.name == "local" && o.line == 2),
            "sourceless `export {{local}}` specifier stays an occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_rust_extern_crate_name_not_occurrence() {
        // `extern crate helper;` names a crate-binding site, never a body use.
        // PRE-FIX the name `helper` buffered as an occurrence and false-resolved
        // to the colliding `use a::helper`. Post-fix: NO `helper` occurrence.
        let t = scopes_of("m.rs", b"use a::helper;\nextern crate helper;\n");
        assert!(
            !t.occurrences.iter().any(|o| o.name == "helper"),
            "extern-crate `helper` name is not an occurrence: {:?}",
            t.occurrences
        );
    }

    #[test]
    fn bf1_ts_index_signature_param_not_occurrence() {
        // `[key: string]: T` — `key` is a type-position placeholder that binds no
        // value; PRE-FIX it buffered as an occurrence and could false-resolve to
        // a same-spelling import. Post-fix: NO `key` occurrence.
        let t = scopes_of("m.ts", b"interface I {\n  [key: string]: number;\n}\n");
        assert!(
            !t.occurrences.iter().any(|o| o.name == "key"),
            "index-signature param `key` is not an occurrence: {:?}",
            t.occurrences
        );
    }
}
