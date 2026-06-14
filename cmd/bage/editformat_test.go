package main

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/session"
)

// TestRunApplyFormatJSON asserts apply with --format json emits the editResults
// JSON: a one-element array whose object carries the changed byte range, new line
// range, and recomputed region/file hashes via each EditResult's struct tags.
func TestRunApplyFormatJSON(t *testing.T) {
	const src = "package main\n\nfunc main() {\n\tfoo := 1\n\t_ = foo\n}\n"
	path := writeTemp(t, src)

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
		"--format", "json",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run apply --format json: %v\nstderr: %s", err, stderr.String())
	}

	var results []region.EditResult
	if jerr := json.Unmarshal(stdout.Bytes(), &results); jerr != nil {
		t.Fatalf("apply --format json not parseable: %v\nout: %s", jerr, stdout.String())
	}
	if len(results) != 1 {
		t.Fatalf("apply --format json results = %d, want 1:\n%s", len(results), stdout.String())
	}
	if results[0].Path != path {
		t.Fatalf("result path = %q, want %q", results[0].Path, path)
	}
	if results[0].NewFileRawHash == "" {
		t.Fatalf("result missing recomputed file raw hash: %+v", results[0])
	}
}

// TestRunApplyFormatToon asserts apply with --format toon emits a non-empty TOON
// document for the editResults slice.
func TestRunApplyFormatToon(t *testing.T) {
	const src = "package main\n\nfunc main() {\n\tfoo := 1\n\t_ = foo\n}\n"
	path := writeTemp(t, src)

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
		"--format", "toon",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run apply --format toon: %v\nstderr: %s", err, stderr.String())
	}
	if stdout.Len() == 0 {
		t.Fatal("apply --format toon produced empty output")
	}
}

// TestRunApplyErrorFormatJSON asserts a conflict reject under --format json emits
// the ErrorEnvelope JSON ({"kind":"conflict",...}) to stderr, returns a non-nil
// error, and leaves the file untouched (reject-not-corrupt).
func TestRunApplyErrorFormatJSON(t *testing.T) {
	const src = "package main\n\nfunc main() {\n\tfoo := 1\n\t_ = foo\n}\n"
	path := writeTemp(t, src)

	bogus := "deadbeefdeadbeef"

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"apply",
		"--file", path,
		"--start", "0",
		"--end", "4",
		"--region-hash", bogus,
		"--text", "X",
		"--lang", "go",
		"--format", "json",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatal("expected conflict error, got nil")
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("file mutated on conflict: %q", got)
	}

	var env session.ErrorEnvelope
	if jerr := json.Unmarshal(stderr.Bytes(), &env); jerr != nil {
		t.Fatalf("apply error --format json not parseable: %v\nstderr: %s", jerr, stderr.String())
	}
	if env.Kind != session.KindConflict {
		t.Fatalf("error envelope kind = %q, want %q\nstderr: %s", env.Kind, session.KindConflict, stderr.String())
	}
}

// TestRunCreateFormatJSON asserts create with --format json emits the editResults
// JSON for the single whole-file result.
func TestRunCreateFormatJSON(t *testing.T) {
	path := filepath.Join(t.TempDir(), "new.go")
	const content = "package main\n\nfunc main() {}\n"

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", path,
		"--text", content,
		"--format", "json",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run create --format json: %v\nstderr: %s", err, stderr.String())
	}

	var results []region.EditResult
	if jerr := json.Unmarshal(stdout.Bytes(), &results); jerr != nil {
		t.Fatalf("create --format json not parseable: %v\nout: %s", jerr, stdout.String())
	}
	if len(results) != 1 {
		t.Fatalf("create --format json results = %d, want 1:\n%s", len(results), stdout.String())
	}
	if results[0].Path != path {
		t.Fatalf("result path = %q, want %q", results[0].Path, path)
	}
}

// TestRunCreateFormatToon asserts create with --format toon emits a non-empty
// TOON document for the single-result editResults slice.
func TestRunCreateFormatToon(t *testing.T) {
	path := filepath.Join(t.TempDir(), "new.go")
	const content = "package main\n\nfunc main() {}\n"

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", path,
		"--text", content,
		"--format", "toon",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run create --format toon: %v\nstderr: %s", err, stderr.String())
	}
	if stdout.Len() == 0 {
		t.Fatal("create --format toon produced empty output")
	}
}

