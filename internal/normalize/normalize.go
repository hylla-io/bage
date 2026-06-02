// Package normalize provides the shared content-normalization rule used to
// compute Båge's normalized content hash. The rule is byte-identical with
// Hylla's so the two systems agree on whitespace-only drift classification.
package normalize

// bom is the UTF-8 byte-order-mark sequence stripped from the start of input.
var bom = []byte{0xEF, 0xBB, 0xBF}

// Normalize applies Båge's canonical content-normalization rule and returns a
// newly allocated slice; the input is never mutated. The rule, in order:
//
//  1. normalize line endings to LF by dropping every carriage return (\r);
//  2. strip trailing horizontal whitespace ([ \t\r]) immediately before each
//     LF and at end-of-input;
//  3. strip ALL consecutive leading UTF-8 BOMs (0xEF 0xBB 0xBF) — LAST.
//
// It is a pure function with no I/O. Interior whitespace (including tabs) is
// preserved byte-for-byte. The ORDER is load-bearing for idempotency (and
// region_hash stability across systems): BOM stripping runs last, on the CR-free
// output, so a \r embedded in a BOM byte run cannot re-form a leading BOM after
// the strip; and ALL leading BOMs are stripped, not just one. Both are
// fuzz-discovered invariants (FuzzNormalizeIdempotent) — Hylla MUST match.
func Normalize(b []byte) []byte {
	// Steps 1 & 2: drop all \r, and buffer any run of horizontal whitespace until
	// a non-whitespace, non-LF byte proves it was interior. Buffered whitespace
	// before an LF or at EOF is discarded.
	out := make([]byte, 0, len(b))
	pending := make([]byte, 0, 16) // buffered ' '/'\t' that may be interior
	for _, c := range b {
		switch c {
		case '\r':
			// Dropped entirely; also counts toward trailing-ws stripping.
		case ' ', '\t':
			pending = append(pending, c)
		case '\n':
			// Trailing horizontal whitespace before the newline is discarded.
			pending = pending[:0]
			out = append(out, '\n')
		default:
			// Interior whitespace is real: flush it verbatim, then the byte.
			out = append(out, pending...)
			pending = pending[:0]
			out = append(out, c)
		}
	}
	// Any whitespace pending at EOF is trailing and discarded.

	// Step 3: strip every consecutive leading UTF-8 BOM — done LAST, after \r
	// removal, on purpose. A \r embedded inside the BOM byte run (e.g. EF BB \r
	// BF) would otherwise be removed by the loop above and re-form a leading BOM
	// that an earlier strip had already walked past, breaking idempotency. Doing
	// it on the CR-free output guarantees the result never begins with a BOM, so
	// re-normalizing is a fixpoint.
	for len(out) >= len(bom) && out[0] == bom[0] && out[1] == bom[1] && out[2] == bom[2] {
		out = out[len(bom):]
	}
	return out
}
