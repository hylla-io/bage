package render

import (
	"strings"
	"testing"
)

func TestParseFormat(t *testing.T) {
	tests := []struct {
		name    string
		in      string
		want    Format
		wantErr bool
	}{
		{name: "text", in: "text", want: FormatText},
		{name: "json", in: "json", want: FormatJSON},
		{name: "toon", in: "toon", want: FormatTOON},
		{name: "empty defaults to text", in: "", want: FormatText},
		{name: "unknown errors", in: "xml", wantErr: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ParseFormat(tt.in)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("ParseFormat(%q) error = nil, want non-nil", tt.in)
				}
				for _, valid := range []string{"text", "json", "toon"} {
					if !strings.Contains(err.Error(), valid) {
						t.Errorf("ParseFormat(%q) error %q does not name valid format %q",
							tt.in, err.Error(), valid)
					}
				}
				return
			}
			if err != nil {
				t.Fatalf("ParseFormat(%q) unexpected error: %v", tt.in, err)
			}
			if got != tt.want {
				t.Errorf("ParseFormat(%q) = %q, want %q", tt.in, got, tt.want)
			}
		})
	}
}
