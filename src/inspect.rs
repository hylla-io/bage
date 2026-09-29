//! Read-only inspection facade: open + parse any file, list its addressable
//! blocks (the Outline), read sub-ranges, and surface parse-health defects.
//! Everything here is strictly read-only — nothing writes to disk.

use serde::{Deserialize, Serialize};
use tree_sitter::Node as TsNode;

use crate::hashing::{self, Hasher};
use crate::parser::{Adapter, Lang, Node, ParserPort, Tree};
use crate::region::{self, LINE_SENTINEL, LineIndex, Region};

// The tier-2 analysis substrate is a sibling read-only surface over an
// [`OpenedFile`]; re-export it here so hosts reach declaration outline and
// analysis sites through the one inspection facade. Its identity is disposable
// (re-derived per parse) and it never mutates the outline contract.
pub use crate::tier2::{Site, Tier2Family, tier2_sites};

// Import/export FACT extraction (XB1) is a second sibling read-only surface
// over an [`OpenedFile`]: structured cross-file linkage facts (who imports what
// from where, who re-exports) Hylla joins into edges. Re-exported through the
// inspection facade so hosts reach outline, tier-2 sites, and facts uniformly;
// like tier-2, facts are DISPOSABLE (re-derived per parse) and never touch the
// outline contract.
pub use crate::facts::{ExportFact, ExportKind, Facts, ImportFact, ImportedItem, extract_facts};

// Scope + binding extraction (XB2) is a third sibling read-only surface over an
// [`OpenedFile`]: the lexical scope tree, per-scope local bindings, and every
// value-identifier occurrence tagged with a typed resolution
// (LocalBinding/ImportedName/Undecidable). Re-exported through the inspection
// facade alongside outline, tier-2 sites, and facts; like them it is DISPOSABLE
// (re-derived per parse) and never touches the outline contract.
pub use crate::facts::{
    Binding, Occurrence, Resolution, Scope, ScopeKind, ScopeTree, Scopes, extract_scopes,
};

/// Errors from the read-only inspection surface.
#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    #[error("bage: open file {path:?}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("bage: parse {path:?} ({lang}): {source}")]
    Parse {
        path: String,
        lang: Lang,
        source: crate::parser::ParseError,
    },
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Resolve(#[from] crate::region::ResolveError),
}

/// A freshly parsed file handle: the path, the selected language, and the
/// concrete syntax tree. It is the read-only convenience an agent IDE uses to
/// inspect a file without opening a full editor. Dropping it frees the native
/// tree (no explicit `Close` needed).
#[derive(Debug)]
pub struct OpenedFile {
    /// The file path that was opened (as supplied by the caller).
    pub path: String,
    /// The language selected for the path via [`Lang::for_path`].
    pub lang: Lang,
    /// The parsed CST together with the source bytes it was parsed from.
    pub tree: Tree,
}

/// Reads `path`, selects a language with [`Lang::for_path`] (falling back to
/// the grammar-free text mode for any type without a registered grammar, so
/// ANY file opens), and parses it with the same tree-sitter adapter Båge
/// edits with.
pub fn open_file(path: &str) -> Result<OpenedFile, InspectError> {
    let src = std::fs::read(path).map_err(|e| InspectError::Io {
        path: path.to_string(),
        source: e,
    })?;
    let lang = Lang::for_path(path);
    let tree = Adapter::new()
        .parse(lang, &src)
        .map_err(|e| InspectError::Parse {
            path: path.to_string(),
            lang,
            source: e,
        })?;
    Ok(OpenedFile {
        path: path.to_string(),
        lang,
        tree,
    })
}

/// One entry in a file's [`outline`]: a named declaration node (or, for the
/// grammar-free text fallback, a single line). Bytes are the half-open CST
/// range; lines are 1-based to match `EditResult` line numbering. `name` is
/// read from the grammar's naming fields where it has them, so a C-family
/// declaration is named after its declarator, never its return type; it is
/// empty for an anonymous declaration or when no name is found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Symbol {
    /// The grammar node kind (e.g. "function_declaration"), or "line" for
    /// the text fallback.
    pub kind: String,
    /// The declared identifier, best-effort; empty when none was found.
    pub name: String,
    /// Inclusive start byte offset of the node.
    pub start_byte: usize,
    /// Exclusive end byte offset of the node.
    pub end_byte: usize,
    /// 1-based start line of the node.
    pub start_line: usize,
    /// 1-based end line of the node.
    pub end_line: usize,
}

/// Returns a documentSymbol-like listing of a parsed tree: every named
/// declaration node, in source order, with its byte and line ranges. Code
/// grammars select declaration nodes by named-node kind, so any tree-sitter
/// grammar works; the data-format grammars (JSON, YAML, TOML, XML, CSS,
/// HTML) instead list their named-block kinds — pairs, tables, elements,
/// rule sets — via [`data_decl_kinds`]. For the grammar-free text fallback
/// it returns one symbol per source line.
///
/// The text fallback is identified by the absence of a native engine tree
/// ([`Tree::has_native`]), NOT by root kind: some real grammars (e.g. HTML)
/// also use a "document" root, so the engine-free handle is the unambiguous
/// discriminator.
pub fn outline(tree: &Tree, lang: Lang) -> Vec<Symbol> {
    let Some(native) = tree.native_root() else {
        return outline_lines(&tree.source);
    };
    let mut out = Vec::new();
    let misread = native.is_error();
    walk_decls(
        &tree.root,
        Some(native),
        misread,
        &tree.source,
        lang,
        0,
        &mut out,
    );
    out
}

/// Recursively appends a symbol for every named outline-worthy node under
/// `n`, in source order. It always recurses (even into a matched node) so
/// methods nested in a class/impl/struct body — and nested data blocks like
/// JSON pairs inside objects — are captured. `depth` is 0 for direct
/// children of the root; TOML uses it to limit bare pairs to the top level.
/// The root itself is never emitted.
///
/// `tn` is the engine node `n` was materialized from, walked in lockstep so
/// naming can read grammar fields. A child whose engine twin does not line up
/// (a count or kind mismatch) is walked with `None`.
///
/// `misread` says the grammar did not read the code around `n` cleanly: the
/// root itself is an ERROR, the top-level item holding `n` contains an ERROR
/// or MISSING node, or its twin did not line up. Such a node is named exactly as v0.11.0 named it (see
/// [`symbol_name`]).
fn walk_decls(
    n: &Node,
    tn: Option<TsNode>,
    misread: bool,
    src: &[u8],
    lang: Lang,
    depth: usize,
    out: &mut Vec<Symbol>,
) {
    let twins: Vec<TsNode> = match tn {
        Some(t) => t.children(&mut t.walk()).collect(),
        None => Vec::new(),
    };
    let aligned = twins.len() == n.children.len();
    for (i, c) in n.children.iter().enumerate() {
        let tc = twins
            .get(i)
            .copied()
            .filter(|t| aligned && t.kind() == c.kind);
        // A top-level item's `has_error` covers every node under it, so it
        // is read once, where the walk enters the item.
        let misread = misread || tc.is_none_or(|t| depth == 0 && t.has_error());
        if c.named && is_outline_kind(lang, &c.kind, depth) {
            out.push(Symbol {
                kind: c.kind.clone(),
                name: symbol_name(lang, c, tc.filter(|_| !misread), src),
                start_byte: c.start_byte,
                end_byte: c.end_byte,
                start_line: c.start_point.row + 1,
                end_line: c.end_point.row + 1,
            });
        }
        walk_decls(c, tc, misread, src, lang, depth + 1, out);
    }
}

/// The substrings whose presence in a node kind marks it a declaration
/// across the supported grammars.
const DECL_KIND_SUBSTRINGS: [&str; 12] = [
    "declaration",
    "definition",
    "function",
    "method",
    "class",
    "struct",
    "interface",
    "impl",
    "enum",
    "trait",
    "module",
    "namespace",
];

/// Whether a node kind names a declaration. It matches Rust's `*_item` kinds
/// (function_item, struct_item, …) and the cross-grammar substring set —
/// painfully simple and grammar-table-free. It first excludes obvious
/// sub-parts that are not outline-worthy: parameters, list containers (e.g.
/// field_declaration_list), and bare type expressions (Go struct_type /
/// interface_type), so the outline lists declarations, not their innards.
fn is_decl_kind(kind: &str) -> bool {
    if kind.contains("parameter") || kind.ends_with("_list") || kind.ends_with("_type") {
        return false;
    }
    if kind.ends_with("_item") {
        return true;
    }
    DECL_KIND_SUBSTRINGS.iter().any(|sub| kind.contains(sub))
}

/// Whether a direct-child node kind carries a declaration's name across the
/// supported grammars.
fn is_name_kind(kind: &str) -> bool {
    matches!(
        kind,
        "name" | "field_identifier" | "type_identifier" | "property_identifier"
    ) || kind.contains("identifier")
}

/// The fallback name for a node whose grammar gives no naming field (see
/// [`field_name`]), and the whole name of a misread node: v0.11.0 named
/// every code node this way, and a misread keeps that name byte for byte,
/// so this function must not change. It first looks at `n`'s direct named
/// children, then — since some grammars wrap the name one level down (Go
/// type_declaration → type_spec → type_identifier) — at the direct named
/// children of `n`'s named children. It stays shallow (≤2 levels) so it never
/// grabs an identifier from a function body. Empty when none is found.
fn decl_name(n: &Node, src: &[u8]) -> String {
    let name = direct_name(n, src);
    if !name.is_empty() {
        return name;
    }
    for c in &n.children {
        if !c.named {
            continue;
        }
        let name = direct_name(c, src);
        if !name.is_empty() {
            return name;
        }
    }
    String::new()
}

/// The text of `n`'s first direct named identifier-kind child, or empty if
/// none. Slice bounds are guarded.
fn direct_name(n: &Node, src: &[u8]) -> String {
    for c in &n.children {
        if !c.named || !is_name_kind(&c.kind) {
            continue;
        }
        if c.end_byte < c.start_byte || c.end_byte > src.len() {
            continue;
        }
        return String::from_utf8_lossy(&src[c.start_byte..c.end_byte]).into_owned();
    }
    String::new()
}

/// The named-block kinds that form the outline for a data-format grammar,
/// or `None` for code grammars (which use [`is_decl_kind`]). TOML's
/// top-level bare pairs are handled separately in [`is_outline_kind`]
/// because they are outline-worthy only at document depth.
fn data_decl_kinds(lang: Lang) -> Option<&'static [&'static str]> {
    Some(match lang {
        Lang::Json => &["pair"],
        Lang::Yaml => &["block_mapping_pair"],
        Lang::Toml => &["table"],
        Lang::Xml | Lang::Html => &["element"],
        Lang::Css => &["rule_set"],
        _ => return None,
    })
}

/// Whether a named node of `kind` at `depth` (0 = direct child of the root)
/// belongs in `lang`'s outline. Data-format grammars use their fixed
/// per-language kind set; every other grammar keeps the exact
/// substring-based [`is_decl_kind`] behavior.
fn is_outline_kind(lang: Lang, kind: &str, depth: usize) -> bool {
    match data_decl_kinds(lang) {
        Some(kinds) => {
            kinds.contains(&kind) || (lang == Lang::Toml && kind == "pair" && depth == 0)
        }
        None => is_decl_kind(kind),
    }
}

/// The display name for an outline node: language-specific key/tag/selector
/// extraction for the data-format grammars; for code, the grammar's own
/// field names via [`field_name`], and the identifier-child search of
/// [`decl_name`] only for a node whose grammar gives no naming field.
///
/// A code node with no engine twin is one the grammar misread (see
/// [`walk_decls`]) and is named by [`decl_name`] alone, exactly as v0.11.0
/// named every code node. The grammar cannot expand macros and reads every
/// `.h` as C, so in misread code its fields hold whatever word landed in
/// them — a keyword, a macro, a parameter, a callee — and no field-based
/// rule tells those from a name without inventing a guess per shape. Keeping
/// the old name there changes nothing a host already relied on.
fn symbol_name(lang: Lang, n: &Node, tn: Option<TsNode>, src: &[u8]) -> String {
    match lang {
        Lang::Json => json_key_name(n, src),
        Lang::Yaml | Lang::Toml => first_named_child_text(n, src),
        Lang::Xml => tag_name(n, src, "Name"),
        Lang::Html => tag_name(n, src, "tag_name"),
        Lang::Css => child_kind_text(n, src, "selectors").trim().to_string(),
        _ => match tn {
            None => decl_name(n, src),
            Some(_) if is_body_kind(&n.kind) => String::new(),
            Some(t) => {
                let name = field_name(lang, t, src).unwrap_or_else(|| decl_name(n, src));
                if is_c_family(lang) {
                    one_line(&name)
                } else {
                    name
                }
            }
        },
    }
}

/// C and C++, where declarations are named by following declarators.
fn is_c_family(lang: Lang) -> bool {
    matches!(lang, Lang::C | Lang::Cpp)
}

