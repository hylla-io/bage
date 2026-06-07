package main

import (
	"bytes"
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/hylla-io/bage/pkg/bage"
)

// readSrc is a small Go file with two declarations the read tests can address by
// symbol name and assert on in the outline.
const readSrc = "package main\n\nfunc helper() int { return 7 }\n\nfunc main() { _ = helper() }\n"

// TestRunRead reads a Go file with the default text format and asserts the
// human-readable outline carries a known symbol name and a region= anchor.
func TestRunRead(t *testing.T) {
	path := writeNamed(t, "main.go", readSrc)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"read", "--file", path}, &stdout, &stderr); err != nil {
		t.Fatalf("run read: %v\nstderr: %s", err, stderr.String())
	}
	out := stdout.String()
	for _, want := range []string{"helper", "main", "region="} {
		if !strings.Contains(out, want) {
			t.Fatalf("read output missing %q:\n%s", want, out)
		}
	}
}

// TestRunReadFormatJSON reads with --format json and asserts the emitted
// ReadResult JSON carries the blocks array.
func TestRunReadFormatJSON(t *testing.T) {
	path := writeNamed(t, "main.go", readSrc)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"read", "--file", path, "--format", "json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run read --format json: %v\nstderr: %s", err, stderr.String())
	}
	if !strings.Contains(stdout.String(), "\"blocks\"") {
		t.Fatalf("read --format json missing \"blocks\":\n%s", stdout.String())
	}

	var res bage.ReadResult
	if err := json.Unmarshal(stdout.Bytes(), &res); err != nil {
		t.Fatalf("read --format json not parseable: %v\nout: %s", err, stdout.String())
	}
	if len(res.Blocks) == 0 {
		t.Fatal("read --format json emitted no blocks for a Go file with decls")
	}
}

// TestRunReadFormatTOON reads with --format toon and asserts the emitted output
// is non-empty.
func TestRunReadFormatTOON(t *testing.T) {
	path := writeNamed(t, "main.go", readSrc)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"read", "--file", path, "--format", "toon"}, &stdout, &stderr); err != nil {
		t.Fatalf("run read --format toon: %v\nstderr: %s", err, stderr.String())
	}
	if strings.TrimSpace(stdout.String()) == "" {
		t.Fatalf("read --format toon emitted empty output")
	}
}

// TestRunReadSymbol reads with --symbol and asserts only the matching block is
// emitted in the JSON result.
func TestRunReadSymbol(t *testing.T) {
	path := writeNamed(t, "main.go", readSrc)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"read", "--file", path, "--symbol", "helper", "--format", "json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run read --symbol: %v\nstderr: %s", err, stderr.String())
	}

	var res bage.ReadResult
	if err := json.Unmarshal(stdout.Bytes(), &res); err != nil {
		t.Fatalf("read --symbol json not parseable: %v\nout: %s", err, stdout.String())
	}
	if len(res.Blocks) != 1 {
		t.Fatalf("read --symbol helper expected exactly 1 block, got %d", len(res.Blocks))
	}
	if res.Blocks[0].Name != "helper" {
		t.Fatalf("read --symbol helper got block %q", res.Blocks[0].Name)
	}
}

// TestRunReadContent reads with --content and asserts the block's source text is
// included in the JSON result.
func TestRunReadContent(t *testing.T) {
	path := writeNamed(t, "main.go", readSrc)

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"read", "--file", path, "--symbol", "helper", "--content", "--format", "json"}, &stdout, &stderr); err != nil {
		t.Fatalf("run read --content: %v\nstderr: %s", err, stderr.String())
	}

	var res bage.ReadResult
	if err := json.Unmarshal(stdout.Bytes(), &res); err != nil {
		t.Fatalf("read --content json not parseable: %v\nout: %s", err, stdout.String())
	}
	if len(res.Blocks) != 1 {
		t.Fatalf("read --content expected exactly 1 block, got %d", len(res.Blocks))
	}
	if !strings.Contains(res.Blocks[0].Content, "func helper()") {
		t.Fatalf("read --content block missing source text, got %q", res.Blocks[0].Content)
	}
}

// TestRunReadUsage runs read with no --file and asserts a usage error is printed
// to stderr and a non-nil error is returned.
func TestRunReadUsage(t *testing.T) {
	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{"read"}, &stdout, &stderr)
	if err == nil {
		t.Fatal("read with no --file expected a non-nil error")
	}
	if !strings.Contains(stderr.String(), "--file") {
		t.Fatalf("read usage error missing --file mention:\n%s", stderr.String())
	}
}
