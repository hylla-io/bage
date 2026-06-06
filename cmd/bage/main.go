// Command bage is the standalone entrypoint for the Båge round-trip file
// editor (SPEC §6 standalone mode): files + LSP, no graph, sharing the same
// region/session edit engine as integrated mode.
//
// main stays thin — it parses os.Args and delegates to run, the testable core
// that wires the treesitter parser, an xxHash hasher, and optional format/lint
// commands into a session and applies region-anchored edits (SPEC §8).
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"go.lsp.dev/uri"

	"github.com/hylla-io/bage/internal/format"
	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/locator"
	"github.com/hylla-io/bage/internal/lsp"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/session"
)

// usage is the top-level usage string printed when no subcommand, an unknown
// subcommand, or a usage error is encountered.
const usage = `bage — round-trip file editor (standalone)

usage:
  bage apply  --file F (--line L | --lines L1-L2 | --start S --end E) --text T [--region-hash H] [--lang go] [--fmt CMD] [--lint CMD]
  bage create --file F (--text T | --text-file P) [--lang go] [--fmt CMD] [--lint CMD]
  bage delete --file F [--raw-hash H]
  bage move   --from F --to G [--raw-hash H]
  bage rename --file F --line L --col C --new NAME [--lsp gopls] [--lang go]
  bage read   --file F [--line L | --lines L1-L2 | --start S --end E] [--symbol NAME] [--content] [--format text|json|toon]
  bage show   --file F [--format text|json|toon]
  bage diagnose --file F [--lsp CMD] [--format text|json|toon]

show is the READ view: it parses F with the shared parser and emits, for every
addressable block (the Outline), its kind, name, 1-based line range, byte range,
and region_hash — the SAME region_hash bage apply --region-hash verifies+accepts
for that exact byte range (the round-trip anchor an agent echoes back). It also
emits the file's raw_hash + norm_hash. A grammar-backed file lists its
declarations; a text-fallback file lists its line-blocks; an empty file yields an
empty outline plus the file hashes. Default output is human-readable; --format
selects text, json, or toon. show is strictly READ-ONLY — it writes nothing to disk.

create writes a NEW file F from --text (or --text-file). Its anchor is
non-existence: if F already exists the create HARD-REJECTS and nothing is
clobbered. Missing parent directories are created. The new content must parse
under its language (auto-detected from F unless --lang is given); the optional
formatter and linter run on the staged bytes first. The create is WAL-logged so
a crash unlinks the half-created file. On success the whole-file EditResult
(new raw/norm hashes, line range) is printed.

delete unlinks an existing file F. Its anchor is the expected raw_hash drift
gate: with --raw-hash the live file must still hash to H or the delete
HARD-REJECTS (never discarding bytes the caller did not see); without --raw-hash
the expected hash is computed from the live bytes (delete-current — no drift
protection). The FULL prior bytes are captured in the WAL BEFORE the unlink, so a
crash restores the file. A missing F rejects. On success a confirmation line
(path + confirmed raw hash) is printed; on any reject nothing is unlinked.

move relocates file F to G, preserving F's bytes unchanged at G (relocate-only;
no import fixup in this slice). The SOURCE anchor is the expected raw_hash drift
gate: with --raw-hash the live F must still hash to H or the move HARD-REJECTS
(never relocating bytes the caller did not see); without --raw-hash the expected
hash is computed from the live bytes (relocate-current — no drift protection). The
DESTINATION anchor is non-existence: if G already exists the move HARD-REJECTS and
G is never clobbered. Missing parent directories of G are created. The move is
WAL-logged with F's bytes so a crash converges to fully-moved without losing the
source. A missing F rejects. On success a confirmation line (from + to + confirmed
raw hash) is printed; on any reject nothing moves.

apply replaces a region of F with text. The region is addressed by a single
line (--line), a 1-based inclusive line range (--lines L1-L2), or a raw byte
range (--start/--end). An optional --region-hash anchors the region by content
so a benign concurrent shift re-resolves and a real conflict hard-rejects. The
optional formatter and linter run on the staged bytes; the result must still
parse. On drift, conflict, lint, or parse failure nothing is written. On success
the per-edit EditResult (changed byte range, recomputed hashes, new line range)
is printed.

rename drives an LSP server (default gopls) to rename the symbol at the
zero-based (line, col) UTF-16 position in F to NAME, converts the resulting
WorkspaceEdit into region-anchored edits (each grounded against the file's live
bytes via a computed region_hash), and applies every affected file atomically
via the same Prepare/Commit engine. On drift, conflict, or parse failure nothing
is written.

diagnose SURFACES problems in F without fixing them — the host/agent decides what
to do. It reports from two sources: (1) parse-health, ALWAYS and LSP-free — every
ERROR/MISSING node the shared tree-sitter parser finds (the same signal the edit
parse-floor uses), with 1-based line/col + byte range; a clean parse reports none,
and the grammar-free text fallback always parses so it never reports a defect.
(2) LSP diagnostics, only when --lsp names a server — diagnose opens F in that
server (textDocument/didOpen) and collects its published diagnostics, each with
severity, 1-based range, message, and source. Reporting problems is SUCCESS: exit
code is 0 even WITH findings; non-zero is reserved for usage/IO/LSP-start errors.
Default output is human-readable; --format selects text, json, or toon.`

