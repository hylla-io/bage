package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/pkg/bage"
)

// showBlock is one addressable block in a file's Outline as emitted by show: a
// declaration (grammar-backed file) or a single line (text fallback). It carries
// everything an agent needs to target an edit — kind, name, 1-based line range,
// half-open byte range, and the region_hash that bage apply --region-hash will
// verify+accept for that exact byte range (the round-trip anchor).
type showBlock struct {
	// Kind is the grammar node kind (e.g. "function_declaration"), or "line" for
	// the text fallback.
	Kind string `json:"kind"`
	// Name is the declared identifier, best-effort; "" when none was found (e.g.
	// for a "line" block).
	Name string `json:"name"`
	// StartLine is the 1-based start line of the block.
	StartLine int `json:"start_line"`
	// EndLine is the 1-based end line of the block.
	EndLine int `json:"end_line"`
	// StartByte is the inclusive start byte offset of the block.
	StartByte int `json:"start_byte"`
	// EndByte is the exclusive end byte offset of the block.
	EndByte int `json:"end_byte"`
	// RegionHash is the region_hash of src[StartByte:EndByte], byte-identical to
	// what bage apply --region-hash verifies for that same range.
	RegionHash string `json:"region_hash"`
}

// showView is the structured read view of a file emitted by show: the resolved
// language, the file-level raw/norm hashes (the per-file drift anchor), and the
// Outline of addressable blocks. It is the standalone/MCP-facing read side an
// agent uses to SEE a file before editing it (in GDD mode Hylla's graph is the
// read side instead).
type showView struct {
	// Path is the file that was shown.
	Path string `json:"path"`
	// Lang is the canonical language name selected for Path (never "unknown";
	// falls back to text).
	Lang string `json:"lang"`
	// RawHash is the xxHash %016x of the file's RAW bytes (byte-offset gate).
	RawHash string `json:"raw_hash"`
	// NormHash is the xxHash %016x of the file's normalized bytes (drift gate).
	NormHash string `json:"norm_hash"`
	// Outline lists every addressable block in source order.
	Outline []showBlock `json:"outline"`
}

// runShow parses the show flags and emits the READ view of --file: it opens and
// parses the file via the shared parser (pkg/bage.OpenFile), builds the Outline
// of addressable blocks via the shared pkg/bage.ReadBlocks (each block carries
// the region_hash apply verifies), alongside the file's raw/norm hashes.
// Default output is human-readable; --json emits the structured showView. show is
// strictly READ-ONLY — it writes nothing to disk, ever. On any error it prints a
// clear message to stderr and returns it.
func runShow(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	fs := flag.NewFlagSet("show", flag.ContinueOnError)
	fs.SetOutput(stderr)

	var (
		file   = fs.String("file", "", "path of the file to show (required)")
		asJSON = fs.Bool("json", false, "emit structured JSON instead of human-readable text")
	)

	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("bage show: %w", err)
	}

	if *file == "" {
		fmt.Fprintln(stderr, "bage show: --file is required")
		return errors.New("bage show: --file is required")
	}

	// Read the raw bytes once: they are both what OpenFile parses and what every
	// region_hash / file hash is computed over, so the bytes show reports against
	// are exactly the bytes apply will verify.
	src, err := os.ReadFile(*file)
	if err != nil {
		fmt.Fprintf(stderr, "bage show: read %q: %v\n", *file, err)
		return fmt.Errorf("bage show: read %q: %w", *file, err)
	}

	opened, err := bage.OpenFile(ctx, *file)
	if err != nil {
		fmt.Fprintf(stderr, "bage show: %v\n", err)
		return fmt.Errorf("bage show: %w", err)
	}
	defer opened.Close()

	hasher := hashing.XXHasher{}
	view := showView{
		Path:     *file,
		Lang:     opened.Lang.String(),
		RawHash:  hashing.RawHash(hasher, src),
		NormHash: hashing.NormHash(hasher, src),
		Outline:  make([]showBlock, 0),
	}
	for _, blk := range bage.ReadBlocks(opened, false) {
		view.Outline = append(view.Outline, showBlock{
			Kind:      blk.Kind,
			Name:      blk.Name,
			StartLine: blk.StartLine,
			EndLine:   blk.EndLine,
			StartByte: blk.StartByte,
			EndByte:   blk.EndByte,
			// SAME region_hash apply verifies, so the hash round-trips exactly.
			RegionHash: blk.RegionHash,
		})
	}

	if *asJSON {
		return printShowJSON(stdout, stderr, view)
	}
	printShowText(stdout, view)
	return nil
}

// printShowJSON writes the showView as indented JSON. A marshal failure (which
// should not happen for this plain struct) is reported to stderr and returned.
func printShowJSON(stdout, stderr io.Writer, view showView) error {
	b, err := json.MarshalIndent(view, "", "  ")
	if err != nil {
		fmt.Fprintf(stderr, "bage show: marshal json: %v\n", err)
		return fmt.Errorf("bage show: marshal json: %w", err)
	}
	fmt.Fprintln(stdout, string(b))
	return nil
}

// printShowText writes the human-readable read view: a header line with the path,
// language, and file raw/norm hashes, then one line per block with its kind,
// name, 1-based line range, byte range, and region_hash — the anchor a caller
// echoes into bage apply --region-hash.
func printShowText(stdout io.Writer, view showView) {
	fmt.Fprintf(stdout, "%s lang=%s raw=%s norm=%s blocks=%d\n",
		view.Path, view.Lang, view.RawHash, view.NormHash, len(view.Outline))
	for _, b := range view.Outline {
		name := b.Name
		if name == "" {
			name = "-"
		}
		fmt.Fprintf(stdout,
			"  %s %s lines [%d:%d] bytes [%d:%d] region=%s\n",
			b.Kind, name, b.StartLine, b.EndLine, b.StartByte, b.EndByte, b.RegionHash)
	}
}
