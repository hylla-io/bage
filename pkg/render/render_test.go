package render

import (
	"bytes"
	"encoding/json"
	"io"
	"testing"
)

// TestEmitJSON checks that Emit with FormatJSON produces byte-identical output to
// json.MarshalIndent(v, "", "  ") followed by a trailing newline, matching
// printShowJSON in cmd/bage/show.go.
func TestEmitJSON(t *testing.T) {
	type payload struct {
		Name  string `json:"name"`
		Count int    `json:"count"`
	}
	v := payload{Name: "alpha", Count: 3}

	var buf bytes.Buffer
	if err := Emit(&buf, FormatJSON, v); err != nil {
		t.Fatalf("Emit FormatJSON: unexpected error: %v", err)
	}

	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		t.Fatalf("MarshalIndent: %v", err)
	}
	want := string(b) + "\n"

	if got := buf.String(); got != want {
		t.Errorf("Emit FormatJSON = %q, want %q", got, want)
	}
}

// fakeText is a TextRenderable test double whose RenderText writes a fixed marker.
type fakeText struct{}

// RenderText writes "X" to w, satisfying the TextRenderable contract.
func (fakeText) RenderText(w io.Writer) error {
	_, err := io.WriteString(w, "X")
	return err
}

// TestEmitText checks that Emit with FormatText delegates to a value's
// RenderText method.
func TestEmitText(t *testing.T) {
	var buf bytes.Buffer
	if err := Emit(&buf, FormatText, fakeText{}); err != nil {
		t.Fatalf("Emit FormatText: unexpected error: %v", err)
	}
	if got := buf.String(); got != "X" {
		t.Errorf("Emit FormatText = %q, want %q", got, "X")
	}
}

// TestEmitTextNonRenderable checks that Emit with FormatText returns a non-nil
// error when the value does not implement TextRenderable.
func TestEmitTextNonRenderable(t *testing.T) {
	type plain struct{ A int }

	var buf bytes.Buffer
	err := Emit(&buf, FormatText, plain{A: 1})
	if err == nil {
		t.Fatalf("Emit FormatText on non-renderable: expected error, got nil")
	}
}

// TestEmitTOON for FormatTOON lives in toon_test.go and exercises the real
// MarshalTOON renderer that replaced the S7 stub.
