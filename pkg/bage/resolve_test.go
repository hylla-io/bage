package bage

import "testing"

func TestResolveRange(t *testing.T) {
	// A 3-line buffer; line 2 ("bbb\n") spans bytes 4..8, trailing newline at 7.
	src := []byte("aaa\nbbb\nccc\n")

	tests := []struct {
		name      string
		line      int
		lines     string
		start     int
		end       int
		wantStart int
		wantEnd   int
		wantErr   bool
	}{
		{
			name:      "line mode trims trailing newline",
			line:      2,
			lines:     "",
			start:     -1,
			end:       -1,
			wantStart: 4,
			wantEnd:   7,
		},
		{
			name:      "lines range mode",
			line:      -1,
			lines:     "1-2",
			start:     -1,
			end:       -1,
			wantStart: 0,
			wantEnd:   7,
		},
		{
			name:      "byte mode",
			line:      -1,
			lines:     "",
			start:     2,
			end:       5,
			wantStart: 2,
			wantEnd:   5,
		},
		{
			name:    "more than one mode is an error",
			line:    2,
			lines:   "",
			start:   2,
			end:     5,
			wantErr: true,
		},
		{
			name:    "no mode is an error",
			line:    -1,
			lines:   "",
			start:   -1,
			end:     -1,
			wantErr: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			reg, err := ResolveRange(src, tt.line, tt.lines, tt.start, tt.end)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("ResolveRange(%d, %q, %d, %d) = nil error, want error",
						tt.line, tt.lines, tt.start, tt.end)
				}
				return
			}
			if err != nil {
				t.Fatalf("ResolveRange(%d, %q, %d, %d) unexpected error: %v",
					tt.line, tt.lines, tt.start, tt.end, err)
			}
			if reg.StartByte != tt.wantStart || reg.EndByte != tt.wantEnd {
				t.Fatalf("ResolveRange(%d, %q, %d, %d) = [%d,%d), want [%d,%d)",
					tt.line, tt.lines, tt.start, tt.end,
					reg.StartByte, reg.EndByte, tt.wantStart, tt.wantEnd)
			}
		})
	}
}