/// A C-family name on one line: whitespace runs collapse to one space, so a
/// name written across lines (`operator const\n    char *`) reads as one
/// line. A name that still holds a comment (the field-free fallback reads
/// raw text, `class B : public ns /* why */ ::Base`) is no name: a comment
/// is never part of a declared identifier.
fn one_line(name: &str) -> String {
    if name.contains("//") || name.contains("/*") {
        return String::new();
    }
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a code node kind is the member body of a class, interface or
/// enum (`class_body`, `interface_body`, `enum_body`, Java's
/// `enum_body_declarations`). A body declares nothing, so it is unnamed:
/// the name belongs to the declaration that owns the body, and borrowing the
/// first member's name would tie the body, and every member nested under it,
/// to whichever member happens to come first.
fn is_body_kind(kind: &str) -> bool {
    kind.ends_with("_body") || kind.ends_with("_body_declarations")
}

/// The TypeScript/JavaScript outline kinds whose name is a property KEY,
/// which may be written as a string, a number or a computed `[…]` key.
/// JavaScript's `field_definition` holds its key in the `property` field.
const JS_MEMBER_KINDS: [&str; 6] = [
    "method_definition",
    "public_field_definition",
    "field_definition",
    "method_signature",
    "abstract_method_signature",
    "enum_assignment",
];

/// A TypeScript/JavaScript member's name from its property key. A literal
/// key loses its brackets and quotes only when what is left is a plain
/// identifier or a number (`['KEY']` → `KEY`, `[42]` → `42`, `"field"` →
/// `field`), because that is the name a caller writes and a language server
/// resolves. Every other key stays exactly as written (`['a-b']`, `['a/b']`,
/// `[Symbol.iterator]`, `[KEY]`): stripping it would put a separator into a
/// path built from names, or make two distinct keys look alike.
fn member_key_name(key: TsNode, src: &[u8]) -> String {
    let as_written = ts_text(key, src);
    let literal = if key.kind() == "computed_property_name" {
        match key.named_child_count() {
            1 => key.named_child(0).expect("one named child was counted"),
            _ => return as_written,
        }
    } else {
        key
    };
    let text = ts_text(literal, src);
    // An escape or a `${…}` substitution leaves a `\` or a brace in the
    // unquoted text, so the identifier and number checks refuse it.
    let bare = match literal.kind() {
        "number" => return text,
        "string" | "template_string" => unquote(&text),
        _ => None,
    };
    match bare {
        Some(b) if is_plain_identifier(b) || is_decimal(b) => b.to_string(),
        _ => as_written,
    }
}

/// `text` without its first and last character when both are the same
/// ASCII quote.
fn unquote(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let quoted = b.len() >= 2 && b[0] == b[b.len() - 1] && matches!(b[0], b'\'' | b'"' | b'`');
    quoted.then(|| &text[1..text.len() - 1])
}

/// An ASCII identifier: a letter, `_` or `$`, then letters, digits, `_` or
/// `$`.
fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// A plain decimal number: digits, optionally a `.` and more digits.
fn is_decimal(s: &str) -> bool {
    let (int, frac) = s.split_once('.').unwrap_or((s, "0"));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    digits(int) && digits(frac)
}

/// A declaration's name read from the grammar's FIELDS, or `None` when the
/// node carries no naming field and the caller must fall back.
///
/// Fields are the only reliable signal in the C family: a C, C++, C# or Java
/// declaration starts with its return or field TYPE, so the first identifier
/// child is the type (`Point make_point()` is not named `Point`). The `name`
/// field wins; otherwise a `declarator` field is followed down to the
/// declared identifier. `Some("")` means the grammar was consulted and the
/// declarator chain ends without a name, which must not fall back to a type
/// name.
fn field_name(lang: Lang, n: TsNode, src: &[u8]) -> Option<String> {
    match (lang, n.kind()) {
        (Lang::Python, "decorated_definition") => {
            return field_name(lang, n.child_by_field_name("definition")?, src);
        }
        (Lang::Cpp, "template_declaration" | "friend_declaration") => {
            let inner = n.named_children(&mut n.walk()).find(|c| {
                !matches!(
                    c.kind(),
                    "template_parameter_list" | "requires_clause" | "comment"
                )
            })?;
            // A befriended type (`friend class Vec<int>;`) is itself the name;
            // a befriended or templated declaration is named like any other.
            let is_type = matches!(
                inner.kind(),
                "type_identifier" | "template_type" | "qualified_identifier"
            );
            if is_type {
                return declarator_name(lang, inner, false, src);
            }
            return field_name(lang, inner, src)
                .or_else(|| declarator_name(lang, inner, false, src));
        }
        // A construct signature has no name; TypeScript's own navigation
        // tree names it `new()` (`getItemName` in `services/navigationBar`),
        // never its return type.
        (Lang::TypeScript | Lang::Tsx, "construct_signature") => {
            return Some("new()".to_string());
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, kind)
            if JS_MEMBER_KINDS.contains(&kind) =>
        {
            let key = n
                .child_by_field_name("name")
                .or_else(|| n.child_by_field_name("property"))?;
            return Some(member_key_name(key, src));
        }
        (Lang::CSharp, "field_declaration" | "event_field_declaration") => {
            let decl = n
                .named_children(&mut n.walk())
                .find(|c| c.kind() == "variable_declaration")?;
            return field_name(lang, decl, src);
        }
        (Lang::CSharp, "variable_declaration") => {
            let first = n
                .named_children(&mut n.walk())
                .find(|c| c.kind() == "variable_declarator")?;
            return Some(ts_text(first.child_by_field_name("name")?, src));
        }
        (Lang::CSharp, "destructor_declaration") => {
            return Some(format!("~{}", ts_text(n.child_by_field_name("name")?, src)));
        }
        (Lang::CSharp, "operator_declaration") => {
            let start = keyword_start(n, &["operator"])?;
            let end = n.child_by_field_name("operator")?.end_byte();
            return Some(slice_text(src, start, end));
        }
        (Lang::CSharp, "conversion_operator_declaration") => {
            let start = keyword_start(n, &["implicit", "explicit", "operator"])?;
            let end = n.child_by_field_name("type")?.end_byte();
            return Some(slice_text(src, start, end));
        }
        (Lang::CSharp, "indexer_declaration") => return Some("this".to_string()),
        _ => {}
    }
    if is_c_family(lang) {
        return c_decl_name(lang, n, src);
    }
    if let Some(name) = n.child_by_field_name("name") {
        return Some(ts_text(name, src));
    }
    let declarator = declarator_child(n)?;
    Some(declarator_name(lang, declarator, false, src).unwrap_or_default())
}

/// A C or C++ declaration's name: the `name` field kept whole, else the
/// declarator chain down to the declared identifier. A typedef may define a
/// builtin type spelling (`typedef _Bool bool;`); no other declaration may.
/// `None` when the node has neither a `name` nor a `declarator` field.
///
/// The grammar can read code it cannot expand WITHOUT an ERROR, so a clean
/// parse still meets two shapes where the declarator slot is not the name:
/// a keyword that names no type in the type slot (`export C_LIB_NAMESPACE
/// {…}`, `return x;`), which declares nothing; and a definition whose
/// declarator is bare parentheses (`do_library_init(void) {…}` after a
/// macro return type on the line before), where the parentheses are the
/// parameter list and the type word is the function.
fn c_decl_name(lang: Lang, n: TsNode, src: &[u8]) -> Option<String> {
    if let Some(name) = n.child_by_field_name("name") {
        return Some(uncommented_text(
            name,
            name.start_byte(),
            name.end_byte(),
            src,
        ));
    }
    let decl = declarator_child(n)?;
    let ty = n.child_by_field_name("type");
    let keyword_type = |t: TsNode| {
        let word = ts_text(t, src);
        is_word(t) && is_reserved(lang, &word) && !TYPE_KEYWORDS.contains(&word.as_str())
    };
    if ty.is_some_and(keyword_type) {
        return Some(String::new());
    }
    if n.kind() == "function_definition" && decl.kind() == "parenthesized_declarator" {
        let word = ty.filter(|t| is_word(*t)).map(|t| ts_text(t, src));
        return Some(word.filter(|w| !is_reserved(lang, w)).unwrap_or_default());
    }
    let builtin_ok = n.kind() == "type_definition";
    Some(declarator_name(lang, decl, builtin_ok, src).unwrap_or_default())
}

/// Whether `n` is a bare word: an identifier the grammar may have filed as a
/// type.
fn is_word(n: TsNode) -> bool {
    matches!(n.kind(), "type_identifier" | "identifier")
}

/// The C keywords that spell a type the grammar may file as a type word
/// (`typedef _Bool bool;`), so one in the type slot is a type, not a
/// statement.
const TYPE_KEYWORDS: &[&str] = &[
    "_BitInt",
    "_Bool",
    "_Complex",
    "_Decimal128",
    "_Decimal32",
    "_Decimal64",
    "_Imaginary",
    "bool",
    "char",
    "double",
    "float",
    "int",
    "long",
    "short",
    "signed",
    "unsigned",
    "void",
];

/// The builtin type names the C and C++ grammars lex as `primitive_type` in
/// a type position (tree-sitter-c's `primitive_type` rule). A declarator word
/// spelled as one is no name: code the grammar read without an ERROR but
/// could not expand leaves one in the slot (`do_library_init(void)` holds
/// `void`). A typedef defining one holds a `primitive_type`, never a word.
const BUILTIN_TYPES: &[&str] = &[
    "bool",
    "char",
    "int",
    "float",
    "double",
    "void",
    "size_t",
    "ssize_t",
    "ptrdiff_t",
    "intptr_t",
    "uintptr_t",
    "charptr_t",
    "nullptr_t",
    "max_align_t",
    "int8_t",
    "int16_t",
    "int32_t",
    "int64_t",
    "uint8_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "char8_t",
    "char16_t",
    "char32_t",
];

/// C keywords (through C23). A declarator that reads as one is no name.
const C_RESERVED: &[&str] = &[
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_BitInt",
    "_Bool",
    "_Complex",
    "_Decimal128",
    "_Decimal32",
    "_Decimal64",
    "_Generic",
    "_Imaginary",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "alignas",
    "alignof",
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "const",
    "constexpr",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "nullptr",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "static_assert",
    "struct",
    "switch",
    "thread_local",
    "true",
    "typedef",
    "typeof",
    "typeof_unqual",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
];

/// The C++ keywords C lacks. They stay usable names in C: aws-lc declares
/// variables called `template`, `public` and `this` in `.c` files.
const CPP_RESERVED: &[&str] = &[
    "and",
    "and_eq",
    "asm",
    "bitand",
    "bitor",
    "catch",
    "class",
    "co_await",
    "co_return",
    "co_yield",
    "compl",
    "concept",
    "const_cast",
    "consteval",
    "constinit",
    "decltype",
    "delete",
    "dynamic_cast",
    "explicit",
    "export",
    "friend",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "not",
    "not_eq",
    "operator",
    "or",
    "or_eq",
    "private",
    "protected",
    "public",
    "reinterpret_cast",
    "requires",
    "static_cast",
    "template",
    "this",
    "throw",
    "try",
    "typeid",
    "typename",
    "using",
    "virtual",
    "xor",
    "xor_eq",
];

/// Whether `word` is a keyword of `lang`, so never a declared name. The
/// character types C++ reserves (`wchar_t`, `char8_t`, …) are left out: they
/// are typedefs in C, and the grammar files them as type words in both. C
/// also refuses `operator`: C++ operator overloads in a `.h` read as C
/// (`float operator()(uint8_t x)`) can parse without an ERROR and leave it
/// in the declarator slot, at the cost of a C variable literally named
/// `operator`.
fn is_reserved(lang: Lang, word: &str) -> bool {
    match lang {
        Lang::C => C_RESERVED.contains(&word) || word == "operator",
        Lang::Cpp => C_RESERVED.contains(&word) || CPP_RESERVED.contains(&word),
        _ => false,
    }
}

/// The first child in `n`'s `declarator` field that is a declarator. The C
/// grammar also files an MSVC calling convention (`void __cdecl f(void);`)
/// under that field, ahead of the real declarator, so it is passed over.
fn declarator_child(n: TsNode) -> Option<TsNode> {
    n.children_by_field_name("declarator", &mut n.walk())
        .find(|c| c.kind() != "ms_call_modifier")
}

/// The identifier a C/C++/Java declarator declares, following the grammar's
/// `declarator` field through pointer, reference, array, function,
/// parenthesized and init declarators. Qualified, template and operator
/// names are kept whole (`Shape::area`, `f<int>`, `operator==`). `None` when
/// the chain ends without a name, on a keyword, or on a builtin type: a word
/// spelled as one always, a `primitive_type` unless `builtin_ok` (a typedef
/// may define one: `typedef _Bool bool;` files `bool` as a `primitive_type`).
fn declarator_name(lang: Lang, mut d: TsNode, builtin_ok: bool, src: &[u8]) -> Option<String> {
    loop {
        match d.kind() {
            "identifier" | "field_identifier" | "type_identifier" => {
                let text = ts_text(d, src);
                let builtin = is_c_family(lang) && BUILTIN_TYPES.contains(&text.as_str());
                return (!builtin && !is_reserved(lang, &text)).then_some(text);
            }
            "primitive_type" => return builtin_ok.then(|| ts_text(d, src)),
            "operator_name"
            | "destructor_name"
            | "template_function"
            | "template_method"
            | "template_type"
            | "structured_binding_declarator" => {
                return Some(uncommented_text(d, d.start_byte(), d.end_byte(), src));
            }
            "qualified_identifier" => return Some(qualified_name(d, src)),
            "operator_cast" => return Some(conversion_name(d, d, src)),
            // A parenthesized declarator may open with a calling convention;
            // an attributed one carries its attributes after the declarator.
            "parenthesized_declarator" | "reference_declarator" | "attributed_declarator" => {
                d = d
                    .named_children(&mut d.walk())
                    .find(|c| c.kind() != "ms_call_modifier")?;
            }
            _ => match declarator_child(d) {
                Some(next) => d = next,
                None => d = d.child_by_field_name("name")?,
            },
        }
    }
}

/// A qualified name kept whole (`Shape::area`, `inner::deep`), except that a
/// conversion operator stops before its parameter list, as an unqualified
/// one does: `Widget::operator bool() const` is `Widget::operator bool`.
fn qualified_name(q: TsNode, src: &[u8]) -> String {
    let mut inner = q;
    while let Some(name) = inner.child_by_field_name("name") {
        if name.kind() == "operator_cast" {
            return conversion_name(q, name, src);
        }
        if name.kind() != "qualified_identifier" {
            break;
        }
        inner = name;
    }
    uncommented_text(q, q.start_byte(), q.end_byte(), src)
}

/// A C++ conversion operator's name: from the start of `from` through the
/// target type of the `operator_cast` `cast`, keeping any pointer or
/// reference on it and stopping at the parameter list, so `operator const
/// char *() const` is `operator const char *` and `operator int &()` stays
/// distinct from `operator int()`.
fn conversion_name(from: TsNode, cast: TsNode, src: &[u8]) -> String {
    let mut end = cast.end_byte();
    let mut abs = cast.child_by_field_name("declarator");
    while let Some(a) = abs {
        if a.kind() == "abstract_function_declarator" {
            end = a.start_byte();
            break;
        }
        abs = a
            .named_children(&mut a.walk())
            .find(|c| c.kind().starts_with("abstract_"));
    }
    uncommented_text(from, from.start_byte(), end, src)
}

/// `src[start..end]` inside node `n` with each of `n`'s comments replaced by
/// a space, so a comment written inside a name (`Box</*n*/3>`) is not part
/// of it. [`one_line`] then collapses the whitespace.
fn uncommented_text(n: TsNode, start: usize, end: usize, src: &[u8]) -> String {
    let mut comments = Vec::new();
    collect_comments(n, &mut comments);
    let mut text = String::new();
    let mut at = start;
    for (s, e) in comments {
        if e <= at || s >= end {
            continue;
        }
        text.push_str(&slice_text(src, at, s.max(at)));
        text.push(' ');
        at = e.min(end);
    }
    text.push_str(&slice_text(src, at, end));
    text
}

/// The byte ranges of every comment under `n`, in source order.
fn collect_comments(n: TsNode, out: &mut Vec<(usize, usize)>) {
    if n.kind() == "comment" {
        out.push((n.start_byte(), n.end_byte()));
        return;
    }
    for c in n.children(&mut n.walk()) {
        collect_comments(c, out);
    }
}

/// The start byte of `n`'s first anonymous keyword child among `keywords`.
fn keyword_start(n: TsNode, keywords: &[&str]) -> Option<usize> {
    n.children(&mut n.walk())
        .find(|c| !c.is_named() && keywords.contains(&c.kind()))
        .map(|c| c.start_byte())
}

/// The source text of an engine node, bounds-guarded.
fn ts_text(n: TsNode, src: &[u8]) -> String {
    slice_text(src, n.start_byte(), n.end_byte())
}

/// `src[start..end]` as text; empty when the range does not fit `src`.
fn slice_text(src: &[u8], start: usize, end: usize) -> String {
    if start > end || end > src.len() {
        return String::new();
    }
    String::from_utf8_lossy(&src[start..end]).into_owned()
}

/// The raw source text of `n`, bounds-guarded; empty when the node's byte
/// range does not fit `src`.
fn node_text(n: &Node, src: &[u8]) -> String {
    if n.end_byte < n.start_byte || n.end_byte > src.len() {
        return String::new();
    }
    String::from_utf8_lossy(&src[n.start_byte..n.end_byte]).into_owned()
}

/// The text of `n`'s first named child — a YAML pair's key flow_node or a
/// TOML table's bare/dotted key — or empty when none exists.
fn first_named_child_text(n: &Node, src: &[u8]) -> String {
    n.children
        .iter()
        .find(|c| c.named)
        .map(|c| node_text(c, src))
        .unwrap_or_default()
}

/// The text of `n`'s first direct child of exactly `kind`, or empty.
fn child_kind_text(n: &Node, src: &[u8], kind: &str) -> String {
    n.children
        .iter()
        .find(|c| c.kind == kind)
        .map(|c| node_text(c, src))
        .unwrap_or_default()
}

/// A JSON pair's key with the surrounding quotes stripped: the key is the
/// pair's first named child (a "string") and its "string_content" child is
/// the unquoted text. Falls back to trimming quotes off the whole key when
/// the content node is absent (the empty key `""`).
fn json_key_name(n: &Node, src: &[u8]) -> String {
    let Some(key) = n.children.iter().find(|c| c.named) else {
        return String::new();
    };
    let content = child_kind_text(key, src, "string_content");
    if !content.is_empty() {
        return content;
    }
    node_text(key, src).trim_matches('"').to_string()
}

/// An XML/HTML element's tag name: the first direct child (start tag,
/// self-closing tag, or empty-element tag) carrying a `name_kind` child
/// supplies it. Empty when no tag name is found.
fn tag_name(n: &Node, src: &[u8], name_kind: &str) -> String {
    for c in &n.children {
        let name = child_kind_text(c, src, name_kind);
        if !name.is_empty() {
            return name;
        }
    }
    String::new()
}

/// One "line" symbol per source line for the grammar-free text fallback.
/// Each symbol's byte range excludes the trailing newline. A trailing
/// newline does not produce a phantom empty final line; genuine
/// interior/leading empty lines are kept.
fn outline_lines(src: &[u8]) -> Vec<Symbol> {
    let mut out = Vec::new();
    let mut row = 1;
    let mut line_start = 0;
    for (i, &b) in src.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        out.push(Symbol {
            kind: "line".to_string(),
            name: String::new(),
            start_byte: line_start,
            end_byte: i, // exclude the '\n'
            start_line: row,
            end_line: row,
        });
        row += 1;
        line_start = i + 1;
    }
    if line_start < src.len() {
        out.push(Symbol {
            kind: "line".to_string(),
            name: String::new(),
            start_byte: line_start,
            end_byte: src.len(),
            start_line: row,
            end_line: row,
        });
    }
    out
}

