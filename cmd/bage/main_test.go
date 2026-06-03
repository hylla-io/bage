package main

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
	"github.com/hylla-io/bage/internal/region"
)

// writeTemp writes src to a fresh temp .go file and returns its path. The file
// lives under t.TempDir so it is cleaned up automatically.
func writeTemp(t *testing.T, src string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "main.go")
	if err := os.WriteFile(path, []byte(src), 0o644); err != nil {
		t.Fatalf("write temp: %v", err)
	}
	return path
}

// readFile reads path or fails the test.
func readFile(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %q: %v", path, err)
	}
	return string(b)
}

// parses reports whether src parses cleanly as Go via the real treesitter
// adapter; used to assert the post-edit file is still valid Go.
func parses(t *testing.T, src string) bool {
	t.Helper()
	tree, err := treesitter.New().Parse(context.Background(), parser.LangGo, []byte(src))
	if err != nil {
		return false
	}
	tree.Close()
	return true
}

// TestRunApplyByteRangeRenamesVar applies a byte-addressed region edit renaming a
// variable with an identity formatter (cat) and asserts the file is updated, still
// parses, and the EditResult line is printed.
func TestRunApplyByteRangeRenamesVar(t *testing.T) {
	const src = "package main\n\nfunc main() {\n\tfoo := 1\n\t_ = foo\n}\n"
	path := writeTemp(t, src)

	// Replace the first "foo" (the declaration) with "bar".
	start := strings.Index(src, "foo")
	end := start + len("foo")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		"--start", strconv.Itoa(start),
		"--end", strconv.Itoa(end),
		"--text", "bar",
		"--lang", "go",
		"--fmt", "cat",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run apply: %v\nstderr: %s", err, stderr.String())
	}

	got := readFile(t, path)
	wantDecl := "bar := 1"
	if !strings.Contains(got, wantDecl) {
		t.Fatalf("file not updated: missing %q\ngot:\n%s", wantDecl, got)
	}
	if !parses(t, got) {
		t.Fatalf("edited file does not parse:\n%s", got)
	}
	if !strings.Contains(stdout.String(), "applied "+path) {
		t.Fatalf("stdout missing applied line: %q", stdout.String())
	}
}

// TestRunApplyLineRangeReplacesLine applies a line-addressed region edit (--line)
// replacing a whole statement line and asserts the file is updated and still
// parses. Line addressing resolves to a byte range via a LineIndex before the
// edit is staged.
func TestRunApplyLineRangeReplacesLine(t *testing.T) {
	const src = "package main\n\nfunc main() {\n\tfoo := 1\n\t_ = foo\n}\n"
	path := writeTemp(t, src)

	// Line 4 (1-based) is "\tfoo := 1\n"; replace the whole line.
	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		"--line", "4",
		"--text", "\tbar := 2\n",
		"--lang", "go",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run apply: %v\nstderr: %s", err, stderr.String())
	}

	got := readFile(t, path)
	if strings.Contains(got, "foo := 1") || !strings.Contains(got, "bar := 2") {
		t.Fatalf("line not replaced:\n%s", got)
	}
	if !parses(t, got) {
		t.Fatalf("edited file does not parse:\n%s", got)
	}
}

// TestRunApplyLineIsNewlineAgnostic asserts line addressing replaces line
// CONTENT and preserves structure: a surrounding blank line survives, and
// --text behaves identically with or without a trailing newline (it never merges
// the line into the next or injects a blank line). Regression for a dogfood-found
// bug where `--line N --text x` (no newline) merged line N into N+1.
func TestRunApplyLineIsNewlineAgnostic(t *testing.T) {
	const src = "package main\n\nvar x = 1\n\nvar y = 2\n"
	const want = "package main\n\nvar x = 99\n\nvar y = 2\n"

	for _, text := range []string{"var x = 99", "var x = 99\n"} {
		path := writeTemp(t, src)
		var stdout, stderr bytes.Buffer
		err := run(context.Background(), []string{
			"apply", "--file", path, "--line", "3", "--text", text,
		}, &stdout, &stderr)
		if err != nil {
			t.Fatalf("run apply (text=%q): %v\nstderr: %s", text, err, stderr.String())
		}
		got := readFile(t, path)
		if got != want {
			t.Fatalf("text=%q:\n got: %q\nwant: %q", text, got, want)
		}
		if !parses(t, got) {
			t.Fatalf("text=%q: edited file does not parse:\n%s", text, got)
		}
	}
}

