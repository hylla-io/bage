package main

import (
	"bytes"
	"testing"

	"github.com/hylla-io/bage/internal/region"
)

// TestEditResultsRenderTextMatchesPrintResults asserts editResults.RenderText
// is byte-identical to the legacy printResults output, so routing apply through
// render.Emit(FormatText) preserves the existing text output exactly. It uses an
// unsorted, multi-path input to also exercise the same path/offset sort.
func TestEditResultsRenderTextMatchesPrintResults(t *testing.T) {
	results := []region.EditResult{
		{
			Path: "b.go", ChangedStart: 5, ChangedEnd: 9,
			NewRegionHash: "00000000000000cc", NewFileRawHash: "00000000000000dd", NewFileNormHash: "00000000000000ee",
			NewStartLine: 2, NewEndLine: 2,
		},
		{
			Path: "a.go", ChangedStart: 20, ChangedEnd: 24,
			NewRegionHash: "0000000000000011", NewFileRawHash: "0000000000000022", NewFileNormHash: "0000000000000033",
			NewStartLine: 4, NewEndLine: 5,
		},
		{
			Path: "a.go", ChangedStart: 0, ChangedEnd: 3,
			NewRegionHash: "0000000000000044", NewFileRawHash: "0000000000000055", NewFileNormHash: "0000000000000066",
			NewStartLine: 1, NewEndLine: 1,
		},
	}

	// Legacy reference output. printResults sorts its argument in place, so pass a
	// copy to keep the RenderText input order independent.
	legacyIn := append([]region.EditResult(nil), results...)
	var legacy bytes.Buffer
	printResults(&legacy, legacyIn)

	renderIn := append(editResults(nil), results...)
	var rendered bytes.Buffer
	if err := renderIn.RenderText(&rendered); err != nil {
		t.Fatalf("RenderText: %v", err)
	}

	if rendered.String() != legacy.String() {
		t.Fatalf("RenderText output differs from printResults:\nrender:\n%s\nlegacy:\n%s", rendered.String(), legacy.String())
	}
}
