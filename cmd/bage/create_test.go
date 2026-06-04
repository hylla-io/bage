package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// TestRunCreateNewFile creates a new Go file via --text and asserts the file
// lands with the exact content, still parses, and an applied result line is
// printed to stdout.
func TestRunCreateNewFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "new.go")
	const content = "package main\n\nfunc main() {}\n"

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", path,
		"--text", content,
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run create: %v\nstderr: %s", err, stderr.String())
	}
	if got := readFile(t, path); got != content {
		t.Fatalf("created file = %q, want %q", got, content)
	}
	if !parses(t, readFile(t, path)) {
		t.Fatalf("created file does not parse")
	}
	if !strings.Contains(stdout.String(), "applied "+path) {
		t.Fatalf("stdout missing applied line: %q", stdout.String())
	}
}

// TestRunCreateRejectsExisting verifies the CLI refuses to clobber an existing
// file and leaves its content untouched.
func TestRunCreateRejectsExisting(t *testing.T) {
	path := filepath.Join(t.TempDir(), "exists.txt")
	const original = "DO NOT CLOBBER\n"
	if err := os.WriteFile(path, []byte(original), 0o644); err != nil {
		t.Fatalf("seed: %v", err)
	}

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", path,
		"--text", "new\n",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatalf("run create over existing = nil error, want reject")
	}
	if got := readFile(t, path); got != original {
		t.Fatalf("existing file clobbered: %q", got)
	}
}

// TestRunCreateFromTextFile creates a file using --text-file as the content
// source (the large/multi-line path) and asserts the bytes round-trip.
func TestRunCreateFromTextFile(t *testing.T) {
	dir := t.TempDir()
	srcPath := filepath.Join(dir, "src.txt")
	const content = "line one\nline two\nline three\n"
	if err := os.WriteFile(srcPath, []byte(content), 0o644); err != nil {
		t.Fatalf("seed src: %v", err)
	}
	dest := filepath.Join(dir, "dest.txt")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", dest,
		"--text-file", srcPath,
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run create --text-file: %v\nstderr: %s", err, stderr.String())
	}
	if got := readFile(t, dest); got != content {
		t.Fatalf("dest content = %q, want %q", got, content)
	}
}

// TestRunCreateMkdirParents verifies the CLI creates missing parent directories.
func TestRunCreateMkdirParents(t *testing.T) {
	path := filepath.Join(t.TempDir(), "x", "y", "deep.go")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", path,
		"--text", "package deep\n",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run create deep: %v\nstderr: %s", err, stderr.String())
	}
	if got := readFile(t, path); got != "package deep\n" {
		t.Fatalf("deep content = %q", got)
	}
}

// TestRunCreateRequiresFile verifies the CLI rejects a missing --file flag.
func TestRunCreateRequiresFile(t *testing.T) {
	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{"create", "--text", "x"}, &stdout, &stderr)
	if err == nil {
		t.Fatalf("create without --file = nil error, want usage error")
	}
}
