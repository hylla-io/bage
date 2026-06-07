package render

import (
	"encoding/json"
	"fmt"
	"io"
)

// Renderer writes a command result value to w in a single output format.
// Implementations are selected by Format and are safe to use as zero values.
type Renderer interface {
	// Render writes v to w in the renderer's format.
	Render(w io.Writer, v any) error
}

// TextRenderable is implemented by result types that know how to render
// themselves as human-readable text. The text renderer type-asserts values to
// this interface, so a domain type opts in simply by defining RenderText —
// without importing pkg/render and thus without an import cycle.
type TextRenderable interface {
	// RenderText writes the receiver's human-readable representation to w.
	RenderText(w io.Writer) error
}

// jsonRenderer renders values as indented JSON.
type jsonRenderer struct{}

// Render writes v as JSON indented with two spaces, followed by a trailing
// newline. The output is byte-identical to cmd/bage's printShowJSON.
func (jsonRenderer) Render(w io.Writer, v any) error {
	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return fmt.Errorf("render: marshal json: %w", err)
	}
	_, err = fmt.Fprintln(w, string(b))
	return err
}

// textRenderer renders values that implement TextRenderable as human-readable
// text.
type textRenderer struct{}

// Render delegates to v's RenderText method when v implements TextRenderable,
// otherwise it returns an error identifying the offending type.
func (textRenderer) Render(w io.Writer, v any) error {
	if tr, ok := v.(TextRenderable); ok {
		return tr.RenderText(w)
	}
	return fmt.Errorf("render: %T is not text-renderable", v)
}

// toonRenderer renders values as a TOON (token-oriented object notation)
// document via MarshalTOON.
type toonRenderer struct{}

// Render writes v as TOON, delegating encoding to MarshalTOON.
func (toonRenderer) Render(w io.Writer, v any) error {
	b, err := MarshalTOON(v)
	if err != nil {
		return err
	}
	_, err = w.Write(b)
	return err
}

// Emit writes v to w in the given Format. FormatJSON marshals indented JSON,
// FormatText delegates to v's RenderText method (v must implement
// TextRenderable), and FormatTOON marshals a TOON document via MarshalTOON. An
// unknown Format is reported as a usage error.
func Emit(w io.Writer, f Format, v any) error {
	switch f {
	case FormatJSON:
		return jsonRenderer{}.Render(w, v)
	case FormatText:
		return textRenderer{}.Render(w, v)
	case FormatTOON:
		return toonRenderer{}.Render(w, v)
	default:
		return fmt.Errorf("render: unknown format %q", f)
	}
}
