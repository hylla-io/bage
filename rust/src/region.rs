//! Båge's region-anchoring primitives: the content-anchored edit unit (SPEC
//! §8) and the concurrency-safe resolver that maps a possibly-stale region
//! back onto the live file (ADR-0003).
//!
//! A region is addressed by a byte range plus a region_hash — the xxHash
//! `{:016x}` of the region's NORMALIZED bytes (HYLLA_NODE_CONTRACT.md §4,
//! byte-identical with Hylla and the Go implementation so a whitespace-only
//! reformat does not false-conflict). The hash does one job here: it verifies
//! that the bytes at a candidate location are the block the edit targets.
//! Relocation (a benign concurrent shift moved the region) is the CST's job:
//! when the in-place hash no longer matches, the resolver reparses the live
//! file and matches the region_hash against every node. Disambiguation of
//! identical-content twins is NOT attempted — two matches are reported as
//! Ambiguous and rejected rather than guessed. Båge over-rejects on purpose:
//! corruption is never acceptable, a rejected edit is.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::hashing::{Hasher, XxHasher, norm_hash};
use crate::parser::{ByteRange, Lang, ParserPort};

/// The `start_byte` value marking a [`Region`] as line-addressed: the byte
/// range is unknown and must be derived from `start_line`/`end_line` via a
/// [`LineIndex`] (`resolve_lines`) before the region can be used.
pub const LINE_SENTINEL: i64 = -1;

/// A content-anchored locator into a file: a byte range, the corresponding
/// line/col range, and the region_hash that anchors it by content (SPEC
/// §8.1). It mirrors Hylla's per-node locator bundle minus graph identity.
///
/// Byte offsets are authoritative; line/col are derived conveniences. When
/// `start_byte` is [`LINE_SENTINEL`] the region is line-addressed and the
/// byte range must be resolved from the line range via a [`LineIndex`]
/// before use. Offsets are `i64` (not `usize`) precisely to carry that
/// sentinel; they are checked and converted at the resolve boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// The file the region lives in.
    pub path: String,
    /// Inclusive starting byte offset, or [`LINE_SENTINEL`] to mark the
    /// region as line-addressed (resolve via `start_line`/`end_line`).
    pub start_byte: i64,
    /// Exclusive ending byte offset.
    pub end_byte: i64,
    /// 1-based starting line.
    pub start_line: i64,
    /// 1-based ending line.
    pub end_line: i64,
    /// 0-based starting byte column within `start_line`.
    pub start_col: i64,
    /// 0-based ending byte column within `end_line`.
    pub end_col: i64,
    /// The xxHash `{:016x}` of the region's NORMALIZED bytes (matches
    /// HYLLA_NODE_CONTRACT §4 so it is byte-identical with Hylla's), or `""`
    /// when the region carries no anchor (single-model file mode), in which
    /// case the given byte range is treated as authoritative.
    pub region_hash: String,
}

/// The per-file drift gate (SPEC §8.1, HYLLA_NODE_CONTRACT.md §2):
/// `raw_hash` gates byte-offset validity; `norm_hash` classifies
/// whitespace-only drift.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileAnchor {
    /// The file the anchor describes.
    pub path: String,
    /// xxHash `{:016x}` of the file's RAW bytes — the byte-offset gate.
    pub raw_hash: String,
    /// xxHash `{:016x}` of the file's normalized bytes — the drift
    /// classifier.
    pub norm_hash: String,
}

/// A region-anchored edit: replace the bytes of `region` with `new_text`
/// (SPEC §8.1). The model echoes a shown region_hash; it never computes a
/// hash or resends old text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    /// The content-anchored target of the edit.
    pub region: Region,
    /// The replacement text for the region's bytes.
    pub new_text: String,
}

/// The write-back contract to Hylla (SPEC §8.2): the changed byte range plus
/// the recomputed region/file hashes and the new line range, so Hylla
/// re-ingests only the changed region.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditResult {
    /// The file that was edited.
    pub path: String,
    /// Inclusive starting byte offset of the changed range.
    pub changed_start: usize,
    /// Exclusive ending byte offset of the changed range.
    pub changed_end: usize,
    /// The region_hash of the post-edit region bytes.
    pub new_region_hash: String,
    /// The post-edit file raw hash.
    pub new_file_raw_hash: String,
    /// The post-edit file normalized hash.
    pub new_file_norm_hash: String,
    /// 1-based starting line of the post-edit region.
    pub new_start_line: usize,
    /// 1-based ending line of the post-edit region.
    pub new_end_line: usize,
}

