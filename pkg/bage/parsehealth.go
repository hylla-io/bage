package bage

import "github.com/hylla-io/bage/internal/region"

// ParseDefect is one syntax problem surfaced by parse-health: an ERROR-kind node
// (a span the grammar could not incorporate) or a MISSING node (a zero-width node
// the parser inserted to recover, e.g. an absent closing brace). It is the cheap,
// LSP-free tier of `bage diagnose` (SPEC §10.5) and uses the SAME ERROR/MISSING
// signal the edit parse-floor relies on. Line/Col are 1-based; the byte range is
// the half-open [StartByte, EndByte) span of the offending node.
type ParseDefect struct {
	// Kind is "ERROR" for an error-kind node or "MISSING" for an inserted
	// recovery node.
	Kind string
	// Line is the 1-based line of StartByte.
	Line int
	// Col is the 1-based column (byte offset within the line, +1) of StartByte.
	Col int
	// StartByte is the inclusive start byte offset of the offending node.
	StartByte int
	// EndByte is the exclusive end byte offset of the offending node.
	EndByte int
}

// ParseHealth walks a parsed file and reports every ERROR-kind or MISSING node as
// a ParseDefect with 1-based line/col and byte range. A clean parse reports none.
//
// The grammar-free text fallback (LangText / nil native tree) ALWAYS parses
// losslessly — every byte lands in a line node — so it can never produce a defect
// and ParseHealth returns an empty slice for it without walking. This mirrors the
// edit parse-floor: the same ERROR/MISSING signal gates an edit and is what
// diagnose surfaces (SPEC §10.5). It never returns nil for a non-nil tree with no
// defects; callers can rely on a non-nil (possibly empty) result.
func ParseHealth(opened *OpenedFile) []ParseDefect {
	out := make([]ParseDefect, 0)
	if opened == nil || opened.Tree == nil || opened.Tree.Root == nil {
		return out
	}
	// The text fallback (nil native handle) is byte-for-byte lossless and has no
	// concept of a syntax error, so it is reported clean without a walk.
	if opened.Tree.Native == nil {
		return out
	}
	li := region.NewLineIndex(opened.Tree.Source)
	collectDefects(opened.Tree.Root, li, &out)
	return out
}

// collectDefects appends a ParseDefect for n and each descendant whose kind is
// "ERROR" or that is MISSING, in source (pre-order) order. It always recurses so a
// defect nested inside an otherwise-valid declaration is still surfaced. 1-based
// line/col come from the LineIndex (PositionForByte returns a 1-based line and a
// 0-based byte column, so the column is +1'd to be 1-based).
func collectDefects(n *Node, li region.LineIndex, out *[]ParseDefect) {
	if n == nil {
		return
	}
	kind := ""
	switch {
	case n.Kind == "ERROR":
		kind = "ERROR"
	case n.Missing:
		kind = "MISSING"
	}
	if kind != "" {
		line, col := li.PositionForByte(n.StartByte)
		*out = append(*out, ParseDefect{
			Kind:      kind,
			Line:      line,
			Col:       col + 1,
			StartByte: n.StartByte,
			EndByte:   n.EndByte,
		})
	}
	for _, c := range n.Children {
		collectDefects(c, li, out)
	}
}
