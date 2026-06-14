package bage

import (
	"errors"
	"fmt"
	"strconv"
	"strings"

	"github.com/hylla-io/bage/internal/region"
)

// ResolveRange builds a region-anchored target over src from one addressing
// mode. Exactly one mode must be supplied: a single line (line >= 1), a 1-based
// inclusive line range (lines = "L1-L2"), or a raw byte range (start and end
// both >= 0). The unset sentinel for line, start, and end is -1, matching the
// cmd/bage --line/--start/--end flag defaults.
//
// Line addressing is resolved to a concrete byte range against src via a
// region.LineIndex. A resolved line range spans THROUGH the final line's
// trailing newline; that newline is excluded so a replacement preserves line
// structure (a final line with no trailing newline is left as-is). Supplying
// more than one mode, or no mode at all, returns an error.
func ResolveRange(src []byte, line int, lines string, start, end int) (region.Region, error) {
	byteMode := start >= 0 || end >= 0
	lineMode := line >= 0 || lines != ""

	switch {
	case byteMode && lineMode:
		return region.Region{}, errors.New("resolve: choose one of line/lines or start/end, not both")
	case byteMode:
		if start < 0 || end < 0 {
			return region.Region{}, errors.New("resolve: start and end are both required for byte addressing")
		}
		li := region.NewLineIndex(src)
		return li.FillLineCols(region.Region{
			StartByte: start,
			EndByte:   end,
		}), nil
	case lineMode:
		startLine, endLine, err := resolveLineRange(line, lines)
		if err != nil {
			return region.Region{}, err
		}
		li := region.NewLineIndex(src)
		reg := li.ResolveLines(region.Region{
			StartByte: region.LineSentinel,
			StartLine: startLine,
			EndLine:   endLine,
		})
		// A resolved line range spans THROUGH the final line's trailing newline.
		// Exclude that newline so a replacement replaces the line CONTENT and the
		// line structure survives even when the replacement has no trailing
		// newline. A final line with no trailing newline is left as-is.
		if reg.EndByte > reg.StartByte && reg.EndByte <= len(src) && src[reg.EndByte-1] == '\n' {
			reg.EndByte--
			reg = li.FillLineCols(reg)
		}
		return reg, nil
	default:
		return region.Region{}, errors.New("resolve: one of line, lines, or start/end is required")
	}
}

// resolveLineRange resolves the single-line / line-range inputs to a 1-based
// inclusive [startLine, endLine]. line and lines are mutually exclusive; lines
// must be "L1-L2" with L1 <= L2 and both >= 1.
func resolveLineRange(line int, lines string) (startLine, endLine int, err error) {
	if line >= 0 && lines != "" {
		return 0, 0, errors.New("resolve: choose one of line or lines, not both")
	}
	if line >= 0 {
		if line < 1 {
			return 0, 0, errors.New("resolve: line must be >= 1")
		}
		return line, line, nil
	}
	lo, hi, ok := strings.Cut(lines, "-")
	if !ok {
		return 0, 0, fmt.Errorf("resolve: lines %q must be L1-L2", lines)
	}
	startLine, err = strconv.Atoi(strings.TrimSpace(lo))
	if err != nil || startLine < 1 {
		return 0, 0, fmt.Errorf("resolve: lines start %q must be >= 1", lo)
	}
	endLine, err = strconv.Atoi(strings.TrimSpace(hi))
	if err != nil || endLine < 1 {
		return 0, 0, fmt.Errorf("resolve: lines end %q must be >= 1", hi)
	}
	if startLine > endLine {
		return 0, 0, fmt.Errorf("resolve: lines %q has start past end", lines)
	}
	return startLine, endLine, nil
}