/// One outline symbol enriched with its content anchor and, optionally, its
/// raw bytes. It is a FLAT struct (deliberately not nested) so both JSON and
/// TOON render it cleanly: JSON keys stay snake_case, and a slice of blocks
/// is a uniform array that TOON emits in its compact tabular form (the token
/// win). `region_hash` anchors the block by content (byte-identical to what
/// Hylla stores per node); `content` is the source slice for the block's
/// byte range, populated only when requested.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// The grammar node kind (e.g. "function_declaration"), "line" for the
    /// text fallback, or "range" for a line/byte-addressed read.
    pub kind: String,
    /// The declared identifier, best-effort; empty when none was found.
    pub name: String,
    /// 1-based start line of the block.
    pub start_line: usize,
    /// 1-based end line of the block.
    pub end_line: usize,
    /// Inclusive start byte offset of the block.
    pub start_byte: usize,
    /// Exclusive end byte offset of the block.
    pub end_byte: usize,
    /// Anchors the block by content; see [`region::hash_region`].
    pub region_hash: String,
    /// Raw source for the block's byte range; empty unless requested (and
    /// then omitted from JSON when empty, matching Go's `omitempty`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
}

/// Returns one [`Block`] per outline symbol of `opened`, in source order:
/// every named declaration for a grammar-backed tree, or one line block for
/// the grammar-free text fallback. Each block carries the region_hash for
/// its byte range so a host can anchor edits by content. When
/// `include_content` is true each block's content is set to the raw source
/// bytes for its range (bounds-guarded); when false content stays empty so
/// callers can list structure cheaply.
pub fn read_blocks(opened: &OpenedFile, include_content: bool) -> Vec<Block> {
    let src = &opened.tree.source;
    outline(&opened.tree, opened.lang)
        .into_iter()
        .map(|sym| {
            let content =
                if include_content && sym.end_byte <= src.len() && sym.end_byte >= sym.start_byte {
                    String::from_utf8_lossy(&src[sym.start_byte..sym.end_byte]).into_owned()
                } else {
                    String::new()
                };
            Block {
                region_hash: region::hash_region(src, sym.start_byte, sym.end_byte),
                kind: sym.kind,
                name: sym.name,
                start_line: sym.start_line,
                end_line: sym.end_line,
                start_byte: sym.start_byte,
                end_byte: sym.end_byte,
                content,
            }
        })
        .collect()
}

/// Selects what a [`read_file`] returns. The default reads the whole file's
/// structure with no raw content. `include_content` populates each block's
/// content with its source slice; `symbol`, when non-empty, filters the
/// returned blocks to those whose name matches exactly. `line`/`end_line`
/// and `start_byte`/`end_byte` address a sub-range (see [`read_file`] for
/// the addressing rule; 0 = unset for lines, a byte range is active only
/// when `end_byte > start_byte`).
#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    /// Populates each returned block's content with its raw bytes.
    pub include_content: bool,
    /// When non-empty, keeps only blocks whose name equals it.
    pub symbol: String,
    /// 1-based start line of a sub-range read (0 = unset).
    pub line: usize,
    /// 1-based end line of a sub-range read (0 = unset).
    pub end_line: usize,
    /// Inclusive start byte of a sub-range read.
    pub start_byte: usize,
    /// Exclusive end byte of a sub-range read (active only when
    /// `end_byte > start_byte`).
    pub end_byte: usize,
}

/// The structured outcome of a [`read_file`]: the read path, the detected
/// language, the raw and normalized whole-file hashes (the drift gate), and
/// the file's blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    /// The file path that was read (as supplied by the caller).
    pub path: String,
    /// The detected source language's canonical name.
    pub lang: String,
    /// The whole-file raw-bytes digest (byte-offset validity gate).
    pub raw_hash: String,
    /// The whole-file normalized-bytes digest (content anchor).
    pub norm_hash: String,
    /// The file's outline blocks, optionally filtered by [`ReadOptions`].
    pub blocks: Vec<Block>,
}

/// Opens `path` with the shared parser, lists its blocks, and returns a
/// [`ReadResult`] carrying the path, detected language, and the whole-file
/// raw and normalized hashes computed with `hasher`.
///
/// Addressing mode is chosen from `opts`, and the three modes are mutually
/// exclusive: line mode (`opts.line >= 1`, optionally bounded by
/// `end_line > line`), byte mode (`end_byte > start_byte`), and
/// whole-file/symbol mode when neither is active. In line or byte mode it
/// returns exactly one synthetic `kind:"range"` block over the resolved
/// range, anchored by [`region::hash_region`]. Setting `symbol` together
/// with a line or byte range is rejected.
pub fn read_file(
    path: &str,
    opts: &ReadOptions,
    hasher: &dyn Hasher,
) -> Result<ReadResult, InspectError> {
    let opened = open_file(path)?;
    let src = &opened.tree.source;

    let line_mode = opts.line >= 1;
    let byte_mode = opts.end_byte > opts.start_byte;
    if (line_mode || byte_mode) && !opts.symbol.is_empty() {
        return Err(InspectError::Usage(
            "read: symbol filtering is mutually exclusive with line/byte addressing".to_string(),
        ));
    }

    let blocks = if line_mode || byte_mode {
        vec![range_block(src, opts, line_mode)?]
    } else {
        let mut blocks = read_blocks(&opened, opts.include_content);
        if !opts.symbol.is_empty() {
            blocks.retain(|b| b.name == opts.symbol);
        }
        blocks
    };

    Ok(ReadResult {
        path: path.to_string(),
        lang: opened.lang.name().to_string(),
        raw_hash: hashing::raw_hash(hasher, src),
        norm_hash: hashing::norm_hash(hasher, src),
        blocks,
    })
}

/// Resolves the line- or byte-addressed sub-range described by `opts`
/// against `src` via [`resolve_range`] and returns a single synthetic
/// `kind:"range"` block anchored by [`region::hash_region`].
fn range_block(src: &[u8], opts: &ReadOptions, line_mode: bool) -> Result<Block, InspectError> {
    let (line, lines, start, end) = if line_mode {
        if opts.end_line > opts.line {
            (-1, format!("{}-{}", opts.line, opts.end_line), -1, -1)
        } else {
            (opts.line as i64, String::new(), -1, -1)
        }
    } else {
        (
            -1,
            String::new(),
            opts.start_byte as i64,
            opts.end_byte as i64,
        )
    };

    let reg = resolve_range(src, line, &lines, start, end).map_err(InspectError::Usage)?;

    let (rs, re) = (reg.start_byte as usize, reg.end_byte as usize);
    let content = if opts.include_content && re <= src.len() && re >= rs {
        String::from_utf8_lossy(&src[rs..re]).into_owned()
    } else {
        String::new()
    };
    Ok(Block {
        kind: "range".to_string(),
        name: String::new(),
        start_byte: rs,
        end_byte: re,
        start_line: reg.start_line as usize,
        end_line: reg.end_line as usize,
        region_hash: region::hash_region(src, rs, re),
        content,
    })
}

