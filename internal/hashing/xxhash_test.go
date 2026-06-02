package hashing

import "testing"

// TestXXHasherSum verifies the canonical 16-hex format, a known vector, and
// determinism. The empty-input vector pins the xxHash64 seed result so a future
// library change that altered it would be caught.
func TestXXHasherSum(t *testing.T) {
	var h XXHasher

	if got := h.Sum(nil); got != "ef46db3751d8e999" {
		t.Fatalf("Sum(nil) = %q, want xxHash64 empty vector ef46db3751d8e999", got)
	}
	if got := h.Sum([]byte{}); got != "ef46db3751d8e999" {
		t.Fatalf("Sum(empty) = %q, want same as nil", got)
	}
	if got := h.Sum([]byte("hello")); len(got) != 16 {
		t.Fatalf("Sum(%q) = %q, want 16 hex chars (got %d)", "hello", got, len(got))
	}
	if a, b := h.Sum([]byte("abc")), h.Sum([]byte("abc")); a != b {
		t.Fatalf("Sum not deterministic: %q vs %q", a, b)
	}
	if a, b := h.Sum([]byte("abc")), h.Sum([]byte("abd")); a == b {
		t.Fatalf("Sum collided on distinct inputs: %q", a)
	}
}

// TestXXHasherDriftSemantics verifies XXHasher satisfies Hasher and drives the
// two-hash drift rule correctly: whitespace-only change moves RawHash but not
// NormHash; a semantic change moves both.
func TestXXHasherDriftSemantics(t *testing.T) {
	var h Hasher = XXHasher{}

	clean := []byte("a = 1\n")
	trailingWS := []byte("a = 1   \n")
	semantic := []byte("a = 2\n")

	if RawHash(h, clean) == RawHash(h, trailingWS) {
		t.Fatal("RawHash should differ on a whitespace-only change")
	}
	if NormHash(h, clean) != NormHash(h, trailingWS) {
		t.Fatal("NormHash should be stable across a whitespace-only change")
	}
	if NormHash(h, clean) == NormHash(h, semantic) {
		t.Fatal("NormHash should differ on a semantic change")
	}
}