// main wires the process entrypoint to run and maps any error to exit code 1.
func main() {
	if err := run(context.Background(), os.Args[1:], os.Stdout, os.Stderr); err != nil {
		os.Exit(1)
	}
}

// run is the testable CLI core. It dispatches on the first argument to a
// subcommand handler, writing results to stdout and errors to stderr. It
// returns a non-nil error (already reported to stderr) when the command fails,
// so main can map that to a non-zero exit without re-printing.
func run(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	if len(args) == 0 {
		fmt.Fprintln(stderr, usage)
		return errors.New("bage: no subcommand")
	}

	switch args[0] {
	case "apply":
		return runApply(ctx, args[1:], stdout, stderr)
	case "create":
		return runCreate(ctx, args[1:], stdout, stderr)
	case "delete":
		return runDelete(ctx, args[1:], stdout, stderr)
	case "move":
		return runMove(ctx, args[1:], stdout, stderr)
	case "rename":
		return runRename(ctx, args[1:], stdout, stderr)
	case "read":
		return runRead(ctx, args[1:], stdout, stderr)
	case "show":
		return runShow(ctx, args[1:], stdout, stderr)
	case "diagnose":
		return runDiagnose(ctx, args[1:], stdout, stderr)
	default:
		fmt.Fprintln(stderr, usage)
		return fmt.Errorf("bage: unknown subcommand %q", args[0])
	}
}

// runApply parses the apply flags, builds a single region-anchored Edit plus the
// file's drift anchor, and drives a session Prepare/Commit. On any error it
// prints a clear message to stderr and returns the error; on success it prints
// each resulting EditResult to stdout. Nothing is written on drift, conflict,
// lint, or parse failure.
func runApply(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("apply", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file       = fs.String("file", "", "path of the file to edit (required)")
		line       = fs.Int("line", -1, "1-based line to replace (mutually exclusive with --lines / --start)")
		lines      = fs.String("lines", "", "1-based inclusive line range L1-L2 to replace")
		start      = fs.Int("start", -1, "inclusive start byte of the region to replace")
		end        = fs.Int("end", -1, "exclusive end byte of the region to replace")
		text       = fs.String("text", "", "replacement text for the region")
		textFile   = fs.String("text-file", "", "read replacement text from this file instead of --text (for large/multi-line edits)")
		regionHash = fs.String("region-hash", "", "optional region_hash anchoring the region by content")
		langStr    = fs.String("lang", "", "source language by canonical name (e.g. go, python, markdown); empty = auto-detect from --file path")
		fmtCmd     = fs.String("fmt", "", "optional formatter command run on the staged bytes")
		lintCmd    = fs.String("lint", "", "optional linter command run on the staged bytes")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage apply: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage apply: --file is required")
		return errors.New("bage apply: --file is required")
	}

	lang, err := parseLang(*langStr)
	if err != nil {
		fmt.Fprintln(stderr, err.Error())
		return err
	}

	live, err := os.ReadFile(*file)
	if err != nil {
		fmt.Fprintf(stderr, "bage apply: read %q: %v\n", *file, err)
		return fmt.Errorf("bage apply: read %q: %w", *file, err)
	}

	reg, err := applyRegion(*file, live, *line, *lines, *start, *end, *regionHash)
	if err != nil {
		fmt.Fprintln(stderr, err.Error())
		return err
	}

	hasher := hashing.XXHasher{}
	sess := &session.Session{
		Parser:    treesitter.New(),
		Hasher:    hasher,
		Formatter: formatterFor(*fmtCmd),
		Linter:    linterFor(*lintCmd),
		Lang:      lang,
		WALDir:    os.TempDir(),
	}

	newText := *text
	if *textFile != "" {
		b, rerr := os.ReadFile(*textFile)
		if rerr != nil {
			fmt.Fprintf(stderr, "bage apply: read --text-file %q: %v\n", *textFile, rerr)
			return fmt.Errorf("bage apply: read --text-file %q: %w", *textFile, rerr)
		}
		newText = string(b)
	}
	if *line >= 0 || *lines != "" {
		// Line addressing replaces line CONTENT — the trailing newline is
		// structural and preserved by applyRegion — so a trailing newline in
		// --text would double it. Strip one so `--text "x"` and `--text "x\n"`
		// behave identically and never merge or split lines.
		newText = strings.TrimSuffix(newText, "\n")
	}
	edits := []region.Edit{{Region: reg, NewText: newText}}
	anchors := []region.FileAnchor{fileAnchor(*file, live, hasher)}

	plan, err := sess.Prepare(ctx, edits, anchors)
	if err != nil {
		fmt.Fprintf(stderr, "bage apply: %v\n", err)
		return fmt.Errorf("bage apply: prepare: %w", err)
	}
	results, err := sess.Commit(plan)
	if err != nil {
		fmt.Fprintf(stderr, "bage apply: %v\n", err)
		return fmt.Errorf("bage apply: commit: %w", err)
	}

	printResults(stdout, results)
	return nil
}