/// Builds a region-anchored target over `src` from one addressing mode.
/// Exactly one mode must be supplied: a single line (`line >= 1`), a 1-based
/// inclusive line range (`lines = "L1-L2"`), or a raw byte range (`start`
/// and `end` both >= 0). The unset sentinel for `line`, `start`, and `end`
/// is -1, matching the CLI flag defaults.
///
/// Line addressing is resolved to a concrete byte range against `src` via a
/// [`LineIndex`]. A resolved line range spans THROUGH the final line's
/// trailing newline; that newline is excluded so a replacement preserves
/// line structure (a final line with no trailing newline is left as-is).
/// Supplying more than one mode, or no mode at all, is an error.
pub fn resolve_range(
    src: &[u8],
    line: i64,
    lines: &str,
    start: i64,
    end: i64,
) -> Result<Region, String> {
    let byte_mode = start >= 0 || end >= 0;
    let line_mode = line >= 0 || !lines.is_empty();

    if byte_mode && line_mode {
        return Err("resolve: choose one of line/lines or start/end, not both".to_string());
    }
    if byte_mode {
        if start < 0 || end < 0 {
            return Err("resolve: start and end are both required for byte addressing".to_string());
        }
        let li = LineIndex::new(src);
        return Ok(li.fill_line_cols(Region {
            start_byte: start,
            end_byte: end,
            ..Default::default()
        }));
    }
    if line_mode {
        let (start_line, end_line) = resolve_line_range(line, lines)?;
        let li = LineIndex::new(src);
        let mut reg = li.resolve_lines(Region {
            start_byte: LINE_SENTINEL,
            start_line,
            end_line,
            ..Default::default()
        });
        // A resolved line range spans THROUGH the final line's trailing
        // newline. Exclude that newline so a replacement replaces the line
        // CONTENT and the line structure survives even when the replacement
        // has no trailing newline. A final line with no trailing newline is
        // left as-is.
        let (rs, re) = (reg.start_byte, reg.end_byte);
        if re > rs && re as usize <= src.len() && src[re as usize - 1] == b'\n' {
            reg.end_byte -= 1;
            reg = li.fill_line_cols(reg);
        }
        return Ok(reg);
    }
    Err("resolve: one of line, lines, or start/end is required".to_string())
}

/// One first-class insertion point over a source buffer, resolved to a
/// zero-width byte position by [`resolve_insertion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionPoint {
    /// Insert at end-of-file (`src.len()`).
    Append,
    /// Insert at the start byte of the 1-based line.
    BeforeLine(i64),
    /// Insert at the start byte of the line AFTER the 1-based line — i.e.
    /// just past the line's trailing newline. A line at or past EOF clamps
    /// to end-of-buffer, matching [`LineIndex::byte_for_line`].
    AfterLine(i64),
}

/// Resolves an [`InsertionPoint`] against `src` to a zero-width region
/// (`start_byte == end_byte`) with an EMPTY region_hash — there is no
/// content to anchor at a point, so the per-file anchor gates drift.
/// Shared by `bage apply --append/--before-line/--after-line` and (later)
/// paste. Line numbers must be >= 1; a line past EOF clamps to
/// end-of-buffer via [`LineIndex::byte_for_line`] rather than erroring.
pub fn resolve_insertion(src: &[u8], point: InsertionPoint) -> Result<Region, String> {
    let li = LineIndex::new(src);
    let pos = match point {
        InsertionPoint::Append => src.len(),
        InsertionPoint::BeforeLine(l) => {
            if l < 1 {
                return Err("resolve: before-line must be >= 1".to_string());
            }
            li.byte_for_line(l)
        }
        InsertionPoint::AfterLine(l) => {
            if l < 1 {
                return Err("resolve: after-line must be >= 1".to_string());
            }
            li.byte_for_line(l + 1)
        }
    };
    Ok(li.fill_line_cols(Region {
        start_byte: pos as i64,
        end_byte: pos as i64,
        ..Default::default()
    }))
}

/// The addressing flags shared by `bage copy` and `bage cut`: symbol
/// addressing, line/byte-range addressing (as in read), or bare
/// `region_hash` addressing (the region is located purely by content).
/// A `region_hash` combined with a range/symbol verifies-and-relocates via
/// [`region::resolve`] instead of trusting the offsets blindly.
#[derive(Debug, Clone, Default)]
pub struct CopyTarget {
    /// 1-based single line (-1 = unset).
    pub line: i64,
    /// 1-based inclusive "L1-L2" range ("" = unset).
    pub lines: String,
    /// Inclusive start byte (-1 = unset).
    pub start: i64,
    /// Exclusive end byte (-1 = unset).
    pub end: i64,
    /// Block name to address ("" = unset).
    pub symbol: String,
    /// Content anchor ("" = unset). Alone, it addresses the region purely
    /// by content; with a range/symbol it verifies the resolved bytes.
    pub region_hash: String,
}

/// Resolves a [`CopyTarget`] against an opened file to the half-open byte
/// range to copy or cut. Exactly one addressing mode is required: symbol,
/// line/lines, start/end, or a bare region_hash. Symbol addressing errors
/// when zero or more than one block carries the name (never guesses). When
/// a region_hash is present the range is verified (and benignly relocated)
/// through [`region::resolve`], so a stale offset can never copy the wrong
/// bytes; a content mismatch is a conflict, not a silent misread.
pub fn resolve_copy_range(
    p: &dyn ParserPort,
    opened: &OpenedFile,
    t: &CopyTarget,
) -> Result<(usize, usize), InspectError> {
    let src = &opened.tree.source;
    let range_mode = t.line >= 0 || !t.lines.is_empty() || t.start >= 0 || t.end >= 0;
    let symbol_mode = !t.symbol.is_empty();
    let hash_mode = !t.region_hash.is_empty();

    if symbol_mode && range_mode {
        return Err(InspectError::Usage(
            "copy: --symbol is mutually exclusive with line/byte addressing".to_string(),
        ));
    }
    if !symbol_mode && !range_mode && !hash_mode {
        return Err(InspectError::Usage(
            "copy: one of --symbol, --line/--lines, --start/--end, or --region-hash is required"
                .to_string(),
        ));
    }

    let range: Option<(usize, usize)> = if symbol_mode {
        let matches: Vec<Block> = read_blocks(opened, false)
            .into_iter()
            .filter(|b| b.name == t.symbol)
            .collect();
        match matches.len() {
            0 => {
                return Err(InspectError::Usage(format!(
                    "copy: no block named {:?} in {:?}",
                    t.symbol, opened.path
                )));
            }
            1 => Some((matches[0].start_byte, matches[0].end_byte)),
            n => {
                return Err(InspectError::Usage(format!(
                    "copy: {n} blocks named {:?} in {:?}; address by --region-hash or line/byte range",
                    t.symbol, opened.path
                )));
            }
        }
    } else if range_mode {
        let r =
            resolve_range(src, t.line, &t.lines, t.start, t.end).map_err(InspectError::Usage)?;
        Some((r.start_byte as usize, r.end_byte as usize))
    } else {
        None // bare region_hash: located purely by content below
    };

    if hash_mode {
        let (sb, eb) = match range {
            Some((s, e)) => (s as i64, e as i64),
            None => (LINE_SENTINEL, LINE_SENTINEL),
        };
        let reg = Region {
            path: opened.path.clone(),
            start_byte: sb,
            end_byte: eb,
            region_hash: t.region_hash.clone(),
            ..Default::default()
        };
        let (s, e, _status) = region::resolve(p, opened.lang, src, &reg)?;
        return Ok((s, e));
    }

    Ok(range.expect("range or hash mode guaranteed by the mode checks above"))
}

/// Resolves the single-line / line-range inputs to a 1-based inclusive
/// `[start_line, end_line]`. `line` and `lines` are mutually exclusive;
/// `lines` must be "L1-L2" with L1 <= L2 and both >= 1.
fn resolve_line_range(line: i64, lines: &str) -> Result<(i64, i64), String> {
    if line >= 0 && !lines.is_empty() {
        return Err("resolve: choose one of line or lines, not both".to_string());
    }
    if line >= 0 {
        if line < 1 {
            return Err("resolve: line must be >= 1".to_string());
        }
        return Ok((line, line));
    }
    let (lo, hi) = lines
        .split_once('-')
        .ok_or_else(|| format!("resolve: lines {lines:?} must be L1-L2"))?;
    let start_line: i64 = lo
        .trim()
        .parse()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| format!("resolve: lines start {lo:?} must be >= 1"))?;
    let end_line: i64 = hi
        .trim()
        .parse()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| format!("resolve: lines end {hi:?} must be >= 1"))?;
    if start_line > end_line {
        return Err(format!("resolve: lines {lines:?} has start past end"));
    }
    Ok((start_line, end_line))
}

/// One syntax problem surfaced by parse-health: an ERROR-kind node (a span
/// the grammar could not incorporate) or a MISSING node (a zero-width node
/// the parser inserted to recover, e.g. an absent closing brace). It is the
/// cheap, LSP-free tier of `bage diagnose` (SPEC §10.5) and uses the SAME
/// ERROR/MISSING signal the edit parse-floor relies on. Line/col are
/// 1-based; the byte range is the half-open span of the offending node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseDefect {
    /// "ERROR" for an error-kind node or "MISSING" for an inserted recovery
    /// node.
    pub kind: String,
    /// 1-based line of `start_byte`.
    pub line: usize,
    /// 1-based column (byte offset within the line, +1) of `start_byte`.
    pub col: usize,
    /// Inclusive start byte offset of the offending node.
    pub start_byte: usize,
    /// Exclusive end byte offset of the offending node.
    pub end_byte: usize,
}