// TestRunCreateErrorFormatJSON asserts a create-over-existing reject under
// --format json emits the ErrorEnvelope JSON ({"kind":"exists",...}) to stderr,
// returns a non-nil error, and leaves the existing file untouched.
func TestRunCreateErrorFormatJSON(t *testing.T) {
	path := writeNamed(t, "exists.go", "package main\n")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"create",
		"--file", path,
		"--text", "package other\n",
		"--format", "json",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatal("expected exists error, got nil")
	}
	if got := readFile(t, path); got != "package main\n" {
		t.Fatalf("existing file clobbered: %q", got)
	}

	var env session.ErrorEnvelope
	if jerr := json.Unmarshal(stderr.Bytes(), &env); jerr != nil {
		t.Fatalf("create error --format json not parseable: %v\nstderr: %s", jerr, stderr.String())
	}
	if env.Kind != session.KindExists {
		t.Fatalf("error envelope kind = %q, want %q\nstderr: %s", env.Kind, session.KindExists, stderr.String())
	}
}

// TestRunDeleteFormatJSON asserts delete with --format json emits the
// DeleteResult JSON (flat snake_case path + raw_hash) for the unlinked file.
func TestRunDeleteFormatJSON(t *testing.T) {
	const src = "package main\n\nfunc main() {}\n"
	path := writeTemp(t, src)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"delete", "--file", path, "--format", "json",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run delete --format json: %v\nstderr: %s", err, stderr.String())
	}

	var res session.DeleteResult
	if jerr := json.Unmarshal(stdout.Bytes(), &res); jerr != nil {
		t.Fatalf("delete --format json not parseable: %v\nout: %s", jerr, stdout.String())
	}
	if res.Path != path {
		t.Fatalf("result path = %q, want %q", res.Path, path)
	}
	if res.RawHash == "" {
		t.Fatalf("result missing confirmed raw hash: %+v", res)
	}
}

// TestRunDeleteFormatToon asserts delete with --format toon emits a non-empty
// TOON document for the DeleteResult.
func TestRunDeleteFormatToon(t *testing.T) {
	path := writeTemp(t, "package main\n\nfunc main() {}\n")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"delete", "--file", path, "--format", "toon",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run delete --format toon: %v\nstderr: %s", err, stderr.String())
	}
	if stdout.Len() == 0 {
		t.Fatal("delete --format toon produced empty output")
	}
}

// TestRunDeleteErrorFormatJSON asserts a raw_hash-drift reject under --format
// json emits the ErrorEnvelope JSON ({"kind":"drift",...}) to stderr, returns a
// non-nil error, and leaves the file untouched (reject-not-corrupt).
func TestRunDeleteErrorFormatJSON(t *testing.T) {
	const src = "package main\n\nfunc main() {}\n"
	path := writeTemp(t, src)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"delete", "--file", path, "--raw-hash", "deadbeefnotthehash", "--format", "json",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatal("expected drift error, got nil")
	}
	if got := readFile(t, path); got != src {
		t.Fatalf("file mutated on drift reject: %q", got)
	}

	var env session.ErrorEnvelope
	if jerr := json.Unmarshal(stderr.Bytes(), &env); jerr != nil {
		t.Fatalf("delete error --format json not parseable: %v\nstderr: %s", jerr, stderr.String())
	}
	if env.Kind != session.KindDrift {
		t.Fatalf("error envelope kind = %q, want %q\nstderr: %s", env.Kind, session.KindDrift, stderr.String())
	}
}

// TestRunMoveFormatJSON asserts move with --format json emits the MoveResult
// JSON: the removed source path plus the destination create-result EditResult.
func TestRunMoveFormatJSON(t *testing.T) {
	dir := t.TempDir()
	const src = "package main\n\nfunc main() {}\n"
	from := filepath.Join(dir, "from.go")
	if err := os.WriteFile(from, []byte(src), 0o644); err != nil {
		t.Fatalf("seed source: %v", err)
	}
	to := filepath.Join(dir, "to.go")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"move", "--from", from, "--to", to, "--format", "json",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run move --format json: %v\nstderr: %s", err, stderr.String())
	}

	var res session.MoveResult
	if jerr := json.Unmarshal(stdout.Bytes(), &res); jerr != nil {
		t.Fatalf("move --format json not parseable: %v\nout: %s", jerr, stdout.String())
	}
	if res.From != from {
		t.Fatalf("result from = %q, want %q", res.From, from)
	}
	if res.Dest.Path != to {
		t.Fatalf("result dest path = %q, want %q", res.Dest.Path, to)
	}
	if res.Dest.NewFileRawHash == "" {
		t.Fatalf("result missing destination raw hash: %+v", res)
	}
}

