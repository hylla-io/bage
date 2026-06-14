package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/hylla-io/bage/pkg/bage"
	"github.com/hylla-io/bage/pkg/render"
)

// runRead parses the read flags and emits the structured READ view of --file via
// the shared pkg/bage Editor.Read: the file's detected language, raw/norm drift
// hashes, and the Outline of addressable Blocks (each carrying the region_hash
// apply verifies). The view is rendered in the --format the caller selects
// (text|json|toon) through pkg/render.Emit, so read shares one format surface
// with every other verb. Sub-range and symbol selection come from the addressing
// flags (--line/--lines/--start/--end, --symbol) and --content includes each
// block's raw source. read is strictly READ-ONLY — it writes nothing to disk. On
// any error it renders the error envelope to stderr in the same format and
// returns the error.
func runRead(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("read", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file    = fs.String("file", "", "path of the file to read (required)")
		line    = fs.Int("line", -1, "1-based line of a single-line sub-range read")
		lines   = fs.String("lines", "", "1-based inclusive line range L1-L2 sub-range read")
		start   = fs.Int("start", -1, "inclusive start byte of a byte sub-range read")
		end     = fs.Int("end", -1, "exclusive end byte of a byte sub-range read")
		symbol  = fs.String("symbol", "", "keep only the block whose symbol name equals this")
		content = fs.Bool("content", false, "include each block's raw source text")
		format  = fs.String("format", "text", "output format: text|json|toon")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage read: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage read: --file is required")
		return errors.New("bage read: --file is required")
	}

	fmtKind, err := render.ParseFormat(*format)
	if err != nil {
		fmt.Fprintln(stderr, err.Error())
		return err
	}

	opts, err := readOptions(*line, *lines, *start, *end, *symbol, *content)
	if err != nil {
		fmt.Fprintln(stderr, err.Error())
		return err
	}

	ed, err := bage.Open(bage.Config{WALDir: os.TempDir()})
	if err != nil {
		fmt.Fprintf(stderr, "bage read: %v\n", err)
		return fmt.Errorf("bage read: %w", err)
	}

	res, err := ed.Read(ctx, *file, opts)
	if err != nil {
		_ = render.Emit(stderr, fmtKind, bage.Envelope(err))
		return fmt.Errorf("bage read: %w", err)
	}

	return render.Emit(stdout, fmtKind, res)
}

// readOptions builds a bage.ReadOptions from the read flags: --line maps to Line,
// --lines "L1-L2" maps to Line/EndLine, --start/--end map to StartByte/EndByte,
// --symbol maps to Symbol, and --content maps to IncludeContent. Line and byte
// addressing default to "unset" (Line 0, byte range inactive) so the zero-value
// options read the whole file. A malformed --lines is a usage error.
func readOptions(line int, lines string, start, end int, symbol string, content bool) (bage.ReadOptions, error) {
	opts := bage.ReadOptions{
		IncludeContent: content,
		Symbol:         symbol,
	}
	if start >= 0 {
		opts.StartByte = start
	}
	if end >= 0 {
		opts.EndByte = end
	}
	if line >= 0 {
		opts.Line = line
	}
	if lines != "" {
		lo, hi, ok := strings.Cut(lines, "-")
		if !ok {
			return bage.ReadOptions{}, fmt.Errorf("bage read: --lines %q must be L1-L2", lines)
		}
		startLine, err := atoiPositive(strings.TrimSpace(lo))
		if err != nil {
			return bage.ReadOptions{}, fmt.Errorf("bage read: --lines start: %w", err)
		}
		endLine, err := atoiPositive(strings.TrimSpace(hi))
		if err != nil {
			return bage.ReadOptions{}, fmt.Errorf("bage read: --lines end: %w", err)
		}
		opts.Line = startLine
		opts.EndLine = endLine
	}
	return opts, nil
}