/// Walks a parsed file and reports every ERROR-kind or MISSING node as a
/// [`ParseDefect`] with 1-based line/col and byte range. A clean parse
/// reports none.
///
/// The grammar-free text fallback ALWAYS parses losslessly — every byte
/// lands in a line node — so it can never produce a defect and this returns
/// empty for it without walking. This mirrors the edit parse-floor: the same
/// ERROR/MISSING signal gates an edit and is what diagnose surfaces.
pub fn parse_health(opened: &OpenedFile) -> Vec<ParseDefect> {
    let mut out = Vec::new();
    // The text fallback is byte-for-byte lossless and has no concept of a
    // syntax error, so it is reported clean without a walk.
    if !opened.tree.has_native() {
        return out;
    }
    let li = LineIndex::new(&opened.tree.source);
    opened.tree.root.walk(&mut |n| {
        let kind = if n.kind == "ERROR" {
            "ERROR"
        } else if n.missing {
            "MISSING"
        } else {
            return;
        };
        let (line, col) = li.position_for_byte(n.start_byte);
        out.push(ParseDefect {
            kind: kind.to_string(),
            line,
            col: col + 1,
            start_byte: n.start_byte,
            end_byte: n.end_byte,
        });
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::XxHasher;

    fn write_temp(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> String {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn outline_lists_go_declarations_with_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(
            &dir,
            "m.go",
            b"package main\n\ntype T struct{ X int }\n\nfunc (t T) M() {}\n\nfunc F() {}\n",
        );
        let opened = open_file(&p).unwrap();
        let syms = outline(&opened.tree, opened.lang);
        let names: Vec<(&str, &str)> = syms
            .iter()
            .map(|s| (s.kind.as_str(), s.name.as_str()))
            .collect();
        assert!(names.contains(&("type_declaration", "T")), "{names:?}");
        assert!(names.contains(&("method_declaration", "M")), "{names:?}");
        assert!(names.contains(&("function_declaration", "F")), "{names:?}");
    }

    #[test]
    fn outline_text_fallback_is_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "notes.txt", b"alpha\n\nbeta");
        let opened = open_file(&p).unwrap();
        let syms = outline(&opened.tree, opened.lang);
        assert_eq!(syms.len(), 3);
        assert!(syms.iter().all(|s| s.kind == "line"));
        // Byte ranges exclude the trailing newline.
        assert_eq!((syms[0].start_byte, syms[0].end_byte), (0, 5));
        assert_eq!((syms[1].start_byte, syms[1].end_byte), (6, 6));
        assert_eq!((syms[2].start_byte, syms[2].end_byte), (7, 11));
    }

    #[test]
    fn read_blocks_carries_region_hashes_and_optional_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "m.go", b"package main\n\nfunc F() {}\n");
        let opened = open_file(&p).unwrap();
        let without = read_blocks(&opened, false);
        let with = read_blocks(&opened, true);
        assert_eq!(without.len(), with.len());
        let f = with.iter().find(|b| b.name == "F").unwrap();
        assert_eq!(f.content, "func F() {}");
        assert_eq!(f.region_hash.len(), 16);
        assert!(without.iter().all(|b| b.content.is_empty()));
    }

    #[test]
    fn read_file_modes() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "t.txt", b"one\ntwo\nthree\n");
        let h = XxHasher;

        // Whole file.
        let all = read_file(&p, &ReadOptions::default(), &h).unwrap();
        assert_eq!(all.lang, "text");
        assert_eq!(all.blocks.len(), 3);

        // Line mode returns one synthetic range block, newline excluded.
        let one = read_file(
            &p,
            &ReadOptions {
                line: 2,
                include_content: true,
                ..Default::default()
            },
            &h,
        )
        .unwrap();
        assert_eq!(one.blocks.len(), 1);
        assert_eq!(one.blocks[0].kind, "range");
        assert_eq!(one.blocks[0].content, "two");

        // Byte mode.
        let byte = read_file(
            &p,
            &ReadOptions {
                start_byte: 0,
                end_byte: 3,
                include_content: true,
                ..Default::default()
            },
            &h,
        )
        .unwrap();
        assert_eq!(byte.blocks[0].content, "one");

        // Symbol + range is a usage error.
        let err = read_file(
            &p,
            &ReadOptions {
                line: 1,
                symbol: "x".into(),
                ..Default::default()
            },
            &h,
        )
        .unwrap_err();
        assert!(matches!(err, InspectError::Usage(_)));
    }

    #[test]
    fn resolve_range_line_excludes_trailing_newline() {
        let src = b"one\ntwo\nthree\n";
        let r = resolve_range(src, 2, "", -1, -1).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (4, 7)); // "two", no '\n'
        let r = resolve_range(src, -1, "1-2", -1, -1).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (0, 7));
        // Final line with no trailing newline is left as-is.
        let r = resolve_range(b"a\nb", 2, "", -1, -1).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (2, 3));
        // Errors.
        assert!(resolve_range(src, 1, "", 0, 3).is_err()); // both modes
        assert!(resolve_range(src, -1, "", -1, -1).is_err()); // no mode
        assert!(resolve_range(src, -1, "3-1", -1, -1).is_err()); // inverted
        assert!(resolve_range(src, -1, "x-2", -1, -1).is_err()); // malformed
    }

    #[test]
    fn resolve_insertion_returns_zero_width_positions() {
        let src = b"one\ntwo\nthree\n";
        let r = resolve_insertion(src, InsertionPoint::Append).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (14, 14));
        assert!(r.region_hash.is_empty(), "insertion carries no region_hash");
        let r = resolve_insertion(src, InsertionPoint::BeforeLine(1)).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (0, 0));
        let r = resolve_insertion(src, InsertionPoint::BeforeLine(2)).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (4, 4));
        // After the last line lands at end-of-buffer.
        let r = resolve_insertion(src, InsertionPoint::AfterLine(3)).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (14, 14));
        // A line past EOF clamps to end-of-buffer, like byte_for_line.
        let r = resolve_insertion(src, InsertionPoint::AfterLine(99)).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (14, 14));
        // Empty buffer: every point resolves to 0.
        let r = resolve_insertion(b"", InsertionPoint::Append).unwrap();
        assert_eq!((r.start_byte, r.end_byte), (0, 0));
        // Line numbers must be >= 1.
        assert!(resolve_insertion(src, InsertionPoint::BeforeLine(0)).is_err());
        assert!(resolve_insertion(src, InsertionPoint::AfterLine(0)).is_err());
    }

    #[test]
    fn parse_health_reports_defects_and_text_is_always_clean() {
        let dir = tempfile::tempdir().unwrap();
        let broken = write_temp(&dir, "b.go", b"package main\n\nfunc F( {\n");
        let opened = open_file(&broken).unwrap();
        let defects = parse_health(&opened);
        assert!(!defects.is_empty());
        assert!(
            defects
                .iter()
                .all(|d| d.kind == "ERROR" || d.kind == "MISSING")
        );
        assert!(defects.iter().all(|d| d.line >= 1 && d.col >= 1));

        let txt = write_temp(&dir, "b.txt", b"anything {{{ at all");
        let opened = open_file(&txt).unwrap();
        assert!(parse_health(&opened).is_empty());
    }
    /// The outline of `path` flattened to "kind:name" strings, for compact
    /// per-grammar assertions.
    fn kinds_names(path: &str) -> Vec<String> {
        let opened = open_file(path).unwrap();
        outline(&opened.tree, opened.lang)
            .into_iter()
            .map(|s| format!("{}:{}", s.kind, s.name))
            .collect()
    }

    #[test]
    fn outline_json_pairs_with_key_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(
            &dir,
            "a.json",
            b"{\"name\": \"x\", \"nested\": {\"k\": 1}}\n",
        );
        // Quoted keys are stripped; nested pairs are included.
        assert_eq!(kinds_names(&p), ["pair:name", "pair:nested", "pair:k"]);
        let opened = open_file(&p).unwrap();
        let syms = outline(&opened.tree, opened.lang);
        assert_eq!((syms[0].start_line, syms[0].end_line), (1, 1));
        assert_eq!((syms[0].start_byte, syms[0].end_byte), (1, 12));
    }

    #[test]
    fn outline_yaml_mapping_pairs_with_key_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "a.yaml", b"top: 1\nmap:\n  inner: 2\n");
        assert_eq!(
            kinds_names(&p),
            [
                "block_mapping_pair:top",
                "block_mapping_pair:map",
                "block_mapping_pair:inner",
            ]
        );
        let opened = open_file(&p).unwrap();
        let syms = outline(&opened.tree, opened.lang);
        assert_eq!(syms[1].start_line, 2);
        assert_eq!(syms[2].start_line, 3);
    }

    #[test]
    fn outline_toml_tables_and_top_level_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "a.toml", b"top = 1\n[server.http]\nport = 8080\n");
        // Dotted table keys keep their full path; pairs inside a table are
        // part of the table block, so only top-level pairs are listed.
        assert_eq!(kinds_names(&p), ["pair:top", "table:server.http"]);
    }

    #[test]
    fn outline_xml_elements_with_tag_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "a.xml", b"<root attr=\"v\"><child>t</child></root>\n");
        assert_eq!(kinds_names(&p), ["element:root", "element:child"]);
    }

    #[test]
    fn outline_css_rule_sets_with_selector_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "a.css", b".btn , a:hover { color: red; }\n");
        // Exactly the rule sets: property declarations and selector innards
        // no longer leak through the code-grammar substring matcher.
        assert_eq!(kinds_names(&p), ["rule_set:.btn , a:hover"]);
    }

    #[test]
    fn outline_html_elements_with_tag_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "a.html", b"<div id=\"x\"><span>hi</span></div>\n");
        assert_eq!(kinds_names(&p), ["element:div", "element:span"]);
    }

    #[test]
    fn outline_rust_declarations_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(
            &dir,
            "m.rs",
            b"struct S;\n\nimpl S {\n    fn m(&self) {}\n}\n\nfn f() {}\n",
        );
        let got = kinds_names(&p);
        assert!(got.contains(&"struct_item:S".to_string()), "{got:?}");
        assert!(got.contains(&"impl_item:S".to_string()), "{got:?}");
        assert!(got.contains(&"function_item:m".to_string()), "{got:?}");
        assert!(got.contains(&"function_item:f".to_string()), "{got:?}");
    }

    /// The names of every outline symbol of `kind` in a file named `file`
    /// holding `src`, in source order.
    fn names_of(file: &str, src: &str, kind: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, file, src.as_bytes());
        let opened = open_file(&p).unwrap();
        assert!(
            parse_health(&opened).is_empty(),
            "fixture {file} must parse cleanly: {:?}",
            parse_health(&opened)
        );
        outline(&opened.tree, opened.lang)
            .into_iter()
            .filter(|s| s.kind == kind)
            .map(|s| s.name)
            .collect()
    }

    const C_SRC: &str = "\
typedef struct Point { int x; int y; } Point;
struct Node { struct Node *next; int (*cb)(int); };
typedef int (*cmp_fn)(int, int);
typedef unsigned long ulong;
int count;
int first, second;
static const char *names[4];
Point origin = {0, 0};
int total(void) { return 0; }
Point make_point(int x, int y) { Point p = {x, y}; return p; }
struct Point make_struct(void) { struct Point p; return p; }
char *dup(const char *s) { return 0; }
unsigned long long big(void) { return 0; }
int (*get_cmp(void))(int, int) { return 0; }
Point decl_only(int x);
Point *ptr_decl(void);
";

    #[test]
    fn c_function_names_skip_the_return_type() {
        assert_eq!(
            names_of("a.c", C_SRC, "function_definition"),
            [
                "total",
                "make_point",
                "make_struct",
                "dup",
                "big",
                "get_cmp"
            ]
        );
    }

    #[test]
    fn c_declaration_names_come_from_the_declarator() {
        assert_eq!(
            names_of("a.c", C_SRC, "declaration"),
            [
                "count",
                "first",
                "names",
                "origin",
                "p",
                "p",
                "decl_only",
                "ptr_decl"
            ]
        );
    }

    #[test]
    fn c_typedef_and_field_names_come_from_the_declarator() {
        assert_eq!(
            names_of("a.c", C_SRC, "type_definition"),
            ["Point", "cmp_fn", "ulong"]
        );
        assert_eq!(
            names_of("a.c", C_SRC, "field_declaration"),
            ["x", "y", "next", "cb"]
        );
    }

    const CPP_SRC: &str = "\
namespace geo {
namespace inner::deep {
struct Point { int x; int y; };
}
class Shape : public Base {
public:
    Shape();
    ~Shape();
    Point area() const;
    virtual std::string name() const = 0;
    Shape &operator=(const Shape &o);
    bool operator==(const Shape &o) const;
    operator bool() const;
    int count;
    static const int kMax = 4;
    Point *ptr;
    int &ref();
};
Point make_point(int x, int y) { return Point{x, y}; }
std::string label() { return \"\"; }
const Point &ref_point() { static Point p; return p; }
Point *ptr_point() { return nullptr; }
Point Shape::area() const { return Point{}; }
Shape::Shape() {}
Shape::~Shape() {}
bool Shape::operator==(const Shape &o) const { return true; }
template <typename T> T identity(T t) { return t; }
template <typename T> class Box { T v; };
std::vector<int> vec() { return {}; }
auto trailing() -> int { return 0; }
using Alias = int;
}
namespace {
int hidden() { return 0; }
}
";

    #[test]
    fn cpp_function_names_skip_the_return_type() {
        assert_eq!(
            names_of("a.cpp", CPP_SRC, "function_definition"),
            [
                "make_point",
                "label",
                "ref_point",
                "ptr_point",
                "Shape::area",
                "Shape::Shape",
                "Shape::~Shape",
                "Shape::operator==",
                "identity",
                "vec",
                "trailing",
                "hidden",
            ]
        );
    }

    #[test]
    fn cpp_member_names_come_from_the_declarator() {
        assert_eq!(
            names_of("a.cpp", CPP_SRC, "field_declaration"),
            [
                "x",
                "y",
                "area",
                "name",
                "operator=",
                "operator==",
                "count",
                "kMax",
                "ptr",
                "ref",
                "v",
            ]
        );
        assert_eq!(
            names_of("a.cpp", CPP_SRC, "declaration"),
            ["Shape", "~Shape", "operator bool", "p"]
        );
    }

    #[test]
    fn cpp_namespaces_templates_and_aliases_are_named() {
        assert_eq!(
            names_of("a.cpp", CPP_SRC, "namespace_definition"),
            ["geo", "inner::deep", ""]
        );
        assert_eq!(
            names_of("a.cpp", CPP_SRC, "template_declaration"),
            ["identity", "Box"]
        );
        assert_eq!(
            names_of("a.cpp", CPP_SRC, "class_specifier"),
            ["Shape", "Box"]
        );
        assert_eq!(names_of("a.cpp", CPP_SRC, "alias_declaration"), ["Alias"]);
    }

    const CS_FILE_SCOPED_SRC: &str = "\
namespace Fixture.Geo;

public class Point<T> : Base, IShape
{
    private int count;
    private string label = \"x\", other;
    public List<string> Items;
    public int Prop { get; set; }
    public event EventHandler Changed;
    public Point(int c) { count = c; }
    ~Point() {}
    Point Make() { return null; }
    int Total() { return 0; }
    List<string> Names() { return null; }
    int[] Arr() { return null; }
    System.Text.StringBuilder Qualified() { return null; }
    void IShape.Explicit() {}
    public static Point operator +(Point a, Point b) { return a; }
    public static implicit operator int(Point p) { return 0; }
    public int this[int i] => 0;
    public delegate Point Del(int x);
}
";

    const CS_BLOCK_SRC: &str = "\
namespace Block.Scoped
{
    struct S { }
    interface IShape { Point Area(); }
    enum Color { Red, Green }
    record R(int A);
}
namespace Outer { namespace Inner { class C { } } }
";

    #[test]
    fn csharp_method_names_skip_the_return_type() {
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "method_declaration"),
            ["Make", "Total", "Names", "Arr", "Qualified", "Explicit"]
        );
        assert_eq!(
            names_of("a.cs", CS_BLOCK_SRC, "method_declaration"),
            ["Area"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "delegate_declaration"),
            ["Del"]
        );
    }

    #[test]
    fn csharp_namespaces_keep_their_qualified_name() {
        assert_eq!(
            names_of(
                "a.cs",
                CS_FILE_SCOPED_SRC,
                "file_scoped_namespace_declaration"
            ),
            ["Fixture.Geo"]
        );
        assert_eq!(
            names_of("a.cs", CS_BLOCK_SRC, "namespace_declaration"),
            ["Block.Scoped", "Outer", "Inner"]
        );
    }

    #[test]
    fn csharp_fields_are_named_after_their_first_declarator() {
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "field_declaration"),
            ["count", "label", "Items"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "event_field_declaration"),
            ["Changed"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "variable_declaration"),
            ["count", "label", "Items", "Changed"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "property_declaration"),
            ["Prop"]
        );
    }

    #[test]
    fn csharp_constructors_destructors_and_operators_are_named() {
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "constructor_declaration"),
            ["Point"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "destructor_declaration"),
            ["~Point"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "operator_declaration"),
            ["operator +"]
        );
        assert_eq!(
            names_of(
                "a.cs",
                CS_FILE_SCOPED_SRC,
                "conversion_operator_declaration"
            ),
            ["implicit operator int"]
        );
        assert_eq!(
            names_of("a.cs", CS_FILE_SCOPED_SRC, "indexer_declaration"),
            ["this"]
        );
    }

    const JAVA_SRC: &str = "\
package fixture.geo;

public class Geo<T> extends Base implements Shape {
    private int count;
    private String label = \"x\", other;
    public static final List<String> NAMES = null;
    int[] grid;
    public Geo(int c) { this.count = c; }
    String report() { return \"\"; }
    int total() { return 0; }
    List<String> names() { return null; }
    <U> Map<String, U> generic(U u) { return null; }
    int[] arr() { return null; }
    java.util.List<String> qualified() { return null; }
    void run() {}
    interface Inner { Point make(); int CONST = 1; }
    enum Color { RED, GREEN }
    record Pair(int a, int b) { Pair { } }
    @interface Ann { String value(); }
}
";