// applyRegion builds the region-anchored target from the apply flags. Exactly
// one addressing mode must be supplied: a single line (--line), a 1-based
// inclusive line range (--lines), or a raw byte range (--start/--end). Line
// addressing is resolved to a concrete byte range against the live bytes via a
// LineIndex; the optional region_hash is attached unchanged so the resolver can
// verify content and relocate a benign shift.
func applyRegion(file string, live []byte, line int, lines string, start, end int, regionHash string) (region.Region, error) {
	byteMode := start >= 0 || end >= 0
	lineMode := line >= 0 || lines != ""

	switch {
	case byteMode && lineMode:
		return region.Region{}, errors.New("bage apply: choose one of --line/--lines or --start/--end, not both")
	case byteMode:
		if start < 0 || end < 0 {
			return region.Region{}, errors.New("bage apply: --start and --end are both required for byte addressing")
		}
		li := region.NewLineIndex(live)
		return li.FillLineCols(region.Region{
			Path:       file,
			StartByte:  start,
			EndByte:    end,
			RegionHash: regionHash,
		}), nil
	case lineMode:
		startLine, endLine, err := parseLineRange(line, lines)
		if err != nil {
			return region.Region{}, err
		}
		li := region.NewLineIndex(live)
		reg := li.ResolveLines(region.Region{
			Path:       file,
			StartByte:  region.LineSentinel,
			StartLine:  startLine,
			EndLine:    endLine,
			RegionHash: regionHash,
		})
		// A resolved line range spans THROUGH the final line's trailing newline.
		// Exclude that newline so --text replaces the line CONTENT and the line
		// structure survives even when --text has no trailing newline (otherwise
		// `--line 5 --text x` would merge line 5 into line 6). A final line with
		// no trailing newline is left as-is.
		if reg.EndByte > reg.StartByte && reg.EndByte <= len(live) && live[reg.EndByte-1] == '\n' {
			reg.EndByte--
			reg = li.FillLineCols(reg)
		}
		return reg, nil
	default:
		return region.Region{}, errors.New("bage apply: one of --line, --lines, or --start/--end is required")
	}
}

// parseLineRange resolves the single-line / line-range flags to a 1-based
// inclusive [startLine, endLine]. --line and --lines are mutually exclusive;
// --lines must be "L1-L2" with L1 <= L2 and both >= 1.
func parseLineRange(line int, lines string) (startLine, endLine int, err error) {
	if line >= 0 && lines != "" {
		return 0, 0, errors.New("bage apply: choose one of --line or --lines, not both")
	}
	if line >= 0 {
		if line < 1 {
			return 0, 0, errors.New("bage apply: --line must be >= 1")
		}
		return line, line, nil
	}
	lo, hi, ok := strings.Cut(lines, "-")
	if !ok {
		return 0, 0, fmt.Errorf("bage apply: --lines %q must be L1-L2", lines)
	}
	startLine, err = atoiPositive(strings.TrimSpace(lo))
	if err != nil {
		return 0, 0, fmt.Errorf("bage apply: --lines start: %w", err)
	}
	endLine, err = atoiPositive(strings.TrimSpace(hi))
	if err != nil {
		return 0, 0, fmt.Errorf("bage apply: --lines end: %w", err)
	}
	if startLine > endLine {
		return 0, 0, fmt.Errorf("bage apply: --lines %q has start past end", lines)
	}
	return startLine, endLine, nil
}

