package bage_test

import (
	"testing"

	"github.com/hylla-io/bage/pkg/bage"
)

// TestNormalizeReexport asserts the facade Normalize applies the canonical rule
// (drop CR, strip trailing ws, strip ALL leading BOMs last) — the rule Hylla
// must single-source from here.
func TestNormalizeReexport(t *testing.T) {
	got := string(bage.Normalize([]byte("\xEF\xBB\xBFa  \r\nb\r\n")))
	if want := "a\nb\n"; got != want {
		t.Fatalf("Normalize = %q, want %q", got, want)
	}
}

// TestHashReexportsFormat asserts every re-exported hash returns the contract
// encoding: 16-char zero-padded lowercase hex, and is deterministic.
func TestHashReexportsFormat(t *testing.T) {
	h := bage.XXHasher{}
	raw := []byte("hello world\n")
	for _, c := range []struct{ name, got string }{
		{"RawHash", bage.RawHash(h, raw)},
		{"NormHash", bage.NormHash(h, raw)},
		{"RegionHash", bage.RegionHash(raw, 0, len(raw))},
	} {
		if len(c.got) != 16 {
			t.Errorf("%s = %q, want 16-char hex", c.name, c.got)
		}
		for _, r := range c.got {
			if !(r >= '0' && r <= '9' || r >= 'a' && r <= 'f') {
				t.Errorf("%s = %q has non lowercase-hex rune %q", c.name, c.got, r)
			}
		}
	}
	if bage.NormHash(h, raw) != bage.NormHash(h, raw) {
		t.Error("NormHash not deterministic")
	}
}

// TestRegionHashMatchesNormHashOfRegion pins the cross-system contract: the
// region_hash of src[start:end] equals NormHash over exactly those region bytes,
// which is how Hylla recomputes the identical anchor from a stored node's bytes.
func TestRegionHashMatchesNormHashOfRegion(t *testing.T) {
	src := []byte("package main\n\nfunc greet() {}\n")
	start, end := 14, 29 // "func greet() {}"
	got := bage.RegionHash(src, start, end)
	want := bage.NormHash(bage.XXHasher{}, src[start:end])
	if got != want {
		t.Fatalf("RegionHash %q != NormHash-of-region %q (bytes %q)", got, want, src[start:end])
	}
}