    #[test]
    fn java_method_names_skip_the_return_type() {
        assert_eq!(
            names_of("A.java", JAVA_SRC, "method_declaration"),
            [
                "report",
                "total",
                "names",
                "generic",
                "arr",
                "qualified",
                "run",
                "make"
            ]
        );
        assert_eq!(
            names_of("A.java", JAVA_SRC, "annotation_type_element_declaration"),
            ["value"]
        );
    }

    #[test]
    fn java_fields_and_constructors_are_named() {
        assert_eq!(
            names_of("A.java", JAVA_SRC, "field_declaration"),
            ["count", "label", "NAMES", "grid"]
        );
        assert_eq!(
            names_of("A.java", JAVA_SRC, "constant_declaration"),
            ["CONST"]
        );
        assert_eq!(
            names_of("A.java", JAVA_SRC, "constructor_declaration"),
            ["Geo"]
        );
        assert_eq!(
            names_of("A.java", JAVA_SRC, "compact_constructor_declaration"),
            ["Pair"]
        );
    }

    #[test]
    fn python_decorated_definitions_take_the_definition_name() {
        let src = "\
@staticmethod
def deco():
    pass

@dataclass
class Model(Base):
    @property
    def value(self) -> int:
        return 0
";
        assert_eq!(
            names_of("a.py", src, "decorated_definition"),
            ["deco", "Model", "value"]
        );
    }

    const TS_LITERAL_KEYS_SRC: &str = "\
const KEY = 'k';
class Box {
  ['KEY']() {}
  [42]() {}
  [\"field\"] = 2;
  'x'() {}
  ['$ok_1']() {}
  ['42']() {}
  [`tmpl`]() {}
  0x1F() {}
  3.5() {}
}
interface Shape { ['area'](): number; }
abstract class Base { abstract ['draw'](): void; }
enum Mode { 'on' = 1, 'b-c' = 2 }
const o = { ['k2']() {} };
";

    #[test]
    fn ts_member_literal_keys_strip_to_a_plain_identifier_or_number() {
        let f = "a.ts";
        assert_eq!(
            names_of(f, TS_LITERAL_KEYS_SRC, "method_definition"),
            ["KEY", "42", "x", "$ok_1", "42", "tmpl", "0x1F", "3.5", "k2"]
        );
        assert_eq!(
            names_of(f, TS_LITERAL_KEYS_SRC, "public_field_definition"),
            ["field"]
        );
        assert_eq!(
            names_of(f, TS_LITERAL_KEYS_SRC, "method_signature"),
            ["area"]
        );
        assert_eq!(
            names_of(f, TS_LITERAL_KEYS_SRC, "abstract_method_signature"),
            ["draw"]
        );
        assert_eq!(
            names_of(f, TS_LITERAL_KEYS_SRC, "enum_assignment"),
            ["on", "'b-c'"]
        );
    }

    #[test]
    fn js_member_literal_keys_strip_to_a_plain_identifier_or_number() {
        let src = "\
class Box {
  ['KEY']() {}
  [42]() {}
  [\"field\"] = 2;
  ['a-b'] = 3;
  plain = 4;
  #hidden = 5;
}
";
        assert_eq!(names_of("a.js", src, "method_definition"), ["KEY", "42"]);
        assert_eq!(
            names_of("a.js", src, "field_definition"),
            ["field", "['a-b']", "plain", "#hidden"]
        );
    }

    #[test]
    fn ts_member_keys_that_are_not_plain_stay_as_written() {
        let src = "\
const KEY = 'k';
class Keep {
  ['a-b']() {}
  'a-b'() {}
  ['a/b']() {}
  ['.env']() {}
  ['#method_definition-0']() {}
  [Symbol.iterator]() {}
  [KEY + '2']() {}
  [KEY]() {}
  [\"with space\"]() {}
  [-1]() {}
  ''() {}
  ['1e3']() {}
  [`a${KEY}`]() {}
  #priv() {}
  plain() {}
}
";
        assert_eq!(
            names_of("a.ts", src, "method_definition"),
            [
                "['a-b']",
                "'a-b'",
                "['a/b']",
                "['.env']",
                "['#method_definition-0']",
                "[Symbol.iterator]",
                "[KEY + '2']",
                "[KEY]",
                "[\"with space\"]",
                "[-1]",
                "''",
                "['1e3']",
                "[`a${KEY}`]",
                "#priv",
                "plain",
            ]
        );
    }

    /// `static void __cdecl do_library_init(void);` is aws-lc
    /// `crypto/crypto.c:52`.
    const MSVC_CALL_SRC: &str = "\
static void __cdecl do_library_init(void);
static void __cdecl f(void);
void __stdcall g(int);
int __fastcall *h(void);
void __cdecl k(void) {}
int (__stdcall *fp)(int);
";

    #[test]
    fn c_msvc_calling_convention_declarations_are_named() {
        assert_eq!(
            names_of("a.c", MSVC_CALL_SRC, "declaration"),
            ["do_library_init", "f", "g", "h", "fp"]
        );
        assert_eq!(names_of("a.c", MSVC_CALL_SRC, "function_definition"), ["k"]);
    }

    #[test]
    fn cpp_conversion_operators_keep_their_target_type() {
        let src = "\
class Widget {
public:
    operator const char *() const;
    operator int &();
    operator int();
    operator bool() const { return true; }
};
Widget::operator bool() const { return true; }
Widget::operator const char *() const { return nullptr; }
";
        assert_eq!(
            names_of("a.cpp", src, "declaration"),
            ["operator const char *", "operator int &", "operator int"]
        );
        assert_eq!(
            names_of("a.cpp", src, "function_definition"),
            [
                "operator bool",
                "Widget::operator bool",
                "Widget::operator const char *"
            ]
        );
    }

    #[test]
    fn member_bodies_are_unnamed() {
        let ts = "\
class Box { v = 1; m() {} }
interface I { m(): void; }
enum E { A, B = 2 }
";
        assert_eq!(names_of("a.ts", ts, "class_body"), [""]);
        assert_eq!(names_of("a.ts", ts, "interface_body"), [""]);
        assert_eq!(names_of("a.ts", ts, "enum_body"), [""]);
        assert_eq!(names_of("a.ts", ts, "public_field_definition"), ["v"]);
        assert_eq!(
            names_of("a.js", "class Box { m() {} }\n", "class_body"),
            [""]
        );
        let java = "\
class Geo { int count; void run() {} }
interface Shape { void area(); }
enum Color { RED, GREEN; void paint() {} }
";
        assert_eq!(names_of("A.java", java, "class_body"), [""]);
        assert_eq!(names_of("A.java", java, "interface_body"), [""]);
        assert_eq!(names_of("A.java", java, "enum_body"), [""]);
        assert_eq!(names_of("A.java", java, "enum_body_declarations"), [""]);
        assert_eq!(names_of("A.java", java, "class_declaration"), ["Geo"]);
    }

    const CPP_ARMS_SRC: &str = "\
struct A {
    friend class Other;
    friend void swap(A &a, A &b);
    friend class Vec<int>;
    template <> void f<int>(int);
    void m<int>();
    int x [[maybe_unused]];
};
auto [first, second] = pair();
int y [[maybe_unused]] = 1;
template <typename T> requires Small<T> T twice(T t) { return t; }
template <typename T> /* doc */ T id(T t) { return t; }
";

    #[test]
    fn cpp_friend_declarations_name_what_they_befriend() {
        assert_eq!(
            names_of("a.cpp", CPP_ARMS_SRC, "friend_declaration"),
            ["Other", "swap", "Vec<int>"]
        );
    }

    #[test]
    fn cpp_template_ids_are_kept_whole() {
        assert_eq!(
            names_of("a.cpp", CPP_ARMS_SRC, "template_declaration"),
            ["f<int>", "twice", "id"]
        );
        assert_eq!(
            names_of("a.cpp", CPP_ARMS_SRC, "field_declaration"),
            ["m<int>", "x"]
        );
    }

    #[test]
    fn cpp_structured_and_attributed_declarators_are_named() {
        assert_eq!(
            names_of("a.cpp", CPP_ARMS_SRC, "declaration"),
            ["swap", "f<int>", "[first, second]", "y"]
        );
    }

    #[test]
    fn c_typedef_of_a_builtin_spelling_is_named_after_it() {
        assert_eq!(
            names_of("a.c", "typedef _Bool bool;\n", "type_definition"),
            ["bool"]
        );
        assert_eq!(
            names_of("a.c", "typedef SSIZE_T ssize_t;\n", "type_definition"),
            ["ssize_t"]
        );
    }

    #[test]
    fn ts_computed_key_with_more_than_one_named_child_stays_as_written() {
        let src = "\
class Box {
  ['k' /* c */]() {}
  [/* c */ 'k']() {}
}
";
        assert_eq!(
            names_of("a.ts", src, "method_definition"),
            ["['k' /* c */]", "[/* c */ 'k']"]
        );
    }

    #[test]
    fn ts_construct_signature_is_named_as_typescript_names_it() {
        let src = "interface Ctor { new (x: number): Foo; new (): Foo; }\n";
        assert_eq!(
            names_of("a.ts", src, "construct_signature"),
            ["new()", "new()"]
        );
    }

    #[test]
    fn cpp_nested_qualified_conversion_operator_stops_at_its_parameters() {
        let src = "\
namespace ns { struct Widget { operator bool() const; }; }
ns::Widget::operator bool() const { return true; }
";
        assert_eq!(
            names_of("a.cpp", src, "function_definition"),
            ["ns::Widget::operator bool"]
        );
    }

    #[test]
    fn cpp_conversion_operator_written_across_lines_is_one_line() {
        let src = "\
class Widget {
public:
    operator const
        char *() const;
    operator   int  &();
};
";
        assert_eq!(
            names_of("a.cpp", src, "declaration"),
            ["operator const char *", "operator int &"]
        );
    }

    /// aws-lc `ssl/ssl_test.cc:1779` writes a base class across lines; the
    /// field-free fallback reads raw text, so its name is put on one line,
    /// and a comment inside it leaves no name at all.
    #[test]
    fn cpp_fallback_names_are_one_line_and_hold_no_comment() {
        let src = "\
class A : public testing::TestWithParam<
          std::tuple<int, long>> {};
class B : public ns /* why */ ::Base {};
";
        assert_eq!(
            names_of("a.cpp", src, "base_class_clause"),
            ["testing::TestWithParam< std::tuple<int, long>>", ""]
        );
    }

    #[test]
    fn cpp_whole_names_for_nested_namespaces_specializations_and_division() {
        let src = "\
namespace a::b::c { int x; }
template <> struct Box<int> { int v; };
struct Frac {
    Frac operator/(Frac b) const;
    Frac &operator/=(Frac b);
};
";
        assert_eq!(names_of("a.cpp", src, "namespace_definition"), ["a::b::c"]);
        assert_eq!(names_of("a.cpp", src, "struct_specifier")[0], "Box<int>");
        assert_eq!(
            names_of("a.cpp", src, "field_declaration")[1..],
            ["operator/", "operator/="]
        );
    }

    #[test]
    fn c_names_that_are_cpp_keywords_stay_names_in_c() {
        let src = "\
void f(void) {
  CONF_VALUE template;
  const vec_t this = prod[0];
}
";
        assert_eq!(names_of("a.c", src, "declaration"), ["template", "this"]);
    }

    /// The first outline symbol of `kind` starting on 1-based `line` of a
    /// file named `file` holding `src`, and whether the top-level item
    /// holding it contains an ERROR or MISSING node.
    fn symbol_at(file: &str, src: &str, kind: &str, line: usize) -> (String, bool) {
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, file, src.as_bytes());
        let opened = open_file(&p).unwrap();
        let outline = outline(&opened.tree, opened.lang);
        let Some(s) = outline
            .iter()
            .find(|s| s.kind == kind && s.start_line == line)
        else {
            panic!("no {kind} on line {line} of {file}: {outline:?}");
        };
        let root = opened.tree.native_root().expect("a code grammar");
        let item = root
            .children(&mut root.walk())
            .find(|c| c.start_byte() <= s.start_byte && s.end_byte <= c.end_byte())
            .expect("a top-level item holds every symbol");
        (s.name.clone(), root.is_error() || item.has_error())
    }

    /// The name of the symbol [`symbol_at`] finds, which the grammar must
    /// misread: these fixtures are real code the grammar cannot read, and a
    /// clean top-level item would mean the misread under test is not
    /// exercised.
    fn misread_name_at(file: &str, src: &str, kind: &str, line: usize) -> String {
        let (name, misread) = symbol_at(file, src, kind, line);
        assert!(
            misread,
            "{file}:{line} {kind} must sit in a misread top-level item"
        );
        name
    }

    /// aws-lc `crypto/crypto.c:62`: the macro return type ends a misread
    /// item of its own, and the definition after it is a clean item the
    /// grammar reads with `do_library_init` as the type and `(void)` as a
    /// parenthesized declarator holding `void`.
    #[test]
    fn c_definition_with_a_macro_return_type_is_named_after_the_function() {
        let src = "\
static void OPENSSL_CDECL do_library_init(void) {
#if defined(NEED_CPUID)
  OPENSSL_cpuid_setup();
#endif
}
";
        let (name, misread) = symbol_at("a.c", src, "function_definition", 1);
        assert_eq!((name.as_str(), misread), ("do_library_init", false));
    }

    /// libFuzzer `FuzzerCommand.h`, a C++ header read as C: a constructor's
    /// member initializer, two lambdas and an `auto` variable.
    const FUZZER_COMMAND_H: &str = "\
namespace fuzzer {

