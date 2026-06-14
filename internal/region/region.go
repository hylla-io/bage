// Package region defines Båge's region-anchoring primitives: the content-anchored
// edit unit (SPEC §8) and the concurrency-safe resolver that maps a possibly-stale
// region back onto the live file (ADR-0003).
//
// A region is addressed by a byte range plus a region_hash — the xxHash %016x of
// the region's NORMALIZED bytes (HYLLA_NODE_CONTRACT.md §4, byte-identical with
// Hylla so a whitespace-only reformat does not false-conflict). The hash does one
// job here: it verifies that the bytes at a candidate location are the block the
// edit targets. Relocation (a benign concurrent shift moved the region) is the CST's
// job: when the in-place hash no longer matches, the resolver reparses the live file
// and matches the region_hash against every node. Disambiguation of identical-content
// twins is NOT attempted — two matches are reported as Ambiguous and rejected rather
// than guessed. Båge over-rejects on purpose: corruption is never acceptable, a
// rejected edit is.
//
// This package mirrors Hylla's per-node locator bundle minus graph identity (no
// parent_id / tail_symbol — Båge is ID-blind, SPEC §1.4); region_hash is the only
// seam, so file-mode and graph-mode resolve identically.
package region

import (
	"context"
	"fmt"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/parser"
)

// Region is a content-anchored locator into a file: a byte range, the
// corresponding line/col range, and the region_hash that anchors it by content
// (SPEC §8.1). It mirrors Hylla's per-node locator bundle minus graph identity.
//
// Byte offsets are authoritative; line/col are derived conveniences. When
// StartByte is the LineSentinel value the region is line-addressed and the byte
// range must be resolved from the line range via a LineIndex before use.
type Region struct {
	// Path is the file the region lives in.
	Path string
	// StartByte is the inclusive starting byte offset, or LineSentinel to mark
	// the region as line-addressed (resolve via StartLine/EndLine).
	StartByte int
	// EndByte is the exclusive ending byte offset.
	EndByte int
	// StartLine is the 1-based starting line.
	StartLine int
	// EndLine is the 1-based ending line.
	EndLine int
	// StartCol is the 0-based starting byte column within StartLine.
	StartCol int
	// EndCol is the 0-based ending byte column within EndLine.
	EndCol int
	// RegionHash is the xxHash %016x of the region's NORMALIZED bytes (matches
	// HYLLA_NODE_CONTRACT §4 so it is byte-identical with Hylla's), or "" when the
	// region carries no anchor (single-model file mode), in which case the given
	// byte range is treated as authoritative.
	RegionHash string
}

// LineSentinel is the StartByte value marking a Region as line-addressed: the
// byte range is unknown and must be derived from StartLine/EndLine via a
// LineIndex (ResolveLines) before the region can be used.
const LineSentinel = -1

// FileAnchor is the per-file drift gate (SPEC §8.1, HYLLA_NODE_CONTRACT.md §2):
// RawHash gates byte-offset validity; NormHash classifies whitespace-only drift.
type FileAnchor struct {
	// Path is the file the anchor describes.
	Path string
	// RawHash is the xxHash %016x of the file's RAW bytes — the byte-offset gate.
	RawHash string
	// NormHash is the xxHash %016x of the file's normalized bytes — the drift
	// classifier.
	NormHash string
}

// Edit is a region-anchored edit: replace the bytes of Region with NewText
// (SPEC §8.1). The model echoes a shown RegionHash; it never computes a hash or
// resends old text.
type Edit struct {
	// Region is the content-anchored target of the edit.
	Region Region
	// NewText is the replacement text for the region's bytes.
	NewText string
}

// EditResult is the write-back contract to Hylla (SPEC §8.2): the changed byte
// range plus the recomputed region/file hashes and the new line range, so Hylla
// re-ingests only the changed region.
type EditResult struct {
	// Path is the file that was edited.
	Path string `json:"path" toon:"path"`
	// ChangedStart is the inclusive starting byte offset of the changed range.
	ChangedStart int `json:"changed_start" toon:"changed_start"`
	// ChangedEnd is the exclusive ending byte offset of the changed range.
	ChangedEnd int `json:"changed_end" toon:"changed_end"`
	// NewRegionHash is the region_hash of the post-edit region bytes.
	NewRegionHash string `json:"new_region_hash" toon:"new_region_hash"`
	// NewFileRawHash is the post-edit file RawHash.
	NewFileRawHash string `json:"new_file_raw_hash" toon:"new_file_raw_hash"`
	// NewFileNormHash is the post-edit file NormHash.
	NewFileNormHash string `json:"new_file_norm_hash" toon:"new_file_norm_hash"`
	// NewStartLine is the 1-based starting line of the post-edit region.
	NewStartLine int `json:"new_start_line" toon:"new_start_line"`
	// NewEndLine is the 1-based ending line of the post-edit region.
	NewEndLine int `json:"new_end_line" toon:"new_end_line"`
}