// runCreate parses the create flags and drives session.CreateFile to bring a
// NEW file into existence (ADR-0004, SPEC §10). The content comes from --text or
// --text-file (mutually exclusive). The non-existence anchor is enforced by the
// engine: a pre-existing F rejects with ErrExists and is never clobbered. The
// language is auto-detected from F unless --lang names one. On any error it
// prints a clear message to stderr and returns it; on success it prints the
// whole-file EditResult to stdout via the same printResults contract as apply.
func runCreate(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("create", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file     = fs.String("file", "", "path of the file to create (required; must not already exist)")
		text     = fs.String("text", "", "full content of the new file")
		textFile = fs.String("text-file", "", "read the new file's content from this file instead of --text (for large/multi-line content)")
		langStr  = fs.String("lang", "", "source language by canonical name (e.g. go, python, markdown); empty = auto-detect from --file path")
		fmtCmd   = fs.String("fmt", "", "optional formatter command run on the staged bytes")
		lintCmd  = fs.String("lint", "", "optional linter command run on the staged bytes")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage create: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage create: --file is required")
		return errors.New("bage create: --file is required")
	}
	if *text != "" && *textFile != "" {
		fmt.Fprintln(stderr, "bage create: choose one of --text or --text-file, not both")
		return errors.New("bage create: --text and --text-file are mutually exclusive")
	}

	lang, err := parseLang(*langStr)
	if err != nil {
		fmt.Fprintln(stderr, err.Error())
		return err
	}

	content := *text
	if *textFile != "" {
		b, rerr := os.ReadFile(*textFile)
		if rerr != nil {
			fmt.Fprintf(stderr, "bage create: read --text-file %q: %v\n", *textFile, rerr)
			return fmt.Errorf("bage create: read --text-file %q: %w", *textFile, rerr)
		}
		content = string(b)
	}

	sess := &session.Session{
		Parser:    treesitter.New(),
		Hasher:    hashing.XXHasher{},
		Formatter: formatterFor(*fmtCmd),
		Linter:    linterFor(*lintCmd),
		Lang:      lang,
		WALDir:    os.TempDir(),
	}

	res, err := sess.CreateFile(ctx, session.Op{
		Kind:    session.OpCreate,
		Path:    *file,
		Content: content,
	})
	if err != nil {
		fmt.Fprintf(stderr, "bage create: %v\n", err)
		return fmt.Errorf("bage create: %w", err)
	}

	printResults(stdout, []region.EditResult{res})
	return nil
}

// runDelete parses the delete flags and drives session.DeleteFile to unlink an
// existing file F (ADR-0004, SPEC §10). The drift gate is the expected raw_hash:
// with --raw-hash the engine enforces it (a live mismatch HARD-REJECTS and
// nothing is unlinked); without --raw-hash the expected hash is computed from the
// live bytes (delete-current — documented as no drift protection). The FULL prior
// bytes are WAL-captured before the unlink so a crash restores the file. A missing
// F rejects. On any error it prints a clear message to stderr and returns it; on
// success it prints a concise confirmation line (path + confirmed raw hash).
func runDelete(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("delete", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file    = fs.String("file", "", "path of the file to delete (required; must exist)")
		rawHash = fs.String("raw-hash", "", "expected raw content hash of the live file (drift gate); empty = compute from live bytes (delete-current, no drift protection)")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage delete: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage delete: --file is required")
		return errors.New("bage delete: --file is required")
	}

	hasher := hashing.XXHasher{}

	// Resolve the expected raw_hash: an explicit --raw-hash is the caller's drift
	// anchor; an empty one means delete-current, so we read the live bytes and
	// compute the anchor from them (no drift protection — documented). A read
	// failure here (e.g. a missing file) rejects before anything is unlinked.
	expected := *rawHash
	if expected == "" {
		live, err := os.ReadFile(*file)
		if err != nil {
			fmt.Fprintf(stderr, "bage delete: read %q: %v\n", *file, err)
			return fmt.Errorf("bage delete: read %q: %w", *file, err)
		}
		expected = hashing.RawHash(hasher, live)
	}

	sess := &session.Session{
		Parser: treesitter.New(),
		Hasher: hasher,
		WALDir: os.TempDir(),
	}

	res, err := sess.DeleteFile(ctx, session.Op{
		Kind:            session.OpDelete,
		Path:            *file,
		ExpectedRawHash: expected,
	})
	if err != nil {
		fmt.Fprintf(stderr, "bage delete: %v\n", err)
		return fmt.Errorf("bage delete: %w", err)
	}

	fmt.Fprintf(stdout, "deleted %s raw=%s\n", res.Path, res.RawHash)
	return nil
}

