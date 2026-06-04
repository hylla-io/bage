package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"go.lsp.dev/uri"

	"github.com/hylla-io/bage/internal/lsp"
	"github.com/hylla-io/bage/pkg/bage"
)

// parseDefectView is one parse-health defect in the diagnose read view: an
// ERROR/MISSING node with 1-based line/col and a half-open byte range. It mirrors
// bage.ParseDefect with JSON tags for --json.
type parseDefectView struct {
	// Kind is "ERROR" or "MISSING".
	Kind string `json:"kind"`
	// Line is the 1-based line of the defect.
	Line int `json:"line"`
	// Col is the 1-based column of the defect.
	Col int `json:"col"`
	// StartByte is the inclusive start byte offset.
	StartByte int `json:"start_byte"`
	// EndByte is the exclusive end byte offset.
	EndByte int `json:"end_byte"`
}

// lspDiagnosticView is one LSP-reported diagnostic in the diagnose read view:
// severity, 1-based range, message, and source. It mirrors lsp.Diagnostic with
// JSON tags for --json.
type lspDiagnosticView struct {
	// Severity is the human label ("Error", "Warning", "Information", "Hint").
	Severity string `json:"severity"`
	// Source names the diagnostic's origin (may be "").
	Source string `json:"source"`
	// Message is the diagnostic text.
	Message string `json:"message"`
	// StartLine is the 1-based start line.
	StartLine int `json:"start_line"`
	// StartCol is the 1-based start column.
	StartCol int `json:"start_col"`
	// EndLine is the 1-based end line.
	EndLine int `json:"end_line"`
	// EndCol is the 1-based end column.
	EndCol int `json:"end_col"`
}

// diagnoseView is the structured read view emitted by diagnose: the file, its
// resolved language, the always-present parse-health defects, and (only when
// --lsp is given) the language server's diagnostics. diagnose SURFACES problems;
// it never fixes them, and reporting defects is success (exit 0), so this view is
// emitted on both clean and defect-bearing files (SPEC §10.5).
type diagnoseView struct {
	// Path is the file that was diagnosed.
	Path string `json:"path"`
	// Lang is the canonical language name selected for Path (never "unknown").
	Lang string `json:"lang"`
	// ParseHealth lists every ERROR/MISSING node from the LSP-free parse tier.
	ParseHealth []parseDefectView `json:"parse_health"`
	// LSP lists the language server's diagnostics; empty unless --lsp was given.
	LSP []lspDiagnosticView `json:"lsp"`
}

// runDiagnose parses the diagnose flags and SURFACES problems in --file from two
// sources (SPEC §10.5): (1) parse-health — always, LSP-free — every ERROR/MISSING
// node from the shared tree-sitter parser, the same signal the edit parse-floor
// uses; and (2) LSP diagnostics — only when --lsp names a server — collected from
// the server's textDocument/publishDiagnostics after didOpen. diagnose does NOT
// fix anything; the host/agent decides. Reporting defects is SUCCESS: exit code is
// 0 even when diagnostics are found, with non-zero reserved for usage/IO/LSP-start
// errors. Default output is human-readable; --json emits the structured view.
func runDiagnose(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("diagnose", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file   = fs.String("file", "", "path of the file to diagnose (required)")
		lspCmd = fs.String("lsp", "", "optional LSP server command; when given, also collect the server's published diagnostics")
		asJSON = fs.Bool("json", false, "emit structured JSON instead of human-readable text")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage diagnose: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage diagnose: --file is required")
		return errors.New("bage diagnose: --file is required")
	}

	opened, err := bage.OpenFile(ctx, *file)
	if err != nil {
		fmt.Fprintf(stderr, "bage diagnose: %v\n", err)
		return fmt.Errorf("bage diagnose: %w", err)
	}
	defer opened.Close()

	view := diagnoseView{
		Path:        *file,
		Lang:        opened.Lang.String(),
		ParseHealth: make([]parseDefectView, 0),
		LSP:         make([]lspDiagnosticView, 0),
	}
	for _, d := range bage.ParseHealth(opened) {
		view.ParseHealth = append(view.ParseHealth, parseDefectView{
			Kind:      d.Kind,
			Line:      d.Line,
			Col:       d.Col,
			StartByte: d.StartByte,
			EndByte:   d.EndByte,
		})
	}

	// The LSP tier is opt-in: an LSP-start/connect failure is a real (non-zero)
	// error since the caller explicitly asked for it, but finding diagnostics is
	// success.
	if strings.TrimSpace(*lspCmd) != "" {
		diags, lerr := collectLSPDiagnostics(ctx, *file, *lspCmd)
		if lerr != nil {
			fmt.Fprintf(stderr, "bage diagnose: %v\n", lerr)
			return fmt.Errorf("bage diagnose: %w", lerr)
		}
		for _, d := range diags {
			view.LSP = append(view.LSP, lspDiagnosticView{
				Severity:  d.Severity,
				Source:    d.Source,
				Message:   d.Message,
				StartLine: d.StartLine,
				StartCol:  d.StartCol,
				EndLine:   d.EndLine,
				EndCol:    d.EndCol,
			})
		}
	}

	if *asJSON {
		return printDiagnoseJSON(stdout, stderr, view)
	}
	printDiagnoseText(stdout, view)
	return nil
}