/// Returns the region_hash — the xxHash `{:016x}` of the NORMALIZED bytes of
/// `src[start..end]` (HYLLA_NODE_CONTRACT §4), byte-identical with Hylla's
/// region_hash, so a whitespace-only reformat of the block does not change
/// it. Panics if the range is out of bounds, which signals a caller bug, not
/// drift.
pub fn hash_region(src: &[u8], start: usize, end: usize) -> String {
    norm_hash(&XxHasher, &src[start..end])
}

/// Reports how [`resolve`] located a region against the live file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveStatus {
    /// The region_hash matched the bytes at the region's own offset; the
    /// range is used as-is.
    Exact,
    /// A concurrent edit moved the region but not its content: the
    /// region_hash matched exactly one node at a new offset (a benign
    /// shift).
    Shifted,
    /// No live node matched the region_hash: the region's own content
    /// changed — a hard reject.
    Conflict,
    /// More than one live node matched the region_hash: identical twins that
    /// the resolver refuses to guess between — a hard reject.
    Ambiguous,
}

impl fmt::Display for ResolveStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ResolveStatus::Exact => "exact",
            ResolveStatus::Shifted => "shifted",
            ResolveStatus::Conflict => "conflict",
            ResolveStatus::Ambiguous => "ambiguous",
        })
    }
}

/// A resolve failure. Each variant carries its [`ResolveStatus`]
/// classification implicitly — Conflict and Ambiguous are the two hard
/// rejects; Parse means the live file could not be parsed at all (also a
/// conflict-class reject).
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("region: parse live file {path:?}: {source}")]
    Parse {
        path: String,
        source: crate::parser::ParseError,
    },
    #[error("region: {path:?} region_hash {hash} no longer matches any node (conflict)")]
    Conflict { path: String, hash: String },
    #[error(
        "region: {path:?} region_hash {hash} matches {count} nodes (ambiguous); refusing to guess"
    )]
    Ambiguous {
        path: String,
        hash: String,
        count: usize,
    },
    #[error("region: {path:?} byte range [{start}:{end}] is not a valid range")]
    InvalidRange { path: String, start: i64, end: i64 },
}

impl ResolveError {
    /// The status classification of the failure.
    pub fn status(&self) -> ResolveStatus {
        match self {
            ResolveError::Ambiguous { .. } => ResolveStatus::Ambiguous,
            _ => ResolveStatus::Conflict,
        }
    }
}

/// Maps `r` onto the live file bytes, returning the byte range to edit and
/// how it was located (ADR-0003, the concurrency core). It is the
/// resolve-under-lock step: callers hold the per-file lock and pass the
/// current file bytes, so every edit sees prior concurrent commits.
///
/// Resolution:
/// - If `r.region_hash` is empty (no anchor, single-model file mode) the
///   given byte range is authoritative and returned as `Exact`.
/// - If `hash_region(live, start, end) == r.region_hash` the region is in
///   place ⇒ `Exact`, used as-is.
/// - Otherwise `live` is parsed under `lang` and every CST node whose
///   region_hash equals `r.region_hash` is collected: exactly one ⇒
///   `Shifted` (benign), its range is returned; zero ⇒ `Conflict`; more than
///   one ⇒ `Ambiguous`. Conflict and Ambiguous return an error — resolve
///   never guesses and never misapplies.
///
/// An in-place range that is out of bounds falls through to the
/// parse-and-match path rather than panicking, so a stale offset can never
/// crash the resolver.
pub fn resolve(
    p: &dyn ParserPort,
    lang: Lang,
    live: &[u8],
    r: &Region,
) -> Result<(usize, usize, ResolveStatus), ResolveError> {
    // Single-model file mode: no anchor, the range is authoritative.
    if r.region_hash.is_empty() {
        let (start, end) = checked_range(r)?;
        return Ok((start, end, ResolveStatus::Exact));
    }

    // Fast path: the region is in place if its bytes still hash to
    // region_hash.
    if let Ok((start, end)) = checked_range(r) {
        if end <= live.len() && hash_region(live, start, end) == r.region_hash {
            return Ok((start, end, ResolveStatus::Exact));
        }
    }

    // Slow path: parse the live file and match the region_hash against the
    // CST.
    let tree = p.parse(lang, live).map_err(|e| ResolveError::Parse {
        path: r.path.clone(),
        source: e,
    })?;
    let matches = match_nodes(&tree.root, live, &r.region_hash);
    match matches.len() {
        1 => Ok((matches[0].start, matches[0].end, ResolveStatus::Shifted)),
        0 => Err(ResolveError::Conflict {
            path: r.path.clone(),
            hash: r.region_hash.clone(),
        }),
        n => Err(ResolveError::Ambiguous {
            path: r.path.clone(),
            hash: r.region_hash.clone(),
            count: n,
        }),
    }
}