// runMove parses the move flags and drives session.MoveFile to relocate file
// --from to --to, preserving the source bytes unchanged (ADR-0004, SPEC §10;
// relocate-only, no import fixup in this slice). The SOURCE drift gate is the
// expected raw_hash: with --raw-hash the engine enforces it (a live mismatch
// HARD-REJECTS and nothing moves); without --raw-hash the expected hash is computed
// from the live bytes (relocate-current — documented as no drift protection). The
// DESTINATION non-existence anchor is enforced by the engine: a pre-existing --to
// rejects with ErrExists and is never clobbered. On any error it prints a clear
// message to stderr and returns it; on success it prints a concise confirmation
// line (from + to + confirmed raw hash). Nothing moves on any reject.
func runMove(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("move", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		from    = fs.String("from", "", "source path to move (required; must exist)")
		to      = fs.String("to", "", "destination path (required; must not already exist)")
		rawHash = fs.String("raw-hash", "", "expected raw content hash of the live source (drift gate); empty = compute from live bytes (relocate-current, no drift protection)")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage move: %w", err)
	}

	if *from == "" {
		fmt.Fprintln(stderr, "bage move: --from is required")
		return errors.New("bage move: --from is required")
	}
	if *to == "" {
		fmt.Fprintln(stderr, "bage move: --to is required")
		return errors.New("bage move: --to is required")
	}

	hasher := hashing.XXHasher{}

	// Resolve the expected source raw_hash: an explicit --raw-hash is the caller's
	// drift anchor; an empty one means relocate-current, so we read the live bytes
	// and compute the anchor from them (no drift protection — documented). A read
	// failure here (e.g. a missing source) rejects before anything moves.
	expected := *rawHash
	if expected == "" {
		live, err := os.ReadFile(*from)
		if err != nil {
			fmt.Fprintf(stderr, "bage move: read %q: %v\n", *from, err)
			return fmt.Errorf("bage move: read %q: %w", *from, err)
		}
		expected = hashing.RawHash(hasher, live)
	}

	sess := &session.Session{
		Parser: treesitter.New(),
		Hasher: hasher,
		WALDir: os.TempDir(),
	}

	res, err := sess.MoveFile(ctx, session.Op{
		Kind:            session.OpMove,
		Path:            *from,
		To:              *to,
		ExpectedRawHash: expected,
	})
	if err != nil {
		fmt.Fprintf(stderr, "bage move: %v\n", err)
		return fmt.Errorf("bage move: %w", err)
	}

	fmt.Fprintf(stdout, "moved %s -> %s raw=%s\n", res.From, res.Dest.Path, res.Dest.NewFileRawHash)
	return nil
}