// collectLSPDiagnostics starts the named LSP server, initializes it rooted at the
// file's directory, opens the file, and collects the server's published
// diagnostics. The file is read here (not by OpenFile, which keeps only the tree)
// because didOpen must send the authoritative current text. Any LSP-stage failure
// is wrapped and returned — diagnose treats an opted-in LSP that cannot start as a
// hard error, distinct from a server that simply reports problems.
func collectLSPDiagnostics(ctx context.Context, file, lspCmd string) ([]lsp.Diagnostic, error) {
	command := strings.Fields(lspCmd)
	if len(command) == 0 {
		return nil, errors.New("--lsp must name a server command")
	}

	abs, err := filepath.Abs(file)
	if err != nil {
		return nil, fmt.Errorf("resolve %q: %w", file, err)
	}
	content, err := os.ReadFile(abs)
	if err != nil {
		return nil, fmt.Errorf("read %q: %w", abs, err)
	}

	client, err := lsp.NewClient(ctx, command)
	if err != nil {
		return nil, fmt.Errorf("start lsp %q: %w", command[0], err)
	}
	defer func() { _ = client.Close(ctx) }()

	if err := client.Initialize(ctx, uri.File(filepath.Dir(abs))); err != nil {
		return nil, fmt.Errorf("initialize: %w", err)
	}
	diags, err := client.Diagnostics(ctx, abs, string(content))
	if err != nil {
		return nil, fmt.Errorf("collect diagnostics: %w", err)
	}
	return diags, nil
}

// printDiagnoseJSON writes the diagnoseView as indented JSON.
func printDiagnoseJSON(stdout, stderr io.Writer, view diagnoseView) error {
	b, err := json.MarshalIndent(view, "", "  ")
	if err != nil {
		fmt.Fprintf(stderr, "bage diagnose: marshal json: %v\n", err)
		return fmt.Errorf("bage diagnose: marshal json: %w", err)
	}
	fmt.Fprintln(stdout, string(b))
	return nil
}

// printDiagnoseText writes the human-readable diagnose view: a header line with
// the path, language, and the two source counts, then one line per parse-health
// defect and one per LSP diagnostic. A file with no problems prints just the
// header (counts of 0), which is the explicit clean signal.
func printDiagnoseText(stdout io.Writer, view diagnoseView) {
	fmt.Fprintf(stdout, "%s lang=%s parse_health=%d lsp=%d\n",
		view.Path, view.Lang, len(view.ParseHealth), len(view.LSP))
	for _, d := range view.ParseHealth {
		fmt.Fprintf(stdout,
			"  parse %s line %d col %d bytes [%d:%d]\n",
			d.Kind, d.Line, d.Col, d.StartByte, d.EndByte)
	}
	for _, d := range view.LSP {
		source := d.Source
		if source == "" {
			source = "-"
		}
		fmt.Fprintf(stdout,
			"  lsp %s [%s] line %d col %d: %s\n",
			d.Severity, source, d.StartLine, d.StartCol, d.Message)
	}
}