// TestRunMoveErrorFormatJSON asserts a source raw_hash-drift reject under
// --format json emits the ErrorEnvelope JSON ({"kind":"drift",...}) to stderr,
// returns a non-nil error, and leaves the source untouched with no destination.
func TestRunMoveErrorFormatJSON(t *testing.T) {
	dir := t.TempDir()
	const src = "package main\n\nfunc main() {}\n"
	from := filepath.Join(dir, "from.go")
	if err := os.WriteFile(from, []byte(src), 0o644); err != nil {
		t.Fatalf("seed source: %v", err)
	}
	to := filepath.Join(dir, "to.go")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"move", "--from", from, "--to", to, "--raw-hash", "deadbeefnotthehash", "--format", "json",
	}, &stdout, &stderr)
	if err == nil {
		t.Fatal("expected drift error, got nil")
	}
	if got := readFile(t, from); got != src {
		t.Fatalf("source mutated on drift reject: %q", got)
	}
	if _, statErr := os.Stat(to); !os.IsNotExist(statErr) {
		t.Fatalf("drift-rejected move created a destination: stat err = %v", statErr)
	}

	var env session.ErrorEnvelope
	if jerr := json.Unmarshal(stderr.Bytes(), &env); jerr != nil {
		t.Fatalf("move error --format json not parseable: %v\nstderr: %s", jerr, stderr.String())
	}
	if env.Kind != session.KindDrift {
		t.Fatalf("error envelope kind = %q, want %q\nstderr: %s", env.Kind, session.KindDrift, stderr.String())
	}
}

// renameSetup writes a tiny module to a fresh temp dir and returns the main.go
// path, skipping the test when gopls is not on PATH so the suite stays hermetic.
func renameSetup(t *testing.T) string {
	t.Helper()
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
	return path
}

// TestRunRenameFormatJSON drives a live gopls rename with --format json and
// asserts the editResults slice (one per affected file) parses as JSON.
func TestRunRenameFormatJSON(t *testing.T) {
	path := renameSetup(t)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"rename",
		"--file", path,
		"--line", "3",
		"--col", "1",
		"--new", "bar",
		"--lang", "go",
		"--format", "json",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run rename --format json: %v\nstderr: %s", err, stderr.String())
	}

	var results []region.EditResult
	if jerr := json.Unmarshal(stdout.Bytes(), &results); jerr != nil {
		t.Fatalf("rename --format json not parseable: %v\nout: %s", jerr, stdout.String())
	}
	if len(results) == 0 {
		t.Fatalf("rename --format json emitted no results:\n%s", stdout.String())
	}
	if got := readFile(t, path); strings.Contains(got, "foo") || !strings.Contains(got, "bar := 1") {
		t.Fatalf("rename did not apply:\n%s", got)
	}
}

// TestRunRenameFormatToon drives a live gopls rename with --format toon and
// asserts a non-empty TOON document is emitted.
func TestRunRenameFormatToon(t *testing.T) {
	path := renameSetup(t)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"rename",
		"--file", path,
		"--line", "3",
		"--col", "1",
		"--new", "bar",
		"--lang", "go",
		"--format", "toon",
	}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("run rename --format toon: %v\nstderr: %s", err, stderr.String())
	}
	if stdout.Len() == 0 {
		t.Fatal("rename --format toon produced empty output")
	}
}

// TestRunRenameErrorFormatJSON drives a live gopls rename to an invalid Go
// identifier so the server rejects before producing a WorkspaceEdit; under
// --format json the resulting domain error is routed through the ErrorEnvelope, so
// stderr parses as the kind envelope. If gopls happens to accept the name the
// envelope assertion is skipped (it stays hermetic across gopls versions).
func TestRunRenameErrorFormatJSON(t *testing.T) {
	path := renameSetup(t)

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{
		"rename",
		"--file", path,
		"--line", "3",
		"--col", "1",
		"--new", "123bad",
		"--lang", "go",
		"--format", "json",
	}, &stdout, &stderr)
	if err == nil {
		t.Skip("gopls accepted an invalid identifier; skipping envelope assertion")
	}

	var env session.ErrorEnvelope
	if jerr := json.Unmarshal(stderr.Bytes(), &env); jerr != nil {
		t.Fatalf("rename error --format json not parseable: %v\nstderr: %s", jerr, stderr.String())
	}
	if env.Kind == "" {
		t.Fatalf("error envelope missing kind\nstderr: %s", stderr.String())
	}
}
