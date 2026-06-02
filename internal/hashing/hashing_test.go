package hashing

import "testing"

// TestFNVHasherSumDeterministic verifies the stdlib adapter is deterministic
// and produces distinct digests for distinct inputs.
func TestFNVHasherSumDeterministic(t *testing.T) {
	t.Parallel()
	var h FNVHasher
	tests := []struct {
		name string
		in   []byte
	}{
		{"empty", []byte{}},
		{"nil", nil},
		{"ascii", []byte("hello world")},
		{"with newlines", []byte("a\nb\nc\n")},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			got1 := h.Sum(tt.in)
			got2 := h.Sum(tt.in)
			if got1 != got2 {
				t.Fatalf("Sum not deterministic: %q vs %q", got1, got2)
			}
			if got1 == "" {
				t.Fatalf("Sum returned empty digest")
			}
		})
	}

	// Distinct inputs must yield distinct digests.
	if h.Sum([]byte("a")) == h.Sum([]byte("b")) {
		t.Fatalf("distinct inputs produced identical digest")
	}
	// Empty and nil are equivalent byte sequences.
	if h.Sum(nil) != h.Sum([]byte{}) {
		t.Fatalf("nil and empty slice produced different digests")
	}
}

// TestRawHashVsNormHash checks the two-hash drift semantics: RawHash tracks raw
// bytes, NormHash tracks normalized content.
func TestRawHashVsNormHash(t *testing.T) {
	t.Parallel()
	var h FNVHasher
	tests := []struct {
		name      string
		raw       []byte
		wantEqual bool // RawHash(raw) == NormHash(raw)?
	}{
		{"already normalized", []byte("a\nb\n"), true},
		{"empty", []byte{}, true},
		{"trailing ws before newline", []byte("a   \nb\n"), false},
		{"trailing ws at eof", []byte("a\nb   "), false},
		{"crlf line endings", []byte("a\r\nb\r\n"), false},
		{"leading bom", []byte{0xEF, 0xBB, 0xBF, 'a', '\n'}, false},
		{"interior ws preserved", []byte("a  b\n"), true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			raw := RawHash(h, tt.raw)
			norm := NormHash(h, tt.raw)
			if (raw == norm) != tt.wantEqual {
				t.Fatalf("RawHash==NormHash got %v want %v (raw=%s norm=%s)",
					raw == norm, tt.wantEqual, raw, norm)
			}
		})
	}
}

// TestWhitespaceOnlyChangeDrift confirms that a whitespace-only edit changes
// RawHash but leaves NormHash stable — the core whitespace-drift classifier.
func TestWhitespaceOnlyChangeDrift(t *testing.T) {
	t.Parallel()
	var h FNVHasher
	clean := []byte("func main() {\n\treturn\n}\n")
	dirty := []byte("func main() {  \n\treturn\t\n}\n") // trailing ws added

	if RawHash(h, clean) == RawHash(h, dirty) {
		t.Fatalf("RawHash should differ after whitespace-only change")
	}
	if NormHash(h, clean) != NormHash(h, dirty) {
		t.Fatalf("NormHash should be stable across whitespace-only change")
	}
}

// TestRawHashSemanticChange confirms a real content change moves NormHash too.
func TestRawHashSemanticChange(t *testing.T) {
	t.Parallel()
	var h FNVHasher
	a := []byte("return 1\n")
	b := []byte("return 2\n")
	if NormHash(h, a) == NormHash(h, b) {
		t.Fatalf("NormHash should differ on a semantic change")
	}
}
