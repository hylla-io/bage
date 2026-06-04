package main

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/region"
)

// writeNamed writes src to a file named name under a fresh temp dir and returns
// its path, so show tests can exercise per-extension language detection.
func writeNamed(t *testing.T, name, src string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte(src), 0o644); err != nil {
		t.Fatalf("write temp %q: %v", name, err)
	}
	return path
}

// TestRunShowGoListsDeclsWithHashes shows a Go file and asserts each declaration
// in the human-readable output carries its kind, name, line/byte range, and a
// region_hash, plus the file-level raw/norm hashes.
func TestRunShowGoListsDeclsWithHashes(t *testing.T) {
	const src = "package main\n\nfunc helper() int { return 7 }\n\nfunc main() { _ = helper() }\n"
	path := writeNamed(t, "main.go", src)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"show", "--file", path}, &stdout, &stderr); err != nil {
		t.Fatalf("run show: %v\nstderr: %s", err, stderr.String())
	}
	out := stdout.String()

	for _, want := range []string{"helper", "main", "function_declaration", "region=", "raw=", "norm="} {
		if !strings.Contains(out, want) {
			t.Fatalf("show output missing %q:\n%s", want, out)
		}
	}
}

// TestRunShowRoundTripsRegionHash is the critical property: the region_hash show
// emits for a block is byte-identical to region.HashRegion over that block's byte
// range, AND a subsequent apply on that exact byte range with that hash is
// accepted (the resolver verifies it Exact in place). show's hashes are useless
// if apply won't accept them.
func TestRunShowRoundTripsRegionHash(t *testing.T) {
	const src = "package main\n\nfunc helper() int { return 7 }\n\nfunc main() { _ = helper() }\n"
	path := writeNamed(t, "main.go", src)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"show", "--file", path, "--json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run show --json: %v\nstderr: %s", err, stderr.String())
	}

	var view showView
	if err := json.Unmarshal(stdout.Bytes(), &view); err != nil {
		t.Fatalf("show --json not parseable: %v\nout: %s", err, stdout.String())
	}
	if len(view.Outline) == 0 {
		t.Fatal("show --json emitted an empty outline for a Go file with decls")
	}

	// Find the helper() declaration block.
	var helper *showBlock
	for i := range view.Outline {
		if view.Outline[i].Name == "helper" {
			helper = &view.Outline[i]
			break
		}
	}
	if helper == nil {
		t.Fatalf("no 'helper' block in outline:\n%s", stdout.String())
	}

	// Property 1: the emitted region_hash equals region.HashRegion over the
	// reported byte range, computed via the SAME path apply verifies.
	want := region.HashRegion([]byte(src), helper.StartByte, helper.EndByte)
	if helper.RegionHash != want {
		t.Fatalf("region_hash mismatch: show=%q HashRegion=%q (bytes [%d:%d])",
			helper.RegionHash, want, helper.StartByte, helper.EndByte)
	}

	// Property 2: apply on that exact byte range with the shown hash is ACCEPTED.
	var astdout, astderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		"--start", strconv.Itoa(helper.StartByte),
		"--end", strconv.Itoa(helper.EndByte),
		"--region-hash", helper.RegionHash,
		"--text", "func helper() int { return 9 }",
		"--lang", "go",
	}, &astdout, &astderr)
	if err != nil {
		t.Fatalf("apply with shown region_hash rejected: %v\nstderr: %s", err, astderr.String())
	}
	got := readFile(t, path)
	if !strings.Contains(got, "return 9") || strings.Contains(got, "return 7") {
		t.Fatalf("round-trip apply did not land:\n%s", got)
	}
}

