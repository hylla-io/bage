package hashing

import (
	"fmt"

	"github.com/cespare/xxhash/v2"
)

// XXHasher implements Hasher with xxHash64 — the canonical content hash shared
// with Hylla. The digest is the 16-character, zero-padded, lowercase hex of the
// 64-bit sum, so Båge and Hylla produce byte-identical digests for identical
// inputs. This fixed format IS the cross-system contract: any change to the
// width or encoding here must be mirrored on the Hylla side.
//
// XXHasher holds no state, so the zero value is ready to use.
type XXHasher struct{}

// Sum returns the 16-character zero-padded lowercase hex of the xxHash64 of b.
func (XXHasher) Sum(b []byte) string {
	return fmt.Sprintf("%016x", xxhash.Sum64(b))
}
