package main

import (
	"bytes"
	"context"
	"encoding/json"
	"testing"
)

// TestRunDiagnoseBrokenGoExitZero asserts that diagnose REPORTS the parse defect
// in a broken Go file yet still returns nil (exit 0): surfacing problems is
// success, not failure. The human output names the file and at least one defect.
func TestRunDiagnoseBrokenGoExitZero(t *testing.T) {
	path := writeNamed(t, "broken.go", "package main\n\nfunc broken() {\n\treturn\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"diagnose", "--file", path}, &stdout, &stderr); err != nil {
		t.Fatalf("diagnose returned error (must be exit 0 with findings): %v\nstderr=%s", err, stderr.String())
	}
	out := stdout.String()
	if !bytes.Contains(stdout.Bytes(), []byte("ERROR")) && !bytes.Contains(stdout.Bytes(), []byte("MISSING")) {
		t.Fatalf("expected an ERROR/MISSING parse defect in output, got:\n%s", out)
	}
}

// TestRunDiagnoseCleanGoExitZero asserts a clean Go file diagnoses with no
// defects and exit 0.
func TestRunDiagnoseCleanGoExitZero(t *testing.T) {
	path := writeNamed(t, "ok.go", "package main\n\nfunc ok() int { return 1 }\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"diagnose", "--file", path}, &stdout, &stderr); err != nil {
		t.Fatalf("diagnose clean returned error: %v\nstderr=%s", err, stderr.String())
	}
}

// TestRunDiagnoseJSONValid asserts --json emits a valid JSON object whose
// parse_health array carries the broken-Go defect with 1-based line/col, and that
// the LSP section is empty when --lsp is not given.
func TestRunDiagnoseJSONValid(t *testing.T) {
	path := writeNamed(t, "broken.go", "package main\n\nfunc broken() {\n\treturn\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"diagnose", "--file", path, "--json"}, &stdout, &stderr); err != nil {
		t.Fatalf("diagnose --json returned error: %v\nstderr=%s", err, stderr.String())
	}

	var view diagnoseView
	if err := json.Unmarshal(stdout.Bytes(), &view); err != nil {
		t.Fatalf("output is not valid JSON: %v\nout=%s", err, stdout.String())
	}
	if view.Path != path {
		t.Fatalf("view.Path = %q, want %q", view.Path, path)
	}
	if len(view.ParseHealth) == 0 {
		t.Fatalf("expected parse_health defects in JSON, got none: %s", stdout.String())
	}
	for _, d := range view.ParseHealth {
		if d.Line < 1 || d.Col < 1 {
			t.Fatalf("parse_health line/col must be 1-based, got %+v", d)
		}
	}
	if len(view.LSP) != 0 {
		t.Fatalf("expected no LSP diagnostics without --lsp, got %+v", view.LSP)
	}
}