/// Converts a region's `i64` byte range into a checked `(usize, usize)`
/// half-open range, rejecting negative or inverted offsets (including the
/// line sentinel of an unresolved line-addressed region).
fn checked_range(r: &Region) -> Result<(usize, usize), ResolveError> {
    let invalid = || ResolveError::InvalidRange {
        path: r.path.clone(),
        start: r.start_byte,
        end: r.end_byte,
    };
    let start = usize::try_from(r.start_byte).map_err(|_| invalid())?;
    let end = usize::try_from(r.end_byte).map_err(|_| invalid())?;
    if end < start {
        return Err(invalid());
    }
    Ok((start, end))
}

/// Walks the CST and collects the byte range of every node whose bytes hash
/// to `want`. Each distinct byte range is reported once: a node and an
/// only-child commonly share a span, and counting that span twice would
/// falsely report Ambiguous, so spans are de-duplicated.
fn match_nodes(root: &crate::parser::Node, src: &[u8], want: &str) -> Vec<ByteRange> {
    let mut matches = Vec::new();
    let mut seen: HashSet<ByteRange> = HashSet::new();
    root.walk(&mut |n| {
        if n.end_byte <= src.len()
            && n.start_byte <= n.end_byte
            && hash_region(src, n.start_byte, n.end_byte) == want
        {
            let br = ByteRange {
                start: n.start_byte,
                end: n.end_byte,
            };
            if seen.insert(br) {
                matches.push(br);
            }
        }
    });
    matches
}

/// Computes a file's [`FileAnchor`] with the given hasher.
pub fn file_anchor(h: &dyn Hasher, path: &str, raw: &[u8]) -> FileAnchor {
    FileAnchor {
        path: path.to_string(),
        raw_hash: crate::hashing::raw_hash(h, raw),
        norm_hash: crate::hashing::norm_hash(h, raw),
    }
}

/// Maps between 1-based line numbers and byte offsets in a fixed source
/// buffer. It records the byte offset at which each line begins, so line/col
/// lookups and the line-addressed → byte-range resolution (SPEC §8.1) are
/// O(log n) or O(1).
///
/// Columns are 0-based BYTE offsets within a line — the UTF-8 byte-col
/// convention of HYLLA_NODE_CONTRACT.md §1 (LSP converts to UTF-16 at its
/// own boundary). Line endings are located by `\n`; a preceding `\r` (CRLF)
/// stays part of the line's bytes, so byte offsets remain faithful to the
/// raw file.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// `line_starts[i]` is the byte offset where line `i+1` begins.
    /// `line_starts[0]` is always 0; a trailing entry past the final `\n`
    /// represents the (possibly empty) last line.
    line_starts: Vec<usize>,
    /// The source length, the clamp bound for byte lookups.
    size: usize,
}

impl LineIndex {
    /// Builds a `LineIndex` over `src`. The source is not retained; only
    /// line offsets and the length are kept.
    pub fn new(src: &[u8]) -> LineIndex {
        let mut starts = vec![0];
        for (i, &b) in src.iter().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex {
            line_starts: starts,
            size: src.len(),
        }
    }

    /// The number of lines. A buffer with a trailing `\n` counts the empty
    /// final line, matching the `line_starts` layout.
    pub fn lines(&self) -> usize {
        self.line_starts.len()
    }