// TestRunApplyTextFile applies replacement text read from --text-file instead of
// --text, the path for large/multi-line edits. Line mode still strips one
// trailing newline, so the result is clean.
func TestRunApplyTextFile(t *testing.T) {
	const src = "package main\n\nvar X = 1\n"
	path := writeTemp(t, src)
	tf := filepath.Join(t.TempDir(), "repl.txt")
	if err := os.WriteFile(tf, []byte("var X = 42\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply", "--file", path, "--line", "3", "--text-file", tf,
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run apply --text-file: %v\nstderr: %s", err, stderr.String())
	}
	got := readFile(t, path)
	if !strings.Contains(got, "var X = 42") || strings.Contains(got, "var X = 1\n") {
		t.Fatalf("--text-file not applied:\n%s", got)
	}
	if !parses(t, got) {
		t.Fatalf("edited file does not parse:\n%s", got)
	}
}

// TestRunApplyRegionHashRelocatesAfterShift anchors the edit with a region_hash
// and supplies a STALE byte range after the target moved (a benign shift). The
// resolver must reparse, match the region_hash to the relocated node, and apply
// at the current offset rather than the stale one.
func TestRunApplyRegionHashRelocatesAfterShift(t *testing.T) {
	const src = "package main\n\nfunc helper() int { return 7 }\n\nfunc main() { _ = helper() }\n"
	path := writeTemp(t, src)

	// Anchor on "func helper() int { return 7 }" at its ORIGINAL offset, but pass
	// a byte range shifted by inserting a comment line first so the recorded
	// range is stale yet the content (hash) still matches at the new location.
	target := "func helper() int { return 7 }"
	origStart := strings.Index(src, target)
	origEnd := origStart + len(target)
	hash := region.HashRegion([]byte(src), origStart, origEnd)

	// Mutate the file on disk: prepend a comment so helper() shifts down.
	shifted := "// shifted down\n" + src
	if err := os.WriteFile(path, []byte(shifted), 0o644); err != nil {
		t.Fatalf("write shifted: %v", err)
	}

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		// Stale byte range from the pre-shift file.
		"--start", strconv.Itoa(origStart),
		"--end", strconv.Itoa(origEnd),
		"--region-hash", hash,
		"--text", "func helper() int { return 9 }",
		"--lang", "go",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run apply (benign shift): %v\nstderr: %s", err, stderr.String())
	}

	got := readFile(t, path)
	if !strings.Contains(got, "return 9") || strings.Contains(got, "return 7") {
		t.Fatalf("benign shift did not relocate edit:\n%s", got)
	}
	if !strings.Contains(got, "// shifted down") {
		t.Fatalf("prepended content lost:\n%s", got)
	}
}

// TestRunApplyRegionHashConflictRejects anchors the edit with a region_hash whose
// content no longer exists anywhere in the live file. Resolve must report a
// conflict and the file must be left untouched (reject-not-corrupt, SPEC §8.4).
func TestRunApplyRegionHashConflictRejects(t *testing.T) {
	const src = "package main\n\nfunc main() {\n\tfoo := 1\n\t_ = foo\n}\n"
	path := writeTemp(t, src)

	// A region_hash for bytes that are not present in the file at all.
	bogus := hashing.RawHash(hashing.XXHasher{}, []byte("does-not-exist-anywhere"))

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		"--start", "0",
		"--end", "4",
		"--region-hash", bogus,
		"--text", "X",
		"--lang", "go",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatal("expected conflict error, got nil")
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("file mutated on conflict: %q", got)
	}
}

// TestRunApplyBadRangeLeavesFileUntouched asserts that an out-of-bounds byte
// range returns an error and the file is unchanged.
func TestRunApplyBadRangeLeavesFileUntouched(t *testing.T) {
	const src = "package main\n"
	path := writeTemp(t, src)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		"--start", "0",
		"--end", "9999",
		"--text", "x",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatal("expected error for out-of-bounds range, got nil")
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("file mutated on bad range: %q", got)
	}
}

