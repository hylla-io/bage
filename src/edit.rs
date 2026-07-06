//! In-memory round-trip edit primitives Båge applies before any file write:
//! drift classification (two hashes), a pure byte-range splice over a single
//! file's bytes, a byte-offset → row/col point helper, and a reparse that
//! prefers incremental parsing for the single-region path.
//!
//! Nothing here performs file I/O. Reading the live file, deciding to apply,
//! and writing the result are the session layer's responsibility (SPEC §5,
//! §6). This module is the pure core that layer composes.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::hashing::{self, Hasher};
use crate::parser::{InputEdit, Lang, ParseError, ParserPort, Point, Tree};

/// A byte-range replacement within one file: splice `new_text` over
/// `[start_byte, end_byte)`.
///
/// Serialization uses Go's default field names (`Path`, `StartByte`, …) so
/// WAL records written by the Go implementation replay here and vice versa.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileEdit {
    /// Absolute or relative path of the file to edit.
    pub path: String,
    /// Inclusive start of the byte range to replace.
    pub start_byte: usize,
    /// Exclusive end of the byte range to replace.
    pub end_byte: usize,
    /// The text spliced in for the range.
    pub new_text: String,
}

/// Classifies how a live file's bytes have drifted from the bytes a byte
/// range was grounded against (SPEC §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftStatus {
    /// The live raw bytes hash-match the expected raw hash, so the byte
    /// range is trustworthy and the edit may apply directly.
    Valid,
    /// The raw hash differs but the normalized hash matches: the change is
    /// whitespace-only and the range must be re-grounded (re-resolved)
    /// before applying, never slid blindly.
    WhitespaceOnly,
    /// The normalized hash differs: the content itself changed, so the
    /// range must be re-grounded from Hylla or the edit rejected.
    Real,
}

impl fmt::Display for DriftStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DriftStatus::Valid => "valid",
            DriftStatus::WhitespaceOnly => "whitespace-only",
            DriftStatus::Real => "real",
        })
    }
}

/// Classifies `live` against the hashes the caller's byte range was grounded
/// on. A raw match yields `Valid`; a raw mismatch with a normalized match
/// yields `WhitespaceOnly`; a normalized mismatch yields `Real`. The same
/// hasher must have produced `expected_raw` and `expected_norm`.
pub fn check_drift(
    h: &dyn Hasher,
    live: &[u8],
    expected_raw: &str,
    expected_norm: &str,
) -> DriftStatus {
    if hashing::raw_hash(h, live) == expected_raw {
        return DriftStatus::Valid;
    }
    if hashing::norm_hash(h, live) == expected_norm {
        return DriftStatus::WhitespaceOnly;
    }
    DriftStatus::Real
}

/// Errors from the pure splice primitives.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("edit: overlapping edits: [{a_start}:{a_end}] and [{b_start}:{b_end}]")]
    Overlap {
        a_start: usize,
        a_end: usize,
        b_start: usize,
        b_end: usize,
    },
    #[error("edit: byte range [{start}:{end}] out of bounds for length {len}")]
    OutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("edit: reparse: {0}")]
    Reparse(#[from] ParseError),
}

/// Applies every edit to one file's bytes purely, returning a fresh buffer.
/// Edits are reverse-sorted by `start_byte` (descending) so splicing an
/// earlier-offset edit never invalidates a later-offset edit's range.
/// Overlapping ranges are rejected because reverse-sorted application is
/// correct only for disjoint ranges. Out-of-range or inverted offsets are
/// rejected. Performs no I/O.
pub fn splice_edits(src: &[u8], edits: &[FileEdit]) -> Result<Vec<u8>, EditError> {
    if edits.is_empty() {
        return Ok(src.to_vec());
    }

    let mut sorted: Vec<&FileEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.start_byte));

    // sorted is descending by start_byte, so sorted[i+1] has the lower
    // start; they overlap when its end_byte extends past sorted[i]'s start.
    for w in sorted.windows(2) {
        if w[1].end_byte > w[0].start_byte {
            return Err(EditError::Overlap {
                a_start: w[1].start_byte,
                a_end: w[1].end_byte,
                b_start: w[0].start_byte,
                b_end: w[0].end_byte,
            });
        }
    }

    let mut out = src.to_vec();
    for e in sorted {
        out = splice(&out, e.start_byte, e.end_byte, &e.new_text)?;
    }
    Ok(out)
}

/// The zero-based row/column for `byte_offset` within `src`: row is the
/// count of `\n` bytes before the offset, and col is the number of bytes
/// since the last `\n` (or since the start of `src` on the first line).
/// Offsets at or past `src.len()` clamp to EOF.
pub fn point_at(src: &[u8], byte_offset: usize) -> Point {
    let end = byte_offset.min(src.len());
    let mut p = Point { row: 0, col: 0 };
    for &b in &src[..end] {
        if b == b'\n' {
            p.row += 1;
            p.col = 0;
        } else {
            p.col += 1;
        }
    }
    p
}