// runRename parses the rename flags, drives an LSP rename, converts the server's
// WorkspaceEdit into region-anchored edits grounded against each file's live
// bytes (per-edit region_hash computed from the targeted byte range), and applies
// them all atomically via a single session Prepare/Commit. On any error it prints
// a clear message to stderr and returns it; on success it prints each affected
// file's EditResult(s) to stdout. Nothing is written on drift, parse, or LSP
// failure.
func runRename(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("rename", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file    = fs.String("file", "", "path of the file containing the symbol (required)")
		line    = fs.Int("line", -1, "zero-based line of the symbol (required)")
		col     = fs.Int("col", -1, "zero-based UTF-16 column of the symbol (required)")
		newName = fs.String("new", "", "new name for the symbol (required)")
		lspCmd  = fs.String("lsp", "gopls", "LSP server command to drive the rename")
		langStr = fs.String("lang", "go", "source language (currently only 'go')")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage rename: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage rename: --file is required")
		return errors.New("bage rename: --file is required")
	}
	if *line < 0 || *col < 0 {
		fmt.Fprintln(stderr, "bage rename: --line and --col are required and must be >= 0")
		return errors.New("bage rename: --line/--col required")
	}
	if *newName == "" {
		fmt.Fprintln(stderr, "bage rename: --new is required")
		return errors.New("bage rename: --new is required")
	}

	lang, err := parseLang(*langStr)
	if err != nil {
		fmt.Fprintln(stderr, err.Error())
		return err
	}

	command := strings.Fields(*lspCmd)
	if len(command) == 0 {
		fmt.Fprintln(stderr, "bage rename: --lsp must name a server command")
		return errors.New("bage rename: empty --lsp")
	}

	abs, err := filepath.Abs(*file)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: resolve %q: %v\n", *file, err)
		return fmt.Errorf("bage rename: resolve %q: %w", *file, err)
	}

	content, err := os.ReadFile(abs)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: read %q: %v\n", abs, err)
		return fmt.Errorf("bage rename: read %q: %w", abs, err)
	}

	client, err := lsp.NewClient(ctx, command)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: start lsp %q: %v\n", command[0], err)
		return fmt.Errorf("bage rename: start lsp: %w", err)
	}
	defer func() { _ = client.Close(ctx) }()

	rootURI := uri.File(filepath.Dir(abs))
	if err := client.Initialize(ctx, rootURI); err != nil {
		fmt.Fprintf(stderr, "bage rename: %v\n", err)
		return fmt.Errorf("bage rename: initialize: %w", err)
	}

	we, err := client.Rename(ctx, abs, string(content), uint32(*line), uint32(*col), *newName)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: %v\n", err)
		return fmt.Errorf("bage rename: %w", err)
	}

	fileEdits, err := lsp.WorkspaceEditToFileEdits(we, os.ReadFile)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: %v\n", err)
		return fmt.Errorf("bage rename: convert: %w", err)
	}
	if len(fileEdits) == 0 {
		fmt.Fprintln(stderr, "bage rename: server returned no edits")
		return errors.New("bage rename: no edits")
	}

	hasher := hashing.XXHasher{}
	edits, anchors, err := renameEdits(fileEdits, hasher)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: %v\n", err)
		return fmt.Errorf("bage rename: %w", err)
	}

	sess := &session.Session{
		Parser: treesitter.New(),
		Hasher: hasher,
		Lang:   lang,
		WALDir: os.TempDir(),
	}

	plan, err := sess.Prepare(ctx, edits, anchors)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: %v\n", err)
		return fmt.Errorf("bage rename: prepare: %w", err)
	}
	results, err := sess.Commit(plan)
	if err != nil {
		fmt.Fprintf(stderr, "bage rename: %v\n", err)
		return fmt.Errorf("bage rename: commit: %w", err)
	}

	printResults(stdout, results)
	return nil
}

// renameEdits converts a flat slice of byte-range FileEdits (from the LSP
// WorkspaceEdit) into region-anchored edits plus one FileAnchor per file. Each
// FileEdit becomes a Region whose byte range carries a region_hash computed from
// that file's live bytes, so Resolve verifies content (Exact in place; benign
// shift re-resolves; real conflict rejects). Files are read once each and the
// per-file anchor is built from those live bytes. Edits are returned in a
// deterministic (path, then start-byte) order.
func renameEdits(fileEdits []locator.FileEdit, hasher hashing.Hasher) ([]region.Edit, []region.FileAnchor, error) {
	byPath := make(map[string][]locator.FileEdit)
	for _, e := range fileEdits {
		byPath[e.Path] = append(byPath[e.Path], e)
	}

	paths := make([]string, 0, len(byPath))
	for p := range byPath {
		paths = append(paths, p)
	}
	sort.Strings(paths)

	var edits []region.Edit
	anchors := make([]region.FileAnchor, 0, len(paths))
	for _, p := range paths {
		live, err := os.ReadFile(p)
		if err != nil {
			return nil, nil, fmt.Errorf("read %q: %w", p, err)
		}
		li := region.NewLineIndex(live)

		group := byPath[p]
		sort.SliceStable(group, func(i, j int) bool { return group[i].StartByte < group[j].StartByte })
		for _, fe := range group {
			if fe.StartByte < 0 || fe.EndByte < fe.StartByte || fe.EndByte > len(live) {
				return nil, nil, fmt.Errorf("edit byte range [%d:%d] out of bounds for %q (len %d)", fe.StartByte, fe.EndByte, p, len(live))
			}
			reg := li.FillLineCols(region.Region{
				Path:       p,
				StartByte:  fe.StartByte,
				EndByte:    fe.EndByte,
				RegionHash: region.HashRegion(live, fe.StartByte, fe.EndByte),
			})
			edits = append(edits, region.Edit{Region: reg, NewText: fe.NewText})
		}
		anchors = append(anchors, fileAnchor(p, live, hasher))
	}
	return edits, anchors, nil
}