// TestRunUsageErrors covers unknown subcommand, no subcommand, and missing
// required/conflicting flags — each must return a non-nil error and emit
// usage/diagnostic text to stderr. The rename cases here are hermetic: they fail
// on argument validation before any LSP server is spawned.
func TestRunUsageErrors(t *testing.T) {
	tests := []struct {
		name string
		args []string
	}{
		{"no subcommand", nil},
		{"unknown subcommand", []string{"frobnicate"}},
		{"missing file", []string{"apply", "--start", "0", "--end", "1", "--text", "x"}},
		{"no addressing", []string{"apply", "--file", "/tmp/x.go", "--text", "x"}},
		{"byte and line", []string{"apply", "--file", "/tmp/x.go", "--start", "0", "--end", "1", "--line", "1"}},
		{"line and lines", []string{"apply", "--file", "/tmp/x.go", "--line", "1", "--lines", "1-2"}},
		{"bad lines format", []string{"apply", "--file", "/tmp/x.go", "--lines", "abc"}},
		{"lines start past end", []string{"apply", "--file", "/tmp/x.go", "--lines", "5-2"}},
		{"unknown lang", []string{"apply", "--file", "/tmp/x.go", "--start", "0", "--end", "1", "--lang", "rust"}},
		{"rename missing file", []string{"rename", "--line", "0", "--col", "0", "--new", "x"}},
		{"rename missing pos", []string{"rename", "--file", "/tmp/x.go", "--new", "x"}},
		{"rename missing new", []string{"rename", "--file", "/tmp/x.go", "--line", "0", "--col", "0"}},
		{"rename unknown lang", []string{"rename", "--file", "/tmp/x.go", "--line", "0", "--col", "0", "--new", "x", "--lang", "rust"}},
		{"rename empty lsp", []string{"rename", "--file", "/tmp/x.go", "--line", "0", "--col", "0", "--new", "x", "--lsp", "  "}},
		{"delete missing file", []string{"delete", "--raw-hash", "abc"}},
		{"delete nonexistent", []string{"delete", "--file", "/tmp/does-not-exist-bage-delete-test.go"}},
	}
	for _, tc := range tests {
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

// TestRunDeleteRemovesFile deletes a file via the CLI with no --raw-hash
// (delete-current: the expected hash is computed from the live bytes), and
// asserts the file is gone and a confirmation line naming the path is printed.
func TestRunDeleteRemovesFile(t *testing.T) {
	path := writeTemp(t, "package main\n\nfunc main() {}\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"delete", "--file", path}, &stdout, &stderr); err != nil {
		t.Fatalf("run delete: %v\nstderr: %s", err, stderr.String())
	}
	if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
		t.Fatalf("delete left the file behind: stat err = %v", statErr)
	}
	if !strings.Contains(stdout.String(), path) {
		t.Fatalf("confirmation line did not name the path:\n%s", stdout.String())
	}
}

// TestRunDeleteRawHashGate proves the --raw-hash drift gate: a delete whose
// supplied --raw-hash does NOT match the live bytes HARD-REJECTS and the file is
// STILL THERE — nothing is unlinked.
func TestRunDeleteRawHashGate(t *testing.T) {
	const src = "package main\n\nfunc main() {}\n"
	path := writeTemp(t, src)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"delete", "--file", path, "--raw-hash", "deadbeefnotthehash",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatalf("delete with stale --raw-hash = nil error, want reject (stderr: %q)", stderr.String())
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("drift-rejected delete altered the file:\n%s", got)
	}
}

// TestRunDeleteMatchingRawHash deletes a file whose supplied --raw-hash matches
// the live bytes: the drift gate is satisfied and the file is removed.
func TestRunDeleteMatchingRawHash(t *testing.T) {
	const src = "package main\n\nfunc main() {}\n"
	path := writeTemp(t, src)
	want := hashing.RawHash(hashing.XXHasher{}, []byte(src))

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{
		"delete", "--file", path, "--raw-hash", want,
	}, &stdout, &stderr); err != nil {
		t.Fatalf("run delete with matching --raw-hash: %v\nstderr: %s", err, stderr.String())
	}
	if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
		t.Fatalf("delete left the file behind: stat err = %v", statErr)
	}
}

// TestRunRenameWithGopls drives the full rename flow against a live gopls. It is
// skipped when gopls is not on PATH so the suite stays hermetic. It writes a tiny
// module to a temp dir, renames a local variable, and asserts the file is updated
// and still parses.
func TestRunRenameWithGopls(t *testing.T) {
	if _, err := exec.LookPath("gopls"); err != nil {
		t.Skip("gopls not found on PATH; skipping live rename test")
	}

	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "go.mod"), []byte("module example.com/tmprename\n\ngo 1.21\n"), 0o644); err != nil {
		t.Fatalf("write go.mod: %v", err)
	}
	const src = "package tmprename\n\nfunc Run() int {\n\tfoo := 1\n\treturn foo\n}\n"
	path := filepath.Join(dir, "main.go")
	if err := os.WriteFile(path, []byte(src), 0o644); err != nil {
		t.Fatalf("write main.go: %v", err)
	}

	// "foo" declaration is on zero-based line 3, character 1 ("\tfoo").
	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"rename",
		"--file", path,
		"--line", "3",
		"--col", "1",
		"--new", "bar",
		"--lang", "go",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run rename: %v\nstderr: %s", err, stderr.String())
	}

	got := readFile(t, path)
	if strings.Contains(got, "foo") || !strings.Contains(got, "bar := 1") {
		t.Fatalf("rename did not apply:\n%s", got)
	}
	if !parses(t, got) {
		t.Fatalf("renamed file does not parse:\n%s", got)
	}
	if !strings.Contains(stdout.String(), "applied "+path) {
		t.Fatalf("stdout missing applied line: %q", stdout.String())
	}
}
