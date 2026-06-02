// Package edit provides the in-memory round-trip edit primitives Båge applies
// before any file write: drift classification (two hashes), a pure byte-range
// splice over a single file's bytes, a byte-offset → row/col point helper, and
// a reparse that prefers incremental parsing for the single-region path.
//
// Nothing here performs file I/O. Reading the live file, deciding to apply, and
// writing the result are the session/locator layers' responsibility (SPEC §5,
// §6). This package is the pure core those layers compose.
package edit

import (
	"context"
	"fmt"
	"sort"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/locator"
	"github.com/hylla-io/bage/internal/parser"
)

// DriftStatus classifies how a live file's bytes have drifted from the bytes a
// byte range was grounded against (SPEC §5).
type DriftStatus int

const (
	// DriftValid means the live raw bytes hash-match the expected raw hash, so
	// the byte range is trustworthy and the edit may apply directly.
	DriftValid DriftStatus = iota
	// DriftWhitespaceOnly means the raw hash differs but the normalized hash
	// matches: the change is whitespace-only and the range must be re-grounded
	// (re-resolved) before applying, never slid blindly.
	DriftWhitespaceOnly
	// DriftReal means the normalized hash differs: the content itself changed,
	// so the range must be re-grounded from Hylla or the edit rejected.
	DriftReal
)

// String returns a stable lowercase label for the drift status.
func (s DriftStatus) String() string {
	switch s {
	case DriftValid:
		return "valid"
	case DriftWhitespaceOnly:
		return "whitespace-only"
	case DriftReal:
		return "real"
	default:
		return "unknown"
	}
}

// CheckDrift classifies live against the hashes the caller's byte range was
// grounded on. A raw match yields DriftValid; a raw mismatch with a normalized
// match yields DriftWhitespaceOnly; a normalized mismatch yields DriftReal. The
// same Hasher must have produced expectedRaw and expectedNorm.
func CheckDrift(h hashing.Hasher, live []byte, expectedRaw, expectedNorm string) DriftStatus {
	if hashing.RawHash(h, live) == expectedRaw {
		return DriftValid
	}
	if hashing.NormHash(h, live) == expectedNorm {
		return DriftWhitespaceOnly
	}
	return DriftReal
}

// SpliceEdits applies every edit to one file's bytes purely, returning a fresh
// slice. Edits are reverse-sorted by StartByte (descending) so splicing an
// earlier-offset edit never invalidates a later-offset edit's range. Overlapping
// ranges are rejected (mirroring locator.ApplyFileEdits) because reverse-sorted
// application is correct only for disjoint ranges. Out-of-range or inverted
// offsets are rejected. SpliceEdits performs no I/O.
func SpliceEdits(src []byte, edits []locator.FileEdit) ([]byte, error) {
	if len(edits) == 0 {
		out := make([]byte, len(src))
		copy(out, src)
		return out, nil
	}

	sorted := make([]locator.FileEdit, len(edits))
	copy(sorted, edits)
	sort.SliceStable(sorted, func(i, j int) bool {
		return sorted[i].StartByte > sorted[j].StartByte
	})

	// sorted is descending by StartByte, so sorted[i+1] has the lower start;
	// they overlap when its EndByte extends past sorted[i].StartByte.
	for i := 0; i+1 < len(sorted); i++ {
		if sorted[i+1].EndByte > sorted[i].StartByte {
			return nil, fmt.Errorf("edit: overlapping edits: [%d:%d] and [%d:%d]",
				sorted[i+1].StartByte, sorted[i+1].EndByte, sorted[i].StartByte, sorted[i].EndByte)
		}
	}

	out := make([]byte, len(src))
	copy(out, src)
	for _, e := range sorted {
		spliced, err := splice(out, e.StartByte, e.EndByte, e.NewText)
		if err != nil {
			return nil, err
		}
		out = spliced
	}
	return out, nil
}

// PointAt returns the zero-based row/column for byteOffset within src: Row is
// the count of '\n' bytes before the offset, and Col is the number of bytes
// since the last '\n' (or since the start of src on the first line). Offsets at
// or past len(src) clamp to EOF. A negative offset clamps to the origin.
func PointAt(src []byte, byteOffset int) parser.Point {
	if byteOffset < 0 {
		byteOffset = 0
	}
	if byteOffset > len(src) {
		byteOffset = len(src)
	}
	row, col := 0, 0
	for i := 0; i < byteOffset; i++ {
		if src[i] == '\n' {
			row++
			col = 0
		} else {
			col++
		}
	}
	return parser.Point{Row: row, Col: col}
}

// Reparse splices edits into old and reparses the result. For a single edit and
// a non-nil oldTree it builds a parser.InputEdit from PointAt over the old and
// new bytes and calls ParseIncremental, reusing oldTree. Otherwise (oldTree nil,
// or a multi-edit batch where a single InputEdit cannot describe the change) it
// calls Parse for a full reparse. It returns the spliced bytes and the new tree;
// the caller owns the returned tree and must Close it.
func Reparse(ctx context.Context, p parser.ParserPort, lang parser.Lang, old []byte, oldTree *parser.Tree, edits []locator.FileEdit) (newBytes []byte, tree *parser.Tree, err error) {
	newBytes, err = SpliceEdits(old, edits)
	if err != nil {
		return nil, nil, fmt.Errorf("edit: reparse splice: %w", err)
	}

	if oldTree == nil || len(edits) != 1 {
		tree, err = p.Parse(ctx, lang, newBytes)
		if err != nil {
			return nil, nil, fmt.Errorf("edit: reparse full: %w", err)
		}
		return newBytes, tree, nil
	}

	e := edits[0]
	newEndByte := e.StartByte + len(e.NewText)
	in := parser.InputEdit{
		StartByte:   e.StartByte,
		OldEndByte:  e.EndByte,
		NewEndByte:  newEndByte,
		StartPoint:  PointAt(old, e.StartByte),
		OldEndPoint: PointAt(old, e.EndByte),
		NewEndPoint: PointAt(newBytes, newEndByte),
	}
	tree, err = p.ParseIncremental(ctx, lang, newBytes, oldTree, in)
	if err != nil {
		return nil, nil, fmt.Errorf("edit: reparse incremental: %w", err)
	}
	return newBytes, tree, nil
}

// splice replaces src[start:end] with newText, guarding against out-of-range or
// inverted offsets. It returns a freshly allocated slice and never mutates src.
func splice(src []byte, start, end int, newText string) ([]byte, error) {
	if start < 0 || end < start || end > len(src) {
		return nil, fmt.Errorf("edit: byte range [%d:%d] out of bounds for length %d", start, end, len(src))
	}
	out := make([]byte, 0, len(src)-(end-start)+len(newText))
	out = append(out, src[:start]...)
	out = append(out, newText...)
	out = append(out, src[end:]...)
	return out, nil
}