class Command final {
public:
  static inline const char *ignoreRemainingArgs() {
    return \"-ignore_remaining_args=1\";
  }

  Command() : CombinedOutAndErr(false) {}

  bool hasFlag(const std::string &Flag) const {
    std::string Arg(\"-\" + Flag + \"=\");
    auto IsMatch = [&](const std::string &Other) {
      return Arg.compare(0, std::string::npos, Other, 0, Arg.length()) == 0;
    };
    return std::any_of(Args.begin(), endMutableArgs(), IsMatch);
  }

  std::string getFlagValue(const std::string &Flag) const {
    auto i = endMutableArgs();
    auto j = std::find_if(Args.begin(), i, IsMatch);
    return result;
  }
};

}  // namespace fuzzer
";

    /// Every expected name in the misread tests below is what the bage
    /// 0.11.0 binary's `show --format json` names that block of the same
    /// bytes: a misread keeps its v0.11.0 name exactly, whatever word that
    /// was.
    #[test]
    fn cpp_read_as_c_keeps_its_v0_11_0_names() {
        let f = "FuzzerCommand.h";
        assert_eq!(
            misread_name_at(f, FUZZER_COMMAND_H, "function_definition", 9),
            "CombinedOutAndErr"
        );
        assert_eq!(
            misread_name_at(f, FUZZER_COMMAND_H, "function_definition", 13),
            "IsMatch"
        );
        assert_eq!(misread_name_at(f, FUZZER_COMMAND_H, "declaration", 21), "j");
    }

    /// libFuzzer `FuzzerTracePC.h:272`.
    #[test]
    fn cpp_lambda_read_as_c_keeps_its_v0_11_0_name() {
        let src = "\
class TracePC {
  // Step function, grows similar to 8 * Log_2(A).
  auto StackDepthStepFunction = [](size_t A) -> size_t {
    if (!A)
      return A;
    auto Log2 = Log(A);
    return (Log2 + 1) * 8 + ((A >> Log2) & 7);
  };
};
";
        assert_eq!(
            misread_name_at("FuzzerTracePC.h", src, "function_definition", 3),
            "StackDepthStepFunction"
        );
    }

    /// aws-lc `include/openssl/span.h:89`: the constructors read as C run
    /// into one declaration whose declarator slot holds `typename`.
    #[test]
    fn cpp_constructor_read_as_c_keeps_its_v0_11_0_name() {
        let src = "\
extern \"C++\" {

BSSL_NAMESPACE_BEGIN

template <typename T>
class Span : private internal::SpanBase<const T> {
 private:
  static const size_t npos = static_cast<size_t>(-1);

 public:
  constexpr Span() : Span(nullptr, 0) {}
  constexpr Span(T *ptr, size_t len) : data_(ptr), size_(len) {}

  template <size_t N>
  constexpr Span(T (&array)[N]) : Span(array, N) {}

  template <
      typename C,
      typename = typename std::enable_if<
          std::is_convertible<decltype(std::declval<C>().data()), T *>::value &&
          std::is_integral<decltype(std::declval<C>().size())>::value>::type,
      typename = typename std::enable_if<std::is_const<T>::value, C>::type>
  Span(const C &container) : data_(container.data()), size_(container.size()) {}
};

BSSL_NAMESPACE_END

}  // extern C++
";
        assert_eq!(misread_name_at("span.h", src, "declaration", 11), "Span");
        // abseil `numeric/int128.h:354-356`.
        let int128 = "\
class int128 {
 public:
  int128() = default;

  // Constructors from arithmetic types
  constexpr int128(int v);                 // NOLINT(runtime/explicit)
  constexpr int128(long v);                // NOLINT(runtime/int)
};
";
        for line in [6, 7] {
            assert_eq!(
                misread_name_at("int128.h", int128, "declaration", line),
                "int128"
            );
        }
    }

    /// aws-lc `crypto/test/file_util.h:83`: the grammar invents the
    /// declarator of `auto x = …`, so the chain ends on a MISSING node.
    #[test]
    fn c_auto_variable_with_an_invented_declarator_keeps_its_v0_11_0_name() {
        let src = "\
class TemporaryFile {
 public:
  TemporaryFile& operator=(TemporaryFile&&other) {
    auto old_other_path = other.path_;
    other.path_ = {};
    return *this;
  }
};
";
        assert_eq!(
            misread_name_at("file_util.h", src, "declaration", 4),
            "old_other_path"
        );
    }

    /// libFuzzer `FuzzerTracePC.cpp:492`: the attribute macros read as a
    /// scope, a missing `::` and an ERROR holding the comments. v0.11.0
    /// named the declarator after that whole text, lines and comments
    /// included, and a misread keeps it.
    #[test]
    fn a_qualified_name_around_an_error_keeps_its_v0_11_0_name() {
        let src = "\
ATTRIBUTE_INTERFACE
ATTRIBUTE_NO_SANITIZE_ALL
ATTRIBUTE_TARGET_POPCNT
// Now the __sanitizer_cov_trace_const_cmp[1248] callbacks just mimic
// the behaviour of __sanitizer_cov_trace_cmp[1248] ones.
void __sanitizer_cov_trace_const_cmp8(uint64_t Arg1, uint64_t Arg2) {
  fuzzer::TPC.HandleCmp(PC, Arg1, Arg2);
}
";
        let f = "FuzzerTracePC.cpp";
        assert_eq!(
            misread_name_at(f, src, "function_definition", 1),
            "ATTRIBUTE_INTERFACE"
        );
        assert_eq!(
            misread_name_at(f, src, "function_declarator", 2),
            "ATTRIBUTE_NO_SANITIZE_ALL\nATTRIBUTE_TARGET_POPCNT\n\
             // Now the __sanitizer_cov_trace_const_cmp[1248] callbacks just mimic\n\
             // the behaviour of __sanitizer_cov_trace_cmp[1248] ones.\n\
             void __sanitizer_cov_trace_const_cmp8"
        );
    }

    /// aws-lc `ssl/ssl_version_test.cc:15`: `class` read as a scope, so the
    /// base class is the `name` field of the definition's declarator.
    #[test]
    fn a_class_head_misread_as_a_qualified_definition_keeps_its_v0_11_0_name() {
        let src = "\
BSSL_NAMESPACE_BEGIN

// SSLVersionTest executes its test cases under all available protocol
// versions.
class SSLVersionTest
    : public ::testing::TestWithParam<::std::tuple<VersionParam, int>> {
 protected:
  void SetUp() { ResetContexts(); }
};
";
        assert_eq!(
            misread_name_at("ssl_version_test.cc", src, "function_definition", 1),
            "BSSL_NAMESPACE_BEGIN"
        );
    }

    /// aws-lc ML-DSA `poly.c:659` and `builtin_curves.h:16`: an unexpanded
    /// alignment or unused macro leaves the builtin type in the declarator
    /// slot.
    #[test]
    fn a_misread_builtin_type_in_the_declarator_slot_keeps_its_v0_11_0_name() {
        let src = "\
void mld_poly_uniform(mld_poly *a)
{
  unsigned int ctr;
  MLD_ALIGN uint8_t buf[MLD_POLY_UNIFORM_NBLOCKS * MLD_STREAM128_BLOCKBYTES];
  mld_xof128_ctx state;
}
";
        assert_eq!(
            misread_name_at("poly.c", src, "declaration", 4),
            "MLD_ALIGN"
        );
        let curves = "\
OPENSSL_UNUSED static const uint64_t kP224B[] = {
    0x270b39432355ffb4, 0x5044b0b7d7bfd8ba,
};
";
        assert_eq!(
            misread_name_at("builtin_curves.h", curves, "declaration", 1),
            "OPENSSL_UNUSED"
        );
    }

    /// aws-lc `crypto/test/file_test.h:81`: an enum in a class read as C
    /// gets an invented declarator; the declaration still declares its tag.
    #[test]
    fn a_tag_with_an_invented_declarator_keeps_its_v0_11_0_name() {
        let src = "\
class FileTest {
 public:
  enum ReadResult {
    kReadSuccess,
    kReadEOF,
    kReadError,
  };
};
";
        assert_eq!(
            misread_name_at("file_test.h", src, "declaration", 3),
            "ReadResult"
        );
    }

    /// libFuzzer `FuzzerCorpus.h:52-65, 171-174`: statements read as field
    /// declarations, and a range-for.
    #[test]
    fn statements_read_as_declarations_keep_their_v0_11_0_names() {
        let src = "\
struct InputCorpus {
  bool DeleteFeatureFreq(uint32_t Idx) {
    if (FeatureFreqs.empty())
      return false;
    auto Lower = std::lower_bound(FeatureFreqs.begin(), FeatureFreqs.end(),
                                  std::pair<uint32_t, uint16_t>(Idx, 0));
    if (Lower != FeatureFreqs.end() && Lower->first == Idx) {
      FeatureFreqs.erase(Lower);
      return true;
    }
    return false;
  }
  ~InputCorpus() {
    for (auto II : Inputs)
      delete II;
  }
};
";
        let f = "FuzzerCorpus.h";
        for (kind, line, name) in [
            ("field_declaration", 4, "return"),
            ("field_declaration", 5, "Lower"),
            ("field_declaration", 7, "if"),
            ("field_declaration", 9, "return"),
            ("declaration", 14, "II"),
        ] {
            assert_eq!(misread_name_at(f, src, kind, line), name, "line {line}");
        }
    }

    /// aws-lc `crypto/test/abi_test.h:40-41`: C++ operators in a `.h` read
    /// as C.
    #[test]
    fn cpp_operators_read_as_c_keep_their_v0_11_0_names() {
        let src = "\
struct alignas(16) Reg128 {
  bool operator==(const Reg128 &x) const { return x.lo == lo && x.hi == hi; }
  uint64_t lo, hi;
};
";
        assert_eq!(
            misread_name_at("abi_test.h", src, "function_definition", 2),
            "operator"
        );
        let index = "\
struct Corpus {
  const Unit &operator[] (size_t Idx) const { return Inputs[Idx]->U; }
};
";
        for (kind, name) in [
            ("field_declaration", "Unit"),
            ("function_declarator", "operator"),
        ] {
            assert_eq!(misread_name_at("FuzzerCorpus.h", index, kind, 2), name);
        }
    }

    /// aws-lc `ssl/internal.h:4161`, `crypto/test/test_util.h:34`,
    /// `ssl/test/settings_writer.h:15`; libFuzzer `FuzzerDictionary.h:56`,
    /// `FuzzerCorpus.h:31`.
    #[test]
    fn cpp_members_read_as_c_keep_their_v0_11_0_names() {
        let init = "struct ssl_st {\n  long verify_result = X509_V_ERR_INVALID_CALL;\n};\n";
        assert_eq!(
            misread_name_at("internal.h", init, "field_declaration", 2),
            "X509_V_ERR_INVALID_CALL"
        );
        let label =
            "struct SettingsWriter {\n public:\n  SettingsWriter();\n\n  bool Commit();\n};\n";
        assert_eq!(
            misread_name_at("settings_writer.h", label, "field_declaration", 2),
            "public"
        );
        let ctor = "\
class DictionaryEntry {
 public:
  DictionaryEntry() {}
  DictionaryEntry(Word W) : W(W) {}
  DictionaryEntry(Word W, size_t PositionHint)
      : W(W), PositionHint(PositionHint) {}
  const Word &GetW() const { return W; }

  bool HasPositionHint() const {
    return PositionHint != std::numeric_limits<size_t>::max();
  }
  size_t GetPositionHint() const {
    assert(HasPositionHint());
    return PositionHint;
  }
};
";
        assert_eq!(
            misread_name_at("FuzzerDictionary.h", ctor, "function_definition", 4),
            "DictionaryEntry"
        );
        assert_eq!(
            misread_name_at("FuzzerDictionary.h", ctor, "declaration", 14),
            "return"
        );
        let bytes = "\
struct Bytes {
  Bytes(const uint8_t *data_arg, size_t len_arg) : span_(data_arg, len_arg) {}
  Bytes(const char *data_arg, size_t len_arg)
      : span_(reinterpret_cast<const uint8_t *>(data_arg), len_arg) {}

  explicit Bytes(const char *str)
      : span_(reinterpret_cast<const uint8_t *>(str), strlen(str)) {}
  explicit Bytes(const std::string &str)
      : span_(reinterpret_cast<const uint8_t *>(str.data()), str.size()) {}
  explicit Bytes(bssl::Span<const uint8_t> span) : span_(span) {}

  bssl::Span<const uint8_t> span_;
};
";
        for (line, name) in [(2, "data_arg"), (6, "Bytes")] {
            assert_eq!(
                misread_name_at("test_util.h", bytes, "field_declaration", line),
                name,
                "line {line}"
            );
        }
        let scoped = "\
namespace fuzzer {

struct InputInfo {
  Unit U;  // The actual input data.
  std::chrono::microseconds TimeOfUnit;
  uint8_t Sha1[kSHA1NumBytes];  // Checksum.
};

}  // namespace fuzzer
";
        assert_eq!(
            misread_name_at("FuzzerCorpus.h", scoped, "field_declaration", 5),
            "std"
        );
        let options = "\
struct FuzzingOptions {
  bool EntropicScalePerExecTime = false;
  std::string OutputCorpus;
  std::string ArtifactPrefix = \"./\";
};
";
        for line in [3, 4] {
            assert_eq!(
                misread_name_at("FuzzerOptions.h", options, "field_declaration", line),
                "std"
            );
        }
    }

    /// libFuzzer `FuzzerTracePC.h:238`: a method defined out of line, glued
    /// to a `::` the C grammar cannot read.
    #[test]
    fn cpp_out_of_line_method_read_as_c_keeps_its_v0_11_0_name() {
        let src = "\
class TracePC {
 public:
  size_t CollectFeatures(Callback HandleFeature) const;
};

