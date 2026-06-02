// Package hashing defines the Hasher port and the raw/normalized content-hash
// helpers Båge uses for drift discipline (SPEC §4.3). RawHash gates byte-offset
// validity against the live file; NormHash classifies whitespace-only drift.
//
// XXHasher (xxhash.go) is the canonical adapter, matching Hylla's content hash;
// FNVHasher is a dependency-free fallback. The two-hash semantics are
// engine-independent, so the drift logic is unaffected by which Hasher is used.
package hashing

import (
	"encoding/hex"
	"hash/fnv"

	"github.com/hylla-io/bage/internal/normalize"
)

// Hasher computes a stable hex digest of a byte slice. Implementations must be
// deterministic and free of hidden state so equal inputs always yield equal
// digests.
type Hasher interface {
	// Sum returns the lowercase hex-encoded digest of b.
	Sum(b []byte) string
}

// FNVHasher is the stdlib adapter implementing Hasher with FNV-1a (64-bit),
// hex-encoded. It is dependency-free and holds no state, so the zero value is
// ready to use. It is the fallback Hasher; XXHasher is the canonical one shared
// with Hylla.
type FNVHasher struct{}

// Sum returns the lowercase hex-encoded FNV-1a 64-bit digest of b.
func (FNVHasher) Sum(b []byte) string {
	h := fnv.New64a()
	// fnv's Write never returns an error, so the result is ignored deliberately.
	_, _ = h.Write(b)
	return hex.EncodeToString(h.Sum(nil))
}

// RawHash returns the digest of raw exactly as given, gating byte-offset
// validity: a byte range is trustworthy only while the live file's RawHash is
// unchanged.
func RawHash(h Hasher, raw []byte) string {
	return h.Sum(raw)
}

// NormHash returns the digest of raw after normalize.Normalize, classifying
// whitespace-only drift: when RawHash differs but NormHash matches, the change
// is whitespace-only and the range can be re-grounded rather than rejected.
func NormHash(h Hasher, raw []byte) string {
	return h.Sum(normalize.Normalize(raw))
}
