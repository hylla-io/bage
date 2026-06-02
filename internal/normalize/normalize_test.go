package normalize

import (
	"bytes"
	"testing"
)

// TestNormalize exercises the canonical normalization rule across BOM
// stripping, CRLF folding, trailing-whitespace removal, EOF handling, no-op
// inputs, mixed cases, empty input, and interior-whitespace preservation.
func TestNormalize(t *testing.T) {
	bomPrefix := "\xEF\xBB\xBF"

	tests := []struct {
		name string
		in   string
		want string
	}{
		{
			name: "empty input",
			in:   "",
			want: "",
		},
		{
			name: "clean input is a no-op",
			in:   "package main\n\nfunc main() {}\n",
			want: "package main\n\nfunc main() {}\n",
		},
		{
			name: "strip leading BOM",
			in:   bomPrefix + "hello\n",
			want: "hello\n",
		},
		{
			// ALL consecutive leading BOMs are stripped (idempotency fixpoint):
			// a second normalization pass must not change the result.
			name: "all consecutive leading BOMs are stripped",
			in:   bomPrefix + bomPrefix + "x\n",
			want: "x\n",
		},
		{
			// A \r embedded in the BOM byte run collapses into a BOM under \r
			// removal; because BOM-strip runs LAST it is still removed, keeping
			// Normalize idempotent. (fuzz-discovered: EF BB \r BF)
			name: "carriage-return-split BOM is stripped after \\r removal",
			in:   "\xEF\xBB\r\xBFx\n",
			want: "x\n",
		},
		{
			name: "BOM mid-content is preserved",
			in:   "a" + bomPrefix + "b\n",
			want: "a" + bomPrefix + "b\n",
		},
		{
			name: "CRLF to LF",
			in:   "a\r\nb\r\nc\r\n",
			want: "a\nb\nc\n",
		},
		{
			name: "lone CR is dropped",
			in:   "a\rb\rc",
			want: "abc",
		},
		{
			name: "trailing spaces before newline",
			in:   "foo   \nbar\n",
			want: "foo\nbar\n",
		},
		{
			name: "trailing tabs before newline",
			in:   "foo\t\t\nbar\n",
			want: "foo\nbar\n",
		},
		{
			name: "mixed trailing spaces and tabs before newline",
			in:   "foo \t \t\nbar\n",
			want: "foo\nbar\n",
		},
		{
			name: "trailing whitespace at EOF without newline",
			in:   "foo   ",
			want: "foo",
		},
		{
			name: "trailing tabs at EOF without newline",
			in:   "foo\t\t",
			want: "foo",
		},
		{
			name: "trailing whitespace at EOF after final newline",
			in:   "foo\n   ",
			want: "foo\n",
		},
		{
			name: "trailing CR before newline is stripped",
			in:   "foo \r\nbar",
			want: "foo\nbar",
		},
		{
			name: "interior single spaces preserved",
			in:   "a b c\n",
			want: "a b c\n",
		},
		{
			name: "interior tabs preserved verbatim",
			in:   "a\tb\tc\n",
			want: "a\tb\tc\n",
		},
		{
			name: "interior multiple spaces preserved",
			in:   "a   b\n",
			want: "a   b\n",
		},
		{
			name: "leading indentation preserved",
			in:   "\tindented\n    spaces\n",
			want: "\tindented\n    spaces\n",
		},
		{
			name: "blank line with only whitespace becomes empty line",
			in:   "a\n   \nb\n",
			want: "a\n\nb\n",
		},
		{
			name: "all-whitespace input collapses to empty",
			in:   "   \t  ",
			want: "",
		},
		{
			name: "everything at once: BOM, CRLF, trailing ws, interior ws, EOF ws",
			in:   bomPrefix + "func f() {\t \r\n\treturn  \t\r\n}  \r\n   ",
			want: "func f() {\n\treturn\n}\n",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := Normalize([]byte(tt.in))
			if !bytes.Equal(got, []byte(tt.want)) {
				t.Errorf("Normalize(%q) = %q, want %q", tt.in, got, tt.want)
			}
		})
	}
}

// TestNormalizeDoesNotMutateInput verifies Normalize never writes through its
// argument slice, since callers pass live file bytes.
func TestNormalizeDoesNotMutateInput(t *testing.T) {
	in := []byte("\xEF\xBB\xBFfoo   \r\nbar")
	cp := make([]byte, len(in))
	copy(cp, in)

	_ = Normalize(in)

	if !bytes.Equal(in, cp) {
		t.Errorf("Normalize mutated its input: got %q, want %q", in, cp)
	}
}

// FuzzNormalizeIdempotent asserts the load-bearing property region_hash relies
// on: Normalize is idempotent (normalizing twice equals normalizing once) and
// its output never contains a CR. If either breaks, two systems hashing the same
// content could disagree, silently breaking drift detection. Arbitrary bytes
// (binary, multibyte UTF-8, lone CRs, BOMs) are fuzzed.
func FuzzNormalizeIdempotent(f *testing.F) {
	for _, s := range []string{
		"", "a", "a\r\nb\r\n", "  \t \n trailing \t", "\xEF\xBB\xBFbom",
		"café ☕\r\n日本語  \n", "no newline", "\x00\x01\x02binary", "\r\r\r", "x   ",
	} {
		f.Add([]byte(s))
	}
	f.Fuzz(func(t *testing.T, b []byte) {
		once := Normalize(b)
		twice := Normalize(once)
		if !bytes.Equal(once, twice) {
			t.Fatalf("Normalize not idempotent: %q -> %q -> %q", b, once, twice)
		}
		if bytes.IndexByte(once, '\r') >= 0 {
			t.Fatalf("Normalize left a CR in output: %q (from %q)", once, b)
		}
	})
}
