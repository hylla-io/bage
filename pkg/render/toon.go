package render

import (
	"fmt"

	toon "github.com/toon-format/toon-go"
)

// MarshalTOON encodes v as a TOON (token-oriented object notation) document.
// Slices of flat structs render as tabular arrays with a single header row
// naming each field once, using the comma array delimiter. Struct fields opt
// into named output via the `toon:"..."` struct tag.
func MarshalTOON(v any) ([]byte, error) {
	b, err := toon.Marshal(v, toon.WithArrayDelimiter(toon.DelimiterComma))
	if err != nil {
		return nil, fmt.Errorf("render: marshal toon: %w", err)
	}
	return b, nil
}
