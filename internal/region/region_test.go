package region

import (
	"regexp"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
)

var hexRe = regexp.MustCompile(`^[0-9a-f]{16}$`)

func TestHashRegionDeterministicAndFormat(t *testing.T) {
	src := []byte("package main\n\nfunc main() {}\n")
	start, end := 14, 28 // "func main() {}"
	if got := string(src[start:end]); got != "func main() {}" {
		t.Fatalf("test bug: slice = %q", got)
	}

	h1 := HashRegion(src, start, end)
	h2 := HashRegion(src, start, end)
	if h1 != h2 {
		t.Fatalf("HashRegion not deterministic: %s vs %s", h1, h2)
	}
	if !hexRe.MatchString(h1) {
		t.Fatalf("HashRegion = %q, want 16-char lowercase hex (%%016x)", h1)
	}

	// Same bytes at a different offset hash identically (content, not position).
	src2 := append([]byte("// prefix\n"), src...)
	if got := HashRegion(src2, start+10, end+10); got != h1 {
		t.Fatalf("same bytes at shifted offset hashed differently: %s vs %s", got, h1)
	}

	// Different bytes hash differently.
	if got := HashRegion([]byte("func main() {}!"), 0, 15); got == h1 {
		t.Fatalf("different content collided with %s", h1)
	}
}

func TestHashRegionEmptyRange(t *testing.T) {
	// An empty region is well-defined and stable.
	a := HashRegion([]byte("abc"), 1, 1)
	b := HashRegion([]byte("xyz"), 0, 0)
	if a != b {
		t.Fatalf("empty regions should hash identically: %s vs %s", a, b)
	}
	if !hexRe.MatchString(a) {
		t.Fatalf("empty-region hash = %q, want 16-char hex", a)
	}
}

func TestResolveExact(t *testing.T) {
	p := treesitter.New()
	live := []byte("package main\n\nfunc main() {}\n")
	start, end := 14, 28
	r := Region{
		Path:       "main.go",
		StartByte:  start,
		EndByte:    end,
		RegionHash: HashRegion(live, start, end),
	}

	gs, ge, status, err := Resolve(p, parser.LangGo, live, r)
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	if status != Exact {
		t.Fatalf("status = %v, want Exact", status)
	}
	if gs != start || ge != end {
		t.Fatalf("range = [%d:%d], want [%d:%d]", gs, ge, start, end)
	}
}

func TestResolveNoAnchorAuthoritative(t *testing.T) {
	// Empty RegionHash (single-model file mode): range is authoritative, no parse.
	p := treesitter.New()
	live := []byte("package main\n")
	r := Region{Path: "main.go", StartByte: 3, EndByte: 7, RegionHash: ""}

	gs, ge, status, err := Resolve(p, parser.LangGo, live, r)
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	if status != Exact {
		t.Fatalf("status = %v, want Exact", status)
	}
	if gs != 3 || ge != 7 {
		t.Fatalf("range = [%d:%d], want [3:7]", gs, ge)
	}
}

func TestResolveShifted(t *testing.T) {
	p := treesitter.New()
	// The region as originally anchored.
	orig := []byte("package main\n\nfunc main() {}\n")
	start, end := 14, 28
	hash := HashRegion(orig, start, end)
	r := Region{
		Path:       "main.go",
		StartByte:  start,
		EndByte:    end,
		RegionHash: hash,
	}

	// A concurrent edit prepended a new declaration: the func moved but its bytes
	// are unchanged, so the stale offset no longer hash-matches in place.
	prefix := "\nfunc helper() {}\n"
	live := []byte("package main\n" + prefix + "\nfunc main() {}\n")

	gs, ge, status, err := Resolve(p, parser.LangGo, live, r)
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	if status != Shifted {
		t.Fatalf("status = %v, want Shifted", status)
	}
	if got := string(live[gs:ge]); got != "func main() {}" {
		t.Fatalf("resolved bytes = %q, want %q", got, "func main() {}")
	}
	// And the resolved range hashes back to the original region_hash.
	if HashRegion(live, gs, ge) != hash {
		t.Fatalf("resolved range does not hash to the anchor")
	}
}

func TestResolveConflict(t *testing.T) {
	p := treesitter.New()
	orig := []byte("package main\n\nfunc main() {}\n")
	start, end := 14, 28
	r := Region{
		Path:       "main.go",
		StartByte:  start,
		EndByte:    end,
		RegionHash: HashRegion(orig, start, end),
	}

	// A concurrent edit changed the region's OWN content: no node hashes to the
	// anchor anymore — a hard conflict.
	live := []byte("package main\n\nfunc main() { return }\n")

	_, _, status, err := Resolve(p, parser.LangGo, live, r)
	if err == nil {
		t.Fatalf("Resolve: want error on conflict, got nil")
	}
	if status != Conflict {
		t.Fatalf("status = %v, want Conflict", status)
	}
}

func TestResolveAmbiguous(t *testing.T) {
	p := treesitter.New()
	// Anchor a region whose content also appears verbatim elsewhere in the live
	// file: two identical-content nodes ⇒ two hash matches ⇒ Ambiguous.
	body := "func twin() {}"
	orig := []byte("package main\n\n" + body + "\n")
	start := strings.Index(string(orig), body)
	end := start + len(body)
	hash := HashRegion(orig, start, end)
	r := Region{
		Path:       "main.go",
		StartByte:  start,
		EndByte:    end,
		RegionHash: hash,
	}

	// Live file has TWO copies of the identical declaration. To force the stale
	// in-place offset to miss (so the parse path runs), prepend a line.
	live := []byte("package main\n\n// pad\n" + body + "\n\n" + body + "\n")

	_, _, status, err := Resolve(p, parser.LangGo, live, r)
	if err == nil {
		t.Fatalf("Resolve: want error on ambiguous, got nil")
	}
	if status != Ambiguous {
		t.Fatalf("status = %v, want Ambiguous", status)
	}
}

