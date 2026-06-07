package render

import (
	"bytes"
	"strings"
	"testing"

	toon "github.com/toon-format/toon-go"
)

// toonRow is a flat struct whose fields carry toon struct tags so that
// MarshalTOON renders a slice of these as a tabular array with a single header
// row naming each field once.
type toonRow struct {
	Name string `toon:"name"`
	Age  int    `toon:"age"`
}

// TestEmitTOON verifies that Emit with FormatTOON renders a slice of flat
// structs as a non-empty tabular document: each field name appears once in the
// header and both rows' scalar values are present in the output.
func TestEmitTOON(t *testing.T) {
	rows := []toonRow{
		{Name: "alice", Age: 30},
		{Name: "bob", Age: 25},
	}

	var buf bytes.Buffer
	if err := Emit(&buf, FormatTOON, rows); err != nil {
		t.Fatalf("Emit(FormatTOON) returned error: %v", err)
	}

	out := buf.String()
	if out == "" {
		t.Fatal("Emit(FormatTOON) produced empty output")
	}

	// Each field is named exactly once in the tabular header.
	if got := strings.Count(out, "name"); got != 1 {
		t.Errorf("field %q appears %d times, want 1 (tabular header)\noutput:\n%s", "name", got, out)
	}
	if got := strings.Count(out, "age"); got != 1 {
		t.Errorf("field %q appears %d times, want 1 (tabular header)\noutput:\n%s", "age", got, out)
	}

	// Both rows' scalar values are present.
	for _, want := range []string{"alice", "bob", "30", "25"} {
		if !strings.Contains(out, want) {
			t.Errorf("output missing row value %q\noutput:\n%s", want, out)
		}
	}

	// Oracle: Emit's TOON path matches MarshalTOON, which delegates to toon-go.
	oracle, err := toon.MarshalString(rows, toon.WithArrayDelimiter(toon.DelimiterComma))
	if err != nil {
		t.Fatalf("oracle MarshalString returned error: %v", err)
	}
	if out != oracle {
		t.Errorf("Emit output does not match toon-go oracle\n got:\n%s\nwant:\n%s", out, oracle)
	}
}