/// Splices `edits` into `old` and reparses the result. For a single edit and
/// a reusable `old_tree` it builds an [`InputEdit`] from [`point_at`] over
/// the old and new bytes and parses incrementally. Otherwise (`old_tree`
/// `None`, or a multi-edit batch where a single `InputEdit` cannot describe
/// the change) it does a full reparse. Returns the spliced bytes and the new
/// tree.
pub fn reparse(
    p: &dyn ParserPort,
    lang: Lang,
    old: &[u8],
    old_tree: Option<&mut Tree>,
    edits: &[FileEdit],
) -> Result<(Vec<u8>, Tree), EditError> {
    let new_bytes = splice_edits(old, edits)?;

    let old_tree = match (old_tree, edits.len()) {
        (Some(t), 1) => t,
        _ => {
            let tree = p.parse(lang, &new_bytes)?;
            return Ok((new_bytes, tree));
        }
    };

    let e = &edits[0];
    let new_end_byte = e.start_byte + e.new_text.len();
    let input = InputEdit {
        start_byte: e.start_byte,
        old_end_byte: e.end_byte,
        new_end_byte,
        start_point: point_at(old, e.start_byte),
        old_end_point: point_at(old, e.end_byte),
        new_end_point: point_at(&new_bytes, new_end_byte),
    };
    let tree = p.parse_incremental(lang, &new_bytes, old_tree, input)?;
    Ok((new_bytes, tree))
}

/// Replaces `src[start..end]` with `new_text`, guarding against out-of-range
/// or inverted offsets. Returns a freshly allocated buffer and never mutates
/// `src`.
fn splice(src: &[u8], start: usize, end: usize, new_text: &str) -> Result<Vec<u8>, EditError> {
    if end < start || end > src.len() {
        return Err(EditError::OutOfBounds {
            start,
            end,
            len: src.len(),
        });
    }
    let mut out = Vec::with_capacity(src.len() - (end - start) + new_text.len());
    out.extend_from_slice(&src[..start]);
    out.extend_from_slice(new_text.as_bytes());
    out.extend_from_slice(&src[end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::XxHasher;
    use crate::parser::Adapter;

    fn fe(start: usize, end: usize, text: &str) -> FileEdit {
        FileEdit {
            path: "f".into(),
            start_byte: start,
            end_byte: end,
            new_text: text.into(),
        }
    }

    #[test]
    fn check_drift_classifies() {
        let h = XxHasher;
        let grounded = b"fn x() {}\n";
        let raw = hashing::raw_hash(&h, grounded);
        let norm = hashing::norm_hash(&h, grounded);
        assert_eq!(check_drift(&h, grounded, &raw, &norm), DriftStatus::Valid);
        assert_eq!(
            check_drift(&h, b"fn x() {}   \r\n", &raw, &norm),
            DriftStatus::WhitespaceOnly
        );
        assert_eq!(
            check_drift(&h, b"fn y() {}\n", &raw, &norm),
            DriftStatus::Real
        );
    }

    #[test]
    fn splice_edits_applies_disjoint_in_any_order() {
        let src = b"0123456789";
        // Given in ascending order; reverse-sorted application must still
        // land both correctly.
        let out = splice_edits(src, &[fe(1, 3, "AA"), fe(7, 9, "B")]).unwrap();
        assert_eq!(out, b"0AA3456B9");
        // Same edits, opposite input order.
        let out2 = splice_edits(src, &[fe(7, 9, "B"), fe(1, 3, "AA")]).unwrap();
        assert_eq!(out2, out);
    }

    #[test]
    fn splice_edits_rejects_overlap() {
        let src = b"0123456789";
        let err = splice_edits(src, &[fe(1, 5, "x"), fe(4, 8, "y")]).unwrap_err();
        assert!(matches!(err, EditError::Overlap { .. }), "{err}");
        // Touching ranges (end == start) are NOT overlapping.
        assert!(splice_edits(src, &[fe(1, 4, "x"), fe(4, 8, "y")]).is_ok());
    }

    #[test]
    fn splice_edits_rejects_out_of_bounds() {
        let err = splice_edits(b"abc", &[fe(2, 9, "x")]).unwrap_err();
        assert!(matches!(err, EditError::OutOfBounds { .. }), "{err}");
    }

    #[test]
    fn splice_edits_empty_copies() {
        assert_eq!(splice_edits(b"abc", &[]).unwrap(), b"abc");
    }

    #[test]
    fn point_at_counts_rows_and_cols() {
        let src = b"ab\ncd\n";
        assert_eq!(point_at(src, 0), Point { row: 0, col: 0 });
        assert_eq!(point_at(src, 2), Point { row: 0, col: 2 });
        assert_eq!(point_at(src, 3), Point { row: 1, col: 0 });
        assert_eq!(point_at(src, 5), Point { row: 1, col: 2 });
        assert_eq!(point_at(src, 999), Point { row: 2, col: 0 });
    }

    #[test]
    fn reparse_incremental_equals_full() {
        let a = Adapter::new();
        let old = b"package main\n\nfunc f() {}\n";
        let mut old_tree = a.parse(Lang::Go, old).unwrap();
        let edits = [fe(19, 20, "gg")]; // f -> gg
        let (bytes, tree) = reparse(&a, Lang::Go, old, Some(&mut old_tree), &edits).unwrap();
        assert_eq!(bytes, b"package main\n\nfunc gg() {}\n");
        let full = a.parse(Lang::Go, &bytes).unwrap();
        assert_eq!(tree.root, full.root);
    }

    #[test]
    fn file_edit_serializes_with_go_field_names() {
        let j = serde_json::to_value(fe(1, 2, "x")).unwrap();
        assert_eq!(j["Path"], "f");
        assert_eq!(j["StartByte"], 1);
        assert_eq!(j["EndByte"], 2);
        assert_eq!(j["NewText"], "x");
    }
}