func TestResolveStaleOffsetOutOfBoundsFallsToParse(t *testing.T) {
	// A stale offset past the end of a now-shorter file must not panic; it falls
	// through to the parse-and-match path. Here the content is gone ⇒ Conflict.
	p := treesitter.New()
	r := Region{
		Path:       "main.go",
		StartByte:  100,
		EndByte:    200,
		RegionHash: HashRegion([]byte("func gone() {}"), 0, 14),
	}
	live := []byte("package main\n")

	_, _, status, err := Resolve(p, parser.LangGo, live, r)
	if err == nil {
		t.Fatalf("Resolve: want error, got nil")
	}
	if status != Conflict {
		t.Fatalf("status = %v, want Conflict", status)
	}
}

// TestHashRegionNormalizedTolerance pins HYLLA_NODE_CONTRACT §4: region_hash is
// computed over NORMALIZED bytes, so a block and the SAME block differing only by
// trailing horizontal whitespace and CRLF line endings hash IDENTICALLY, while any
// SEMANTIC change to the block's bytes hashes differently. This is the property
// that lets a whitespace-only reformat avoid a false conflict.
func TestHashRegionNormalizedTolerance(t *testing.T) {
	// A multi-line block: clean LF, no trailing whitespace.
	clean := []byte("func f() {\n\tx := 1\n\treturn x\n}")
	cleanHash := HashRegion(clean, 0, len(clean))

	// The byte-identical block save for trailing spaces/tabs before each newline
	// and CRLF line endings — normalization (§4 steps 2 & 3) erases both, so the
	// region_hash MUST be unchanged.
	reformatted := []byte("func f() {  \r\n\tx := 1\t\r\n\treturn x \r\n}")
	if reformatted[0] != clean[0] {
		t.Fatalf("test bug: blocks should start with the same byte")
	}
	if got := HashRegion(reformatted, 0, len(reformatted)); got != cleanHash {
		t.Fatalf("trailing-ws / CRLF reformat changed region_hash: %s vs %s (normalized hash must be identical)", got, cleanHash)
	}

	// A SEMANTIC change (interior bytes differ) MUST change the hash: normalization
	// only strips trailing horizontal whitespace + CR, never interior content.
	semantic := []byte("func f() {\n\tx := 2\n\treturn x\n}")
	if got := HashRegion(semantic, 0, len(semantic)); got == cleanHash {
		t.Fatalf("semantic change collided with clean hash %s — normalization must not erase interior content", cleanHash)
	}

	// And an interior-whitespace change (a tab inside the line, not trailing) is
	// also semantic under §4 — interior whitespace is preserved byte-for-byte.
	interior := []byte("func f() {\n\t\tx := 1\n\treturn x\n}")
	if got := HashRegion(interior, 0, len(interior)); got == cleanHash {
		t.Fatalf("interior-whitespace change collided with clean hash %s — interior ws must be preserved", cleanHash)
	}
}

// TestResolveBenignWhitespaceReformat pins the end-to-end normalized-anchor
// tolerance: a block is anchored by region_hash, then the live file gains a
// whitespace-only reformat that BOTH moves the block (a prefix line is prepended,
// so the in-place offset misses) AND rewrites the block's own trailing whitespace
// (so its raw bytes differ from what was anchored). Because region_hash is over
// NORMALIZED bytes, the resolver still relocates the block (Exact or Shifted),
// NEVER Conflict — proving a reformat does not false-conflict.
func TestResolveBenignWhitespaceReformat(t *testing.T) {
	p := treesitter.New()

	// Anchor main() against the clean original.
	orig := []byte("package main\n\nfunc main() {\n\treturn\n}\n")
	body := "func main() {\n\treturn\n}"
	start := strings.Index(string(orig), body)
	end := start + len(body)
	if got := string(orig[start:end]); got != body {
		t.Fatalf("test bug: slice = %q", got)
	}
	hash := HashRegion(orig, start, end)
	r := Region{
		Path:       "main.go",
		StartByte:  start,
		EndByte:    end,
		RegionHash: hash,
	}

	// Live file: a header line is prepended (moves main() down so the stale offset
	// misses) AND main()'s own lines gained trailing whitespace + CRLF. The raw
	// bytes of the block now differ from the anchored bytes; only the NORMALIZED
	// bytes still match.
	live := []byte("package main\n\nfunc helper() {}\n\nfunc main() {  \r\n\treturn \r\n}\n")

	gs, ge, status, err := Resolve(p, parser.LangGo, live, r)
	if err != nil {
		t.Fatalf("Resolve: benign reformat must not error, got %v (status %v)", err, status)
	}
	if status == Conflict || status == Ambiguous {
		t.Fatalf("status = %v, want Exact or Shifted — a whitespace-only reformat must not conflict", status)
	}
	// The resolved bytes are main()'s reformatted source, and they normalize back
	// to the original anchor hash.
	if HashRegion(live, gs, ge) != hash {
		t.Fatalf("resolved range %q does not normalize to the anchor hash", string(live[gs:ge]))
	}
}

func TestResolveStatusString(t *testing.T) {
	cases := map[ResolveStatus]string{
		Exact:            "exact",
		Shifted:          "shifted",
		Conflict:         "conflict",
		Ambiguous:        "ambiguous",
		ResolveStatus(9): "unknown",
	}
	for s, want := range cases {
		if got := s.String(); got != want {
			t.Fatalf("ResolveStatus(%d).String() = %q, want %q", int(s), got, want)
		}
	}
}
