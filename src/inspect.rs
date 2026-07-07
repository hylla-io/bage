//! Read-only inspection facade: open + parse any file, list its addressable
//! blocks (the Outline), read sub-ranges, and surface parse-health defects.
//! Everything here is strictly read-only — nothing writes to disk.

use serde::{Deserialize, Serialize};

use crate::hashing::{self, Hasher};
use crate::parser::{Adapter, Lang, Node, ParserPort, Tree};
use crate::region::{self, LINE_SENTINEL, LineIndex, Region};

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
/// best-effort and may be empty when no identifier child is found.
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
    if !tree.has_native() {
        return outline_lines(&tree.source);
    }
    let mut out = Vec::new();
    walk_decls(&tree.root, &tree.source, lang, 0, &mut out);
    out
}

/// Recursively appends a symbol for every named outline-worthy node under
/// `n`, in source order. It always recurses (even into a matched node) so
/// methods nested in a class/impl/struct body — and nested data blocks like
/// JSON pairs inside objects — are captured. `depth` is 0 for direct
/// children of the root; TOML uses it to limit bare pairs to the top level.
/// The root itself is never emitted.
fn walk_decls(n: &Node, src: &[u8], lang: Lang, depth: usize, out: &mut Vec<Symbol>) {
    for c in &n.children {
        if c.named && is_outline_kind(lang, &c.kind, depth) {
            out.push(Symbol {
                kind: c.kind.clone(),
                name: symbol_name(lang, c, src),
                start_byte: c.start_byte,
                end_byte: c.end_byte,
                start_line: c.start_point.row + 1,
                end_line: c.end_point.row + 1,
            });
        }
        walk_decls(c, src, lang, depth + 1, out);
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

/// The declared identifier text for `n`, best-effort. It first looks at
/// `n`'s direct named children, then — since some grammars wrap the name one
/// level down (Go type_declaration → type_spec → type_identifier, C
/// declaration → declarator → identifier) — at the direct named children of
/// `n`'s named children. It stays shallow (≤2 levels) so it never grabs an
/// identifier from a function body. Empty when none is found.
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
/// extraction for the data-format grammars, the identifier-child search of
/// [`decl_name`] for everything else.
fn symbol_name(lang: Lang, n: &Node, src: &[u8]) -> String {
    match lang {
        Lang::Json => json_key_name(n, src),
        Lang::Yaml | Lang::Toml => first_named_child_text(n, src),
        Lang::Xml => tag_name(n, src, "Name"),
        Lang::Html => tag_name(n, src, "tag_name"),
        Lang::Css => child_kind_text(n, src, "selectors").trim().to_string(),
        _ => decl_name(n, src),
    }
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
}