    /// The byte offset where the 1-based line begins. Lines below 1 clamp to
    /// the first line's offset (0); lines past the last clamp to the buffer
    /// size, so out-of-range line numbers never index out of bounds.
    pub fn byte_for_line(&self, line: i64) -> usize {
        if line < 1 {
            return 0;
        }
        let line = line as usize;
        if line > self.line_starts.len() {
            return self.size;
        }
        self.line_starts[line - 1]
    }

    /// The 1-based line containing the byte offset. Offsets at or past the
    /// buffer size resolve to the last line. The boundary rule is half-open:
    /// the byte immediately after a `\n` belongs to the next line.
    pub fn line_for_byte(&self, off: usize) -> usize {
        let off = off.min(self.size);
        if off == 0 {
            return 1;
        }
        // Binary search for the greatest line start <= off.
        match self.line_starts.binary_search(&off) {
            Ok(i) => i + 1,
            Err(i) => i, // i is the insertion point; the line is at i-1 (1-based i).
        }
    }

    /// The 1-based line and 0-based byte column of `off`.
    pub fn position_for_byte(&self, off: usize) -> (usize, usize) {
        let line = self.line_for_byte(off);
        let off = off.min(self.size);
        (line, off - self.line_starts[line - 1])
    }

    /// Populates `r`'s line/col fields from its byte range (1-based lines,
    /// 0-based byte cols) and returns the updated region. The end position
    /// is the point AT `end_byte` (the exclusive boundary), matching
    /// tree-sitter end-point semantics.
    pub fn fill_line_cols(&self, mut r: Region) -> Region {
        let (sl, sc) = self.position_for_byte(r.start_byte.max(0) as usize);
        let (el, ec) = self.position_for_byte(r.end_byte.max(0) as usize);
        r.start_line = sl as i64;
        r.start_col = sc as i64;
        r.end_line = el as i64;
        r.end_col = ec as i64;
        r
    }

