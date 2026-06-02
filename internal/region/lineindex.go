package region

// LineIndex maps between 1-based line numbers and byte offsets in a fixed source
// buffer. It records the byte offset at which each line begins, so line/col
// lookups and the line-addressed → byte-range resolution (SPEC §8.1, model-facing
// line addressing / byte-internal) are O(log n) or O(1).
//
// Columns are 0-based BYTE offsets within a line — the UTF-8 byte-col convention
// of HYLLA_NODE_CONTRACT.md §1 (LSP converts to UTF-16 at its own boundary). Line
// endings are located by '\n'; a preceding '\r' (CRLF) stays part of the line's
// bytes, so byte offsets remain faithful to the raw file.
type LineIndex struct {
	// lineStarts[i] is the byte offset where line i+1 begins. lineStarts[0] is
	// always 0; a trailing entry past the final '\n' represents the (possibly
	// empty) last line.
	lineStarts []int
	// size is len(src), the clamp bound for byte lookups.
	size int
}

// NewLineIndex builds a LineIndex over src. The source is not retained; only line
// offsets and the length are kept.
func NewLineIndex(src []byte) LineIndex {
	starts := []int{0}
	for i, b := range src {
		if b == '\n' {
			starts = append(starts, i+1)
		}
	}
	return LineIndex{lineStarts: starts, size: len(src)}
}

// Lines returns the number of lines. A buffer with a trailing '\n' counts the
// empty final line, matching the lineStarts layout.
func (li LineIndex) Lines() int {
	return len(li.lineStarts)
}

// ByteForLine returns the byte offset where the 1-based line begins. Lines below
// 1 clamp to the first line's offset (0); lines past the last clamp to the buffer
// size, so out-of-range line numbers never index out of bounds.
func (li LineIndex) ByteForLine(line int) int {
	if line < 1 {
		return 0
	}
	if line > len(li.lineStarts) {
		return li.size
	}
	return li.lineStarts[line-1]
}

// LineForByte returns the 1-based line containing the byte offset. Offsets below 0
// clamp to line 1; offsets at or past the buffer size resolve to the last line.
// The boundary rule is half-open: the byte immediately after a '\n' belongs to the
// next line.
func (li LineIndex) LineForByte(off int) int {
	if off <= 0 {
		return 1
	}
	if off >= li.size {
		off = li.size
	}
	// Binary search for the greatest line start <= off.
	lo, hi := 0, len(li.lineStarts)-1
	for lo < hi {
		mid := (lo + hi + 1) / 2
		if li.lineStarts[mid] <= off {
			lo = mid
		} else {
			hi = mid - 1
		}
	}
	return lo + 1
}

// PositionForByte returns the 1-based line and 0-based byte column of off. The
// column is off minus the containing line's start offset.
func (li LineIndex) PositionForByte(off int) (line, col int) {
	line = li.LineForByte(off)
	if off < 0 {
		off = 0
	} else if off > li.size {
		off = li.size
	}
	return line, off - li.lineStarts[line-1]
}

// FillLineCols populates r.StartLine/EndLine and r.StartCol/EndCol from
// r.StartByte/EndByte (1-based lines, 0-based byte cols) and returns the updated
// Region. The end position is the point AT EndByte (the exclusive boundary),
// matching tree-sitter EndPoint semantics. It is a no-op-safe convenience for
// turning a byte-resolved region into a fully-populated locator bundle.
func (li LineIndex) FillLineCols(r Region) Region {
	r.StartLine, r.StartCol = li.PositionForByte(r.StartByte)
	r.EndLine, r.EndCol = li.PositionForByte(r.EndByte)
	return r
}

// ResolveLines turns a line-addressed Region (StartByte == LineSentinel) into a
// byte-range Region: StartByte becomes the start of StartLine and EndByte the
// start of the line AFTER EndLine (so the range covers EndLine in full,
// including its newline). The line/col fields are then refreshed from the
// resolved bytes. A Region that is already byte-addressed is returned unchanged.
//
// Lines clamp via ByteForLine, so an over-large EndLine resolves to end-of-buffer
// rather than erroring; the caller's region_hash is the integrity check.
func (li LineIndex) ResolveLines(r Region) Region {
	if r.StartByte != LineSentinel {
		return r
	}
	r.StartByte = li.ByteForLine(r.StartLine)
	r.EndByte = li.ByteForLine(r.EndLine + 1)
	return li.FillLineCols(r)
}
