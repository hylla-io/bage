// Package render defines output formats for bage command results and the helpers
// to parse them from user-facing flags.
package render

import "fmt"

// Format identifies how a command renders its output. Its zero value is not a
// valid format; use ParseFormat to obtain a Format from a flag value, which
// resolves the empty string to the default FormatText.
type Format string

const (
	// FormatText renders human-readable plain text. It is the default format
	// when the --format flag is empty.
	FormatText Format = "text"
	// FormatJSON renders machine-readable JSON.
	FormatJSON Format = "json"
	// FormatTOON renders TOON (token-oriented object notation) output.
	FormatTOON Format = "toon"
)

// ParseFormat maps the --format flag to a Format. An EMPTY string resolves to
// FormatText, the default. A non-empty value must match a known format's
// canonical name ("text", "json", or "toon"); anything else is an explicit
// usage error rather than a silent fallthrough.
func ParseFormat(s string) (Format, error) {
	switch s {
	case "", "text":
		return FormatText, nil
	case "json":
		return FormatJSON, nil
	case "toon":
		return FormatTOON, nil
	default:
		return "", fmt.Errorf("bage: unknown --format %q (want text|json|toon)", s)
	}
}