// hasher is the canonical content hasher shared with Hylla (xxHash %016x). It
// holds no state, so a package-level value is safe for concurrent use.
var hasher hashing.Hasher = hashing.XXHasher{}

// HashRegion returns the region_hash — the xxHash %016x of the NORMALIZED bytes of
// src[start:end] (HYLLA_NODE_CONTRACT §4), so it is byte-identical with Hylla's
// region_hash and a whitespace-only reformat of the block does not change it. It
// panics if the range is out of bounds, which signals a caller bug, not drift.
func HashRegion(src []byte, start, end int) string {
	return hashing.NormHash(hasher, src[start:end])
}

// ResolveStatus reports how Resolve located a region against the live file.
type ResolveStatus int

const (
	// Exact means the region_hash matched the bytes at the region's own offset;
	// the range is used as-is.
	Exact ResolveStatus = iota
	// Shifted means a concurrent edit moved the region but not its content: the
	// region_hash matched exactly one node at a new offset (a benign shift).
	Shifted
	// Conflict means no live node matched the region_hash: the region's own
	// content changed — a hard reject.
	Conflict
	// Ambiguous means more than one live node matched the region_hash: identical
	// twins that this package refuses to guess between — a hard reject.
	Ambiguous
)

// String returns the lowercase name of the status.
func (s ResolveStatus) String() string {
	switch s {
	case Exact:
		return "exact"
	case Shifted:
		return "shifted"
	case Conflict:
		return "conflict"
	case Ambiguous:
		return "ambiguous"
	default:
		return "unknown"
	}
}

// Resolve maps r onto the live file bytes, returning the byte range to edit and
// how it was located (ADR-0003, the concurrency core). It is the resolve-under-lock
// step: callers hold the per-file lock and pass the current file bytes, so every
// edit sees prior concurrent commits.
//
// Resolution:
//   - If r.RegionHash == "" (no anchor, single-model file mode) the given byte
//     range is authoritative and returned as Exact.
//   - If HashRegion(live, r.StartByte, r.EndByte) == r.RegionHash the region is in
//     place ⇒ Exact, used as-is.
//   - Otherwise live is parsed under lang and every CST node whose region_hash
//     equals r.RegionHash is collected: exactly one ⇒ Shifted (benign), its range
//     is returned; zero ⇒ Conflict; more than one ⇒ Ambiguous. Conflict and
//     Ambiguous return an error — Resolve never guesses and never misapplies.
//
// A nil or empty-range region with a non-empty hash, or an in-place range that is
// out of bounds, falls through to the parse-and-match path rather than panicking,
// so a stale offset can never crash the resolver.
func Resolve(p parser.ParserPort, lang parser.Lang, live []byte, r Region) (start, end int, status ResolveStatus, err error) {
	// Single-model file mode: no anchor, the range is authoritative.
	if r.RegionHash == "" {
		return r.StartByte, r.EndByte, Exact, nil
	}

	// Fast path: the region is in place if its bytes still hash to RegionHash.
	if inBounds(len(live), r.StartByte, r.EndByte) &&
		HashRegion(live, r.StartByte, r.EndByte) == r.RegionHash {
		return r.StartByte, r.EndByte, Exact, nil
	}

	// Slow path: parse the live file and match the region_hash against the CST.
	tree, perr := p.Parse(context.Background(), lang, live)
	if perr != nil {
		return 0, 0, Conflict, fmt.Errorf("region: parse live file %q: %w", r.Path, perr)
	}
	defer tree.Close()

	matches := matchNodes(tree.Root, live, r.RegionHash)
	switch len(matches) {
	case 1:
		return matches[0].Start, matches[0].End, Shifted, nil
	case 0:
		return 0, 0, Conflict, fmt.Errorf("region: %q region_hash %s no longer matches any node (conflict)", r.Path, r.RegionHash)
	default:
		return 0, 0, Ambiguous, fmt.Errorf("region: %q region_hash %s matches %d nodes (ambiguous); refusing to guess", r.Path, r.RegionHash, len(matches))
	}
}

// inBounds reports whether [start:end) is a valid half-open range within n bytes.
func inBounds(n, start, end int) bool {
	return start >= 0 && end >= start && end <= n
}

// matchNodes walks the CST and collects the byte range of every node whose RAW
// bytes hash to want. Each distinct byte range is reported once: a node and an
// only-child commonly share a span, and counting that span twice would falsely
// report Ambiguous, so spans are de-duplicated.
func matchNodes(root *parser.Node, src []byte, want string) []parser.ByteRange {
	var matches []parser.ByteRange
	seen := make(map[parser.ByteRange]struct{})
	var walk func(n *parser.Node)
	walk = func(n *parser.Node) {
		if n == nil {
			return
		}
		if inBounds(len(src), n.StartByte, n.EndByte) &&
			HashRegion(src, n.StartByte, n.EndByte) == want {
			br := parser.ByteRange{Start: n.StartByte, End: n.EndByte}
			if _, dup := seen[br]; !dup {
				seen[br] = struct{}{}
				matches = append(matches, br)
			}
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(root)
	return matches
}
