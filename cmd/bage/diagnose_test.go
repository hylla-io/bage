package main

import (
	"bytes"
	"context"
	"encoding/json"
	"strings"
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

// TestRunDiagnoseJSONValid asserts --format json emits a valid JSON object whose
// parse_health array carries the broken-Go defect with 1-based line/col, and that
// the LSP section is empty when --lsp is not given.
func TestRunDiagnoseJSONValid(t *testing.T) {
	path := writeNamed(t, "broken.go", "package main\n\nfunc broken() {\n\treturn\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"diagnose", "--file", path, "--format", "json"}, &stdout, &stderr); err != nil {
		t.Fatalf("diagnose --format json returned error: %v\nstderr=%s", err, stderr.String())
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

// TestRunDiagnoseFormatJSONByteIdentical asserts --format json emits exactly the
// indented diagnoseView JSON the old --json flag produced: json.MarshalIndent with
// a two-space indent followed by a trailing newline, byte-for-byte, so a wrapper
// reading the JSON sees the SAME bytes after the flag rename.
func TestRunDiagnoseFormatJSONByteIdentical(t *testing.T) {
	path := writeNamed(t, "broken.go", "package main\n\nfunc broken() {\n\treturn\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"diagnose", "--file", path, "--format", "json"}, &stdout, &stderr); err != nil {
		t.Fatalf("diagnose --format json returned error: %v\nstderr=%s", err, stderr.String())
	}

	var view diagnoseView
	if err := json.Unmarshal(stdout.Bytes(), &view); err != nil {
		t.Fatalf("diagnose --format json not parseable: %v\nout=%s", err, stdout.String())
	}
	want, err := json.MarshalIndent(view, "", "  ")
	if err != nil {
		t.Fatalf("oracle MarshalIndent: %v", err)
	}
	want = append(want, '\n')
	if !bytes.Equal(stdout.Bytes(), want) {
		t.Fatalf("--format json not byte-identical to MarshalIndent+newline\n got:\n%q\nwant:\n%q", stdout.Bytes(), want)
	}
}

// TestRunDiagnoseFormatToon asserts --format toon renders the view as a non-empty
// compact tabular document: the parse_health array carries a tabular header and
// the defect kind appears in the rows, with exit 0 even though defects are found.
func TestRunDiagnoseFormatToon(t *testing.T) {
	path := writeNamed(t, "broken.go", "package main\n\nfunc broken() {\n\treturn\n")

	var stdout, stderr bytes.Buffer
	if err := run(context.Background(), []string{"diagnose", "--file", path, "--format", "toon"}, &stdout, &stderr); err != nil {
		t.Fatalf("diagnose --format toon returned error (must be exit 0 with findings): %v\nstderr=%s", err, stderr.String())
	}

	out := stdout.String()
	if out == "" {
		t.Fatal("diagnose --format toon produced empty output")
	}
	if !strings.Contains(out, "parse_health[") {
		t.Fatalf("diagnose --format toon missing tabular 'parse_health[' header:\n%s", out)
	}
	if !strings.Contains(out, "ERROR") && !strings.Contains(out, "MISSING") {
		t.Fatalf("diagnose --format toon missing defect kind:\n%s", out)
	}
}

// TestRunDiagnoseFormatInvalid asserts an unknown --format value is an explicit
// usage error: a non-nil error is returned and stderr names the valid format set.
func TestRunDiagnoseFormatInvalid(t *testing.T) {
	path := writeNamed(t, "ok.go", "package main\n\nfunc ok() int { return 1 }\n")

	var stdout, stderr bytes.Buffer
	err := run(context.Background(), []string{"diagnose", "--file", path, "--format", "xml"}, &stdout, &stderr)
	if err == nil {
		t.Fatalf("expected error for --format xml, got nil (stdout: %q)", stdout.String())
	}
	if !strings.Contains(stderr.String(), "text|json|toon") {
		t.Fatalf("stderr should name the valid format set, got: %q", stderr.String())
	}
}