inline size_t TracePC::CollectFeatures(Callback HandleFeature) const {
  return 0;
}
";
        assert_eq!(
            misread_name_at("FuzzerTracePC.h", src, "function_definition", 6),
            "TracePC"
        );
    }

    /// aws-lc `ssl/ssl_common_test.h:143`, libFuzzer `FuzzerCommand.h:37`,
    /// aws-lc `crypto/test/test_util.h:97`: what lands in the declarator slot
    /// is a template argument, a member being initialized, or a callee.
    #[test]
    fn cpp_heads_read_as_c_keep_their_v0_11_0_names() {
        let head = "class SSLTest : public testing::TestWithParam<SSLTestParam> {};\n";
        assert_eq!(
            misread_name_at("ssl_common_test.h", head, "function_definition", 1),
            "class"
        );
        // aws-lc `ssl/internal.h:1288`: the export macro lands in the slot
        // and the class name in the ERROR after it.
        let exported = "\
#define SSLBUFFER_MAX_CAPACITY INT_MAX
class OPENSSL_EXPORT SSLBuffer {
 public:
  SSLBuffer() {}
  ~SSLBuffer() { Clear(); }
};
";
        assert_eq!(
            misread_name_at("internal.h", exported, "function_definition", 2),
            "class"
        );
        let init = "\
class Command final {
public:
  Command() : CombinedOutAndErr(false) {}

  explicit Command(const std::vector<std::string> &ArgsToAdd)
      : Args(ArgsToAdd), CombinedOutAndErr(false) {}
};
";
        assert_eq!(
            misread_name_at("FuzzerCommand.h", init, "function_definition", 5),
            "explicit"
        );
        let body = "\
struct FileCloser {
  void operator()(FILE *f) const { fclose(f); }
};
";
        assert_eq!(
            misread_name_at("test_util.h", body, "field_declaration", 2),
            "const"
        );
    }

    /// aws-lc `ssl/ssl_x509.cc:1259`: the macro return type ends a
    /// declaration of its own, its `;` invented, with the macro argument in
    /// the declarator slot.
    #[test]
    fn a_macro_call_cut_off_as_a_declaration_keeps_its_v0_11_0_name() {
        let src = "\
static void set_client_CA_list(void) {
  sk_X509_NAME_pop_free(name_list, X509_NAME_free);
}

static STACK_OF(X509_NAME) *buffer_names_to_x509(
    const STACK_OF(CRYPTO_BUFFER) *names, STACK_OF(X509_NAME) **cached) {
  if (names == NULL) {
    return NULL;
  }
  return NULL;
}
";
        assert_eq!(
            misread_name_at("ssl_x509.cc", src, "declaration", 5),
            "STACK_OF"
        );
    }

    /// aws-lc `crypto/rand_extra/windows.c:58`: the unexpanded `WINAPI`
    /// macro makes the declarator an ERROR, and v0.11.0 named the typedef
    /// after its return type.
    #[test]
    fn a_misread_typedef_keeps_its_v0_11_0_name() {
        let src = "typedef BOOL (WINAPI *ProcessPrngFunction)(PBYTE pbData, SIZE_T cbData);\n";
        assert_eq!(misread_name_at("a.c", src, "type_definition", 1), "BOOL");
    }

    /// mimalloc `include/mimalloc.h:183`: the calling-convention macro
    /// inside the parentheses is an ERROR.
    #[test]
    fn a_macro_calling_convention_in_parentheses_keeps_its_v0_11_0_name() {
        let src = "typedef void (mi_cdecl mi_output_fun)(const char* msg, void* arg);\n";
        let f = "mimalloc.h";
        assert_eq!(misread_name_at(f, src, "type_definition", 1), "");
        assert_eq!(
            misread_name_at(f, src, "function_declarator", 1),
            "mi_output_fun"
        );
    }

    /// c-ares `ares.h:495, 566`: the deprecation macro makes the grammar
    /// read on past the declaration, and `struct iovec;` gets an invented
    /// declarator.
    #[test]
    fn a_tag_declaration_with_an_invented_declarator_keeps_its_v0_11_0_name() {
        let src = "\
CARES_EXTERN CARES_DEPRECATED_FOR(ares_init_options) int ares_init(
  ares_channel_t **channelptr);

CARES_EXTERN int ares_library_initialized(void);

struct iovec;

struct ares_socket_functions {
  ares_socket_t (*asocket)(int, int, int, void *);
};
";
        assert_eq!(misread_name_at("ares.h", src, "declaration", 6), "iovec");
    }

    /// c-ares `ares.h:1208`: a declaration whose own nodes are all sound
    /// but which sits inside an ERROR.
    #[test]
    fn a_declaration_inside_an_error_keeps_its_v0_11_0_name() {
        let src = "\
CARES_EXTERN CARES_DEPRECATED_FOR(ares_dns_record_create) int ares_mkquery(
  const char *name, int dnsclass, int type, unsigned short id, int rd,
  unsigned char **buf, int *buflen);

CARES_EXTERN ares_bool_t
ares_threadsafety(void);
";
        assert_eq!(
            misread_name_at("ares.h", src, "declaration", 6),
            "ares_threadsafety"
        );
    }

    /// The rule is the same for every grammar: a TypeScript interface
    /// holding a syntax error keeps the names v0.11.0 gave its members.
    #[test]
    fn a_misread_typescript_interface_keeps_its_v0_11_0_names() {
        let src = "\
interface Ctor {
  new (x: number): Foo;
  bad: = 1;
}
";
        assert_eq!(misread_name_at("a.ts", src, "interface_body", 1), "bad");
        assert_eq!(
            misread_name_at("a.ts", src, "construct_signature", 2),
            "Foo"
        );
    }

    /// A misread is scoped to the top-level item holding it: the item
    /// beside it is named from its fields, and every block inside the
    /// misread item keeps its v0.11.0 name, even one whose own nodes are
    /// sound.
    #[test]
    fn a_misread_is_scoped_to_its_top_level_item() {
        let src = "\
typedef void (mi_cdecl mi_output_fun)(const char* msg, void* arg);
Point make_point(int x, int y);
";
        assert!(symbol_at("a.h", src, "type_definition", 1).1);
        let (name, misread) = symbol_at("a.h", src, "declaration", 2);
        assert_eq!((name.as_str(), misread), ("make_point", false));
        let cpp = "\
namespace geo {
Point make_point(int x, int y);
int broken( ;
}
";
        assert_eq!(
            misread_name_at("a.cpp", cpp, "declaration", 2),
            "Point",
            "a sound declaration in a misread namespace"
        );
    }

    /// aws-lc `include/openssl/pool.h:4-5, 23-27, 80-97`: the `extern "C"`
    /// braces split across `#if` arms inside the include guard leave the
    /// whole root an ERROR, so no top-level item is a unit the grammar read,
    /// however clean its own nodes. The declaration here holds no ERROR yet
    /// keeps its v0.11.0 name.
    #[test]
    fn every_item_under_an_error_root_keeps_its_v0_11_0_name() {
        let src = "\
#ifndef OPENSSL_HEADER_POOL_H
#define OPENSSL_HEADER_POOL_H

#if defined(__cplusplus)
extern \"C\" {
#endif

DEFINE_STACK_OF(CRYPTO_BUFFER)

OPENSSL_EXPORT CRYPTO_BUFFER_POOL* CRYPTO_BUFFER_POOL_new(void);

#if defined(__cplusplus)
}  // extern C

extern \"C++\" {

BSSL_NAMESPACE_BEGIN

BORINGSSL_MAKE_DELETER(CRYPTO_BUFFER_POOL, CRYPTO_BUFFER_POOL_free)

BSSL_NAMESPACE_END

}  // extern C++

#endif

#endif  // OPENSSL_HEADER_POOL_H
";
        let dir = tempfile::tempdir().unwrap();
        let opened = open_file(&write_temp(&dir, "pool.h", src.as_bytes())).unwrap();
        let root = opened.tree.native_root().expect("a code grammar");
        assert!(root.is_error(), "the root must be an ERROR");
        let item = root
            .children(&mut root.walk())
            .find(|c| c.kind() == "declaration" && c.start_position().row == 9)
            .expect("the declaration is a top-level item");
        assert!(!item.has_error(), "the item itself must read clean");
        assert_eq!(
            misread_name_at("pool.h", src, "declaration", 10),
            "CRYPTO_BUFFER_POOL"
        );
    }

    /// The grammar can read code it cannot expand without an ERROR, leaving
    /// a keyword or builtin type where the name goes: aws-lc
    /// `crypto/evp_extra/p_dh_asn1.c:173` reads a macro statement and an
    /// `if` as a nested definition, mlx's `backend/cpu/unary_ops.h:171` a C++
    /// `operator()` and `backend/metal/kernels/fp8.h:74` `operator float()`
    /// in a `.h` read as C, libstdc++'s `bits/std.cc:3826` `export
    /// C_LIB_NAMESPACE {…}` with `export` in the type slot, and a macro
    /// call read as a declaration of `uint8_t`. None is named after the
    /// keyword, the builtin or the type; v0.11.0 named them
    /// `SET_DIT_AUTO_RESET`, `operator`, `operator`, `export` and
    /// `CONSTEXPR_ARRAY`.
    #[test]
    fn a_clean_parse_is_never_named_after_a_keyword_or_builtin_type() {
        let evp = "\
int EVP_PKEY_set1_DH(EVP_PKEY *pkey, DH *key) {
  SET_DIT_AUTO_RESET
  if (EVP_PKEY_assign_DH(pkey, key)) {
    DH_up_ref(key);
    return 1;
  }
  return 0;
}
";
        assert_eq!(
            names_of("a.c", evp, "function_definition"),
            ["EVP_PKEY_set1_DH", ""]
        );
        let op = "float operator()(uint8_t x) {\n  return 0;\n}\n";
        assert_eq!(names_of("a.h", op, "function_definition"), [""]);
        let cast = "operator float() thread {\n  return 0;\n}\n";
        assert_eq!(names_of("fp8.h", cast, "function_definition"), [""]);
        let builtin = "static CONSTEXPR_ARRAY(uint8_t);\n";
        assert_eq!(names_of("a.h", builtin, "declaration"), [""]);
        let export = "export C_LIB_NAMESPACE\n{\n  using std::isalnum;\n}\n";
        assert_eq!(names_of("a.cc", export, "function_definition"), [""]);
    }

    /// A definition whose declarator is bare parentheses reads cleanly when
    /// its return type is a macro on the line before (aws-lc
    /// `crypto/crypto.c:61-62`, mimalloc `src/init.c:575`): the parentheses
    /// are the parameter list and the word in the type slot is the function.
    #[test]
    fn a_clean_definition_with_bare_parentheses_is_named_after_the_type_word() {
        let src = "do_library_init(void) {\n}\n_mi_preloading(void) {\n  return 0;\n}\n";
        assert_eq!(
            names_of("a.c", src, "function_definition"),
            ["do_library_init", "_mi_preloading"]
        );
    }

    /// A type keyword the grammar files as a type word is a type, never a
    /// function, so bare parentheses after one name nothing.
    #[test]
    fn a_type_keyword_before_bare_parentheses_is_no_name() {
        let src = "_Bool (x) {\n  return 0;\n}\n";
        assert_eq!(names_of("a.c", src, "function_definition"), [""]);
    }

    /// A builtin type in the name slot outside a typedef declares nothing,
    /// so a specialization that only spells one is unnamed.
    #[test]
    fn a_builtin_type_outside_a_typedef_is_no_name() {
        assert_eq!(
            names_of("a.cpp", "template <> int;\n", "template_declaration"),
            [""]
        );
    }

    /// A comment inside a name is not part of it, and the space it leaves
    /// keeps the tokens on either side apart.
    #[test]
    fn a_comment_inside_a_cpp_name_is_left_out_of_it() {
        let src = "\
template <> struct Box</*n*/3> { int v; };
void ns /* why */ ::f() {}
struct F { F &operator /* x */ =(const F &); };
";
        assert_eq!(names_of("a.cpp", src, "struct_specifier")[0], "Box< 3>");
        assert_eq!(names_of("a.cpp", src, "function_definition"), ["ns ::f"]);
        assert_eq!(names_of("a.cpp", src, "field_declaration")[1], "operator =");
    }

    /// `Tree::root` is public, so a caller can hand [`outline`] a tree that
    /// no longer matches its engine tree. A node is then never named from
    /// another node's fields: one whose twin is missing or of another kind
    /// keeps its v0.11.0 name.
    #[test]
    fn a_node_is_never_named_from_a_twin_that_does_not_line_up() {
        let src = "\
int f(void) {
  return 0;
}
Point make_point(int x, int y);
Point other(int z);
";
        let dir = tempfile::tempdir().unwrap();
        let named = |edit: fn(&mut Vec<Node>)| {
            let mut opened = open_file(&write_temp(&dir, "a.c", src.as_bytes())).unwrap();
            edit(&mut opened.tree.root.children);
            outline(&opened.tree, opened.lang)
                .into_iter()
                .filter(|s| s.kind == "declaration")
                .map(|s| s.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(named(|_| {}), ["make_point", "other"]);
        assert_eq!(
            named(|c| c.swap(0, 1)),
            ["Point", "other"],
            "a kind mismatch"
        );
        assert_eq!(named(|c| drop(c.remove(1))), ["Point"], "a count mismatch");
    }
}