// TestRunShowTextFallbackListsLines shows a non-grammar .txt file and asserts the
// outline is the per-line blocks the text fallback produces, each with a
// region_hash, plus the file hashes.
func TestRunShowTextFallbackListsLines(t *testing.T) {
	const src = "alpha\nbeta\ngamma\n"
	path := writeNamed(t, "notes.txt", src)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"show", "--file", path, "--json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run show --json: %v\nstderr: %s", err, stderr.String())
	}

	var view showView
	if err := json.Unmarshal(stdout.Bytes(), &view); err != nil {
		t.Fatalf("show --json not parseable: %v\nout: %s", err, stdout.String())
	}
	if len(view.Outline) != 3 {
		t.Fatalf("text fallback outline = %d blocks, want 3:\n%s", len(view.Outline), stdout.String())
	}
	for _, b := range view.Outline {
		if b.Kind != "line" {
			t.Fatalf("text fallback block kind = %q, want \"line\"", b.Kind)
		}
		if b.RegionHash == "" {
			t.Fatalf("text fallback block missing region_hash: %+v", b)
		}
		if want := region.HashRegion([]byte(src), b.StartByte, b.EndByte); b.RegionHash != want {
			t.Fatalf("line region_hash mismatch: show=%q want=%q", b.RegionHash, want)
		}
	}
	if view.RawHash == "" || view.NormHash == "" {
		t.Fatalf("file hashes missing: %+v", view)
	}
}

// TestRunShowEmptyFile shows an empty file: an empty outline plus the file hashes,
// with no crash.
func TestRunShowEmptyFile(t *testing.T) {
	path := writeNamed(t, "empty.go", "")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"show", "--file", path, "--json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run show empty: %v\nstderr: %s", err, stderr.String())
	}

	var view showView
	if err := json.Unmarshal(stdout.Bytes(), &view); err != nil {
		t.Fatalf("show --json not parseable: %v\nout: %s", err, stdout.String())
	}
	if len(view.Outline) != 0 {
		t.Fatalf("empty file outline = %d, want 0", len(view.Outline))
	}
	if view.RawHash == "" || view.NormHash == "" {
		t.Fatalf("empty file should still report hashes: %+v", view)
	}
}

// TestRunShowBinaryNoCrash shows a file of odd/binary bytes: the text fallback is
// lossless and must not crash; an outline + file hashes are produced.
func TestRunShowBinaryNoCrash(t *testing.T) {
	path := writeNamed(t, "blob.bin", string([]byte{0x00, 0x01, 0xff, 'a', '\n', 0xfe, 'b'}))

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"show", "--file", path, "--json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run show binary: %v\nstderr: %s", err, stderr.String())
	}
	var view showView
	if err := json.Unmarshal(stdout.Bytes(), &view); err != nil {
		t.Fatalf("show --json not parseable: %v\nout: %s", err, stdout.String())
	}
	if view.RawHash == "" {
		t.Fatal("binary file should still report a raw hash")
	}
}

// TestRunShowIsReadOnly asserts show writes NOTHING: the file bytes are unchanged
// and no extra files appear in the directory.
func TestRunShowIsReadOnly(t *testing.T) {
	const src = "package main\n\nfunc main() {}\n"
	path := writeNamed(t, "main.go", src)
	dir := filepath.Dir(path)

	before, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("read dir: %v", err)
	}

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"show", "--file", path}, &stdout, &stderr); err != nil {
		t.Fatalf("run show: %v\nstderr: %s", err, stderr.String())
	}

	if got := readFile(t, path); got != src {
		t.Fatalf("show mutated the file:\n%s", got)
	}
	after, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("read dir: %v", err)
	}
	if len(after) != len(before) {
		t.Fatalf("show created/removed files: before=%d after=%d", len(before), len(after))
	}
}

// TestRunShowMissingFile asserts a missing --file is a clean error with a stderr
// diagnostic, and no required --file is rejected.
func TestRunShowMissingFile(t *testing.T) {
	for _, tc := range []struct {
		name string
		args []string
	}{
		{"no file flag", []string{"show"}},
		{"nonexistent file", []string{"show", "--file", filepath.Join(t.TempDir(), "nope.go")}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			if err := run(context.Background(), tc.args, &stdout, &stderr); err == nil {
				t.Fatalf("expected error, got nil (stderr: %q)", stderr.String())
			}
			if stderr.Len() == 0 {
				t.Fatal("expected diagnostic on stderr, got none")
			}
		})
	}
}