    /// Turns a line-addressed region (`start_byte == LINE_SENTINEL`) into a
    /// byte-range region: `start_byte` becomes the start of `start_line` and
    /// `end_byte` the start of the line AFTER `end_line` (so the range
    /// covers `end_line` in full, including its newline). The line/col
    /// fields are then refreshed from the resolved bytes. A region that is
    /// already byte-addressed is returned unchanged.
    ///
    /// Lines clamp via `byte_for_line`, so an over-large `end_line` resolves
    /// to end-of-buffer rather than erroring; the caller's region_hash is
    /// the integrity check.
    pub fn resolve_lines(&self, r: Region) -> Region {
        if r.start_byte != LINE_SENTINEL {
            return r;
        }
        let mut r = r;
        r.start_byte = self.byte_for_line(r.start_line) as i64;
        r.end_byte = self.byte_for_line(r.end_line + 1) as i64;
        self.fill_line_cols(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Adapter;

    fn byte_region(path: &str, src: &[u8], start: usize, end: usize) -> Region {
        Region {
            path: path.to_string(),
            start_byte: start as i64,
            end_byte: end as i64,
            region_hash: hash_region(src, start, end),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_exact_in_place() {
        let a = Adapter::new();
        let src = b"package main\n\nfunc f() {}\n";
        let r = byte_region("m.go", src, 14, 25);
        let (s, e, st) = resolve(&a, Lang::Go, src, &r).unwrap();
        assert_eq!((s, e, st), (14, 25, ResolveStatus::Exact));
    }

    #[test]
    fn resolve_no_anchor_is_authoritative() {
        let a = Adapter::new();
        let src = b"anything at all";
        let r = Region {
            path: "x.txt".into(),
            start_byte: 3,
            end_byte: 8,
            ..Default::default()
        };
        let (s, e, st) = resolve(&a, Lang::Text, src, &r).unwrap();
        assert_eq!((s, e, st), (3, 8, ResolveStatus::Exact));
    }

    #[test]
    fn resolve_shifted_after_benign_prefix() {
        let a = Adapter::new();
        let old = b"package main\n\nfunc f() {}\n";
        let r = byte_region("m.go", old, 14, 25); // "func f() {}"
        // A concurrent edit prepended an import, shifting the function.
        let live = b"package main\n\nimport \"os\"\n\nfunc f() {}\n";
        let (s, e, st) = resolve(&a, Lang::Go, live, &r).unwrap();
        assert_eq!(st, ResolveStatus::Shifted);
        assert_eq!(&live[s..e], b"func f() {}");
    }

    #[test]
    fn resolve_conflict_when_content_changed() {
        let a = Adapter::new();
        let old = b"package main\n\nfunc f() {}\n";
        let r = byte_region("m.go", old, 14, 25);
        let live = b"package main\n\nfunc g() { panic(1) }\n";
        let err = resolve(&a, Lang::Go, live, &r).unwrap_err();
        assert_eq!(err.status(), ResolveStatus::Conflict);
    }

    #[test]
    fn resolve_ambiguous_twins_reject() {
        let a = Adapter::new();
        // Two byte-identical lines in text mode: two "line" nodes with the
        // same content hash at different offsets.
        let old = b"dup\nother\n";
        let r = byte_region("t.txt", old, 0, 4); // "dup\n"
        let live = b"x\ndup\nmid\ndup\n";
        let err = resolve(&a, Lang::Text, live, &r).unwrap_err();
        assert_eq!(err.status(), ResolveStatus::Ambiguous);
    }

    #[test]
    fn resolve_dedups_parent_child_same_span() {
        let a = Adapter::new();
        // JSON document whose root and sole value share a span; a shifted
        // resolve must not report the shared span twice as ambiguous.
        let old = b"[1,2]";
        let r = byte_region("d.json", old, 0, 5);
        let live = b" [1,2]"; // shifted by one byte
        let (s, e, st) = resolve(&a, Lang::Json, live, &r).unwrap();
        assert_eq!(st, ResolveStatus::Shifted);
        assert_eq!(&live[s..e], b"[1,2]");
    }

    #[test]
    fn resolve_stale_out_of_bounds_offset_never_panics() {
        let a = Adapter::new();
        let old = b"line one\nline two\n";
        let r = byte_region("t.txt", old, 9, 18); // "line two\n"
        let live = b"line two\n"; // file shrank below the stale offset
        let (s, e, st) = resolve(&a, Lang::Text, live, &r).unwrap();
        assert_eq!(st, ResolveStatus::Shifted);
        assert_eq!((s, e), (0, 9));
    }

    #[test]
    fn line_index_round_trips() {
        let src = b"ab\ncde\n\nfg";
        let li = LineIndex::new(src);
        assert_eq!(li.lines(), 4);
        assert_eq!(li.byte_for_line(1), 0);
        assert_eq!(li.byte_for_line(2), 3);
        assert_eq!(li.byte_for_line(3), 7);
        assert_eq!(li.byte_for_line(4), 8);
        // Clamps.
        assert_eq!(li.byte_for_line(0), 0);
        assert_eq!(li.byte_for_line(-3), 0);
        assert_eq!(li.byte_for_line(99), src.len());
        // line_for_byte half-open boundaries.
        assert_eq!(li.line_for_byte(0), 1);
        assert_eq!(li.line_for_byte(2), 1);
        assert_eq!(li.line_for_byte(3), 2);
        assert_eq!(li.line_for_byte(7), 3);
        assert_eq!(li.line_for_byte(9), 4);
        assert_eq!(li.line_for_byte(999), 4);
        assert_eq!(li.position_for_byte(5), (2, 2));
    }

    #[test]
    fn resolve_lines_covers_end_line_in_full() {
        let src = b"one\ntwo\nthree\n";
        let li = LineIndex::new(src);
        let r = Region {
            path: "t.txt".into(),
            start_byte: LINE_SENTINEL,
            start_line: 2,
            end_line: 2,
            ..Default::default()
        };
        let r = li.resolve_lines(r);
        assert_eq!((r.start_byte, r.end_byte), (4, 8));
        assert_eq!((r.start_line, r.end_line), (2, 3));
        // Already byte-addressed regions pass through unchanged.
        let b = Region {
            path: "t.txt".into(),
            start_byte: 1,
            end_byte: 2,
            ..Default::default()
        };
        assert_eq!(li.resolve_lines(b.clone()), b);
    }

    #[test]
    fn hash_region_matches_go_parity_vector() {
        // Same digest the Go binary produced for "hello world\n".
        let src = b"xx hello world\n yy";
        assert_eq!(hash_region(src, 3, 15), "5215e13b207d6d8c");
    }
}