// fileAnchor builds the per-file drift gate (SPEC §8.1): the raw-byte hash gates
// byte-offset validity and the normalized hash classifies whitespace-only drift,
// both computed from the file's current live bytes.
func fileAnchor(path string, live []byte, hasher hashing.Hasher) region.FileAnchor {
	return region.FileAnchor{
		Path:     path,
		RawHash:  hashing.RawHash(hasher, live),
		NormHash: hashing.NormHash(hasher, live),
	}
}

// printResults writes one line per EditResult (sorted by path then changed
// start offset) describing the changed byte range, the new 1-based line range,
// and the recomputed region/file hashes — the write-back contract a coordinator
// (or a human) reads back (SPEC §8.2).
func printResults(stdout io.Writer, results []region.EditResult) {
	sort.SliceStable(results, func(i, j int) bool {
		if results[i].Path != results[j].Path {
			return results[i].Path < results[j].Path
		}
		return results[i].ChangedStart < results[j].ChangedStart
	})
	for _, r := range results {
		fmt.Fprintf(stdout,
			"applied %s bytes [%d:%d] lines [%d:%d] region=%s raw=%s norm=%s\n",
			r.Path, r.ChangedStart, r.ChangedEnd, r.NewStartLine, r.NewEndLine,
			r.NewRegionHash, r.NewFileRawHash, r.NewFileNormHash)
	}
}

// parseLang maps the --lang flag to a parser.Lang. An EMPTY string resolves to
// LangUnknown, which tells the session to auto-detect each file's language from
// its path via parser.LangForPath — so `bage apply` works on any file type
// without naming the language. A non-empty value must match a known language's
// canonical name (e.g. "go", "python", "markdown", "text"); anything else is an
// explicit usage error rather than a silent fallthrough.
func parseLang(s string) (parser.Lang, error) {
	if s == "" {
		return parser.LangUnknown, nil
	}
	for l := parser.LangGo; l <= parser.LangText; l++ {
		if l.String() == s {
			return l, nil
		}
	}
	return parser.LangUnknown, fmt.Errorf("bage apply: unsupported --lang %q", s)
}

// atoiPositive parses s as an integer that must be >= 1, used for 1-based line
// numbers. It returns a clear error for non-numeric or non-positive input.
func atoiPositive(s string) (int, error) {
	n := 0
	if s == "" {
		return 0, errors.New("empty number")
	}
	for _, r := range s {
		if r < '0' || r > '9' {
			return 0, fmt.Errorf("invalid number %q", s)
		}
		n = n*10 + int(r-'0')
	}
	if n < 1 {
		return 0, fmt.Errorf("line %q must be >= 1", s)
	}
	return n, nil
}

// formatterFor returns a CmdFormatter for the given command string, or nil to
// skip formatting when the string is empty. The command is split on spaces
// into name + args (sufficient for simple commands like "gofmt" or "cat").
func formatterFor(cmd string) format.Formatter {
	name, args, ok := splitCmd(cmd)
	if !ok {
		return nil
	}
	return format.CmdFormatter{Name: name, Args: args}
}

// linterFor returns a CmdLinter for the given command string, or nil to skip
// linting when the string is empty.
func linterFor(cmd string) format.Linter {
	name, args, ok := splitCmd(cmd)
	if !ok {
		return nil
	}
	return format.CmdLinter{Name: name, Args: args}
}

// splitCmd splits a command string into its executable name and arguments on
// runs of whitespace. It returns ok=false for an empty (or whitespace-only)
// string so callers can skip the corresponding step. It is sufficient for the
// simple commands the CLI accepts (e.g. "gofmt", "cat").
func splitCmd(cmd string) (name string, args []string, ok bool) {
	fields := strings.Fields(cmd)
	if len(fields) == 0 {
		return "", nil, false
	}
	return fields[0], fields[1:], true
}
