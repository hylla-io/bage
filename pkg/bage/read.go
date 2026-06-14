package bage

import (
	"context"
	"errors"
	"fmt"
	"io"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/region"
)

// Block is one Outline Symbol enriched with its content anchor and, optionally,
// its raw bytes. It is a FLAT struct (deliberately not an embed) so both JSON and
// TOON render it cleanly: JSON keys stay snake_case with no leaked Go field names
// or nested object, and a slice of Blocks is a uniform array that TOON emits in
// its compact tabular form (the token win). RegionHash is the region_hash that
// anchors the block by content (byte-identical to what Hylla stores per node);
// Content is the source slice for the block's byte range, populated only when
// ReadBlocks is asked to include it.
type Block struct {
	// Kind is the grammar node kind (e.g. "function_declaration"), "line" for the
	// text fallback, or "range" for a line/byte-addressed read.
	Kind string `json:"kind" toon:"kind"`
	// Name is the declared identifier, best-effort; "" when none was found.
	Name string `json:"name" toon:"name"`
	// StartLine is the 1-based start line of the block.
	StartLine int `json:"start_line" toon:"start_line"`
	// EndLine is the 1-based end line of the block.
	EndLine int `json:"end_line" toon:"end_line"`
	// StartByte is the inclusive start byte offset of the block.
	StartByte int `json:"start_byte" toon:"start_byte"`
	// EndByte is the exclusive end byte offset of the block.
	EndByte int `json:"end_byte" toon:"end_byte"`
	// RegionHash anchors the block by content; see region.HashRegion.
	RegionHash string `json:"region_hash" toon:"region_hash"`
	// Content is the raw source for the block's byte range; "" unless requested.
	Content string `json:"content,omitempty" toon:"content,omitempty"`
}

// ReadBlocks returns one Block per Outline Symbol of opened, in source order:
// every named declaration for a grammar-backed tree, or one line block for the
// grammar-free text fallback. Each Block carries the region_hash for its byte
// range (region.HashRegion over the opened source) so a host can anchor edits by
// content. When includeContent is true each Block.Content is set to the raw
// source bytes for its range (bounds-guarded as in directName); when false
// Content stays empty so callers can list structure cheaply.
func ReadBlocks(opened *OpenedFile, includeContent bool) []Block {
	if opened == nil || opened.Tree == nil {
		return nil
	}
	src := opened.Tree.Source
	syms := Outline(opened.Tree)
	blocks := make([]Block, 0, len(syms))
	for _, sym := range syms {
		var content string
		if includeContent &&
			sym.StartByte >= 0 && sym.EndByte <= len(src) && sym.EndByte >= sym.StartByte {
			content = string(src[sym.StartByte:sym.EndByte])
		}
		blocks = append(blocks, Block{
			Kind:       sym.Kind,
			Name:       sym.Name,
			StartLine:  sym.StartLine,
			EndLine:    sym.EndLine,
			StartByte:  sym.StartByte,
			EndByte:    sym.EndByte,
			RegionHash: region.HashRegion(src, sym.StartByte, sym.EndByte),
			Content:    content,
		})
	}
	return blocks
}

// ReadOptions selects what an Editor.Read returns. The zero value reads the whole
// file's structure with no raw content. IncludeContent populates each Block's
// Content with its source slice; Symbol, when non-empty, filters the returned
// Blocks to those whose Name matches exactly. Line/EndLine and
// StartByte/EndByte address a sub-range (see Read for the addressing rule).
type ReadOptions struct {
	// IncludeContent populates each returned Block's Content with its raw bytes.
	IncludeContent bool
	// Symbol, when non-empty, keeps only Blocks whose Name equals it.
	Symbol string
	// Line is the 1-based start line of a sub-range read (0 = unset; lines are
	// 1-based). When EndLine > Line the read spans the inclusive [Line, EndLine].
	Line int
	// EndLine is the 1-based end line of a sub-range read (0 = unset).
	EndLine int
	// StartByte is the inclusive start byte of a sub-range read.
	StartByte int
	// EndByte is the exclusive end byte of a sub-range read (range active only
	// when EndByte > StartByte).
	EndByte int
}

// ReadResult is the structured outcome of an Editor.Read: the read path, the
// detected language, the raw and normalized whole-file hashes (the drift gate),
// and the file's Blocks. RawHash gates byte-offset validity; NormHash is the
// whitespace-insensitive content anchor. Blocks carry per-block region_hashes and,
// when requested, raw content.
type ReadResult struct {
	// Path is the file path that was read (as supplied by the caller).
	Path string `json:"path" toon:"path"`
	// Lang is the detected source language's string name.
	Lang string `json:"lang" toon:"lang"`
	// RawHash is the whole-file raw-bytes digest (byte-offset validity gate).
	RawHash string `json:"raw_hash" toon:"raw_hash"`
	// NormHash is the whole-file normalized-bytes digest (content anchor).
	NormHash string `json:"norm_hash" toon:"norm_hash"`
	// Blocks are the file's Outline blocks, optionally filtered by ReadOptions.
	Blocks []Block `json:"blocks" toon:"blocks"`
}

// RenderText writes the human-readable read view of r to w: a header line
// "<path> lang=<lang> raw=<raw> norm=<norm> blocks=<N>" followed by one line per
// Block — "  <kind> <name> lines [<sl>:<el>] bytes [<sb>:<eb>] region=<H>" — where
// an empty Block.Name renders as "-". This is byte-identical to the text output
// cmd/bage show emits, so the CLI and any RenderText-aware host share one format.
// Implementing RenderText makes ReadResult text-renderable without importing
// pkg/render.
func (r ReadResult) RenderText(w io.Writer) error {
	if _, err := fmt.Fprintf(w, "%s lang=%s raw=%s norm=%s blocks=%d\n",
		r.Path, r.Lang, r.RawHash, r.NormHash, len(r.Blocks)); err != nil {
		return err
	}
	for _, b := range r.Blocks {
		name := b.Name
		if name == "" {
			name = "-"
		}
		if _, err := fmt.Fprintf(w,
			"  %s %s lines [%d:%d] bytes [%d:%d] region=%s\n",
			b.Kind, name, b.StartLine, b.EndLine, b.StartByte, b.EndByte, b.RegionHash); err != nil {
			return err
		}
	}
	return nil
}

// Read opens path with the shared parser, lists its Blocks, and returns a
// ReadResult carrying the path, detected language, and the whole-file raw and
// normalized hashes computed with the Editor's hasher.
//
// Addressing mode is chosen from opts, and the three modes are mutually
// exclusive: line mode (opts.Line >= 1, optionally bounded by EndLine > Line),
// byte mode (opts.EndByte > opts.StartByte), and whole-file/symbol mode when
// neither line nor byte addressing is active. Because lines are 1-based, Line 0
// means "unset"; a byte range is active only when EndByte > StartByte, so the
// zero-value ReadOptions{} stays whole-file. Setting opts.Symbol together with a
// line or byte range is rejected with an error.
//
// In line or byte mode Read returns exactly one Block: a synthetic Kind:"range"
// Symbol over the range ResolveRange resolves, anchored by region.HashRegion over
// that range; its Content is the range's raw bytes when opts.IncludeContent is
// set (bounds-guarded). Otherwise, when opts.IncludeContent is set each Block's
// Content is populated; when opts.Symbol is non-empty the Blocks are filtered to
// those whose Name matches exactly. It reuses OpenFile,
// ReadBlocks, ResolveRange, and the Editor's hasher; the opened file is closed
// before Read returns.
func (e *Editor) Read(ctx context.Context, path string, opts ReadOptions) (ReadResult, error) {
	opened, err := OpenFile(ctx, path)
	if err != nil {
		return ReadResult{}, err
	}
	defer opened.Close()

	src := opened.Tree.Source

	lineMode := opts.Line >= 1
	byteMode := opts.EndByte > opts.StartByte
	if (lineMode || byteMode) && opts.Symbol != "" {
		return ReadResult{}, errors.New("read: symbol filtering is mutually exclusive with line/byte addressing")
	}

	var blocks []Block
	switch {
	case lineMode || byteMode:
		b, err := e.rangeBlock(src, opts, lineMode)
		if err != nil {
			return ReadResult{}, err
		}
		blocks = []Block{b}
	default:
		blocks = ReadBlocks(opened, opts.IncludeContent)
		if opts.Symbol != "" {
			filtered := blocks[:0:0]
			for _, b := range blocks {
				if b.Name == opts.Symbol {
					filtered = append(filtered, b)
				}
			}
			blocks = filtered
		}
	}
	return ReadResult{
		Path:     path,
		Lang:     opened.Lang.String(),
		RawHash:  hashing.RawHash(e.hasher, src),
		NormHash: hashing.NormHash(e.hasher, src),
		Blocks:   blocks,
	}, nil
}

// rangeBlock resolves the line- or byte-addressed sub-range described by opts
// against src via ResolveRange and returns a single synthetic Kind:"range" Block
// anchored by region.HashRegion over the resolved byte range. When lineMode is
// true it passes opts.Line plus, if opts.EndLine > opts.Line, a "start-end" lines
// string (else lines=""); otherwise it passes line=-1 with opts.StartByte/EndByte.
// Content is the resolved range's raw bytes when opts.IncludeContent is set and
// the range is in bounds.
func (e *Editor) rangeBlock(src []byte, opts ReadOptions, lineMode bool) (Block, error) {
	lineArg := -1
	linesArg := ""
	startByte, endByte := -1, -1
	if lineMode {
		lineArg = opts.Line
		if opts.EndLine > opts.Line {
			linesArg = fmt.Sprintf("%d-%d", opts.Line, opts.EndLine)
			lineArg = -1
		}
	} else {
		startByte, endByte = opts.StartByte, opts.EndByte
	}

	reg, err := ResolveRange(src, lineArg, linesArg, startByte, endByte)
	if err != nil {
		return Block{}, err
	}

	var content string
	if opts.IncludeContent &&
		reg.StartByte >= 0 && reg.EndByte <= len(src) && reg.EndByte >= reg.StartByte {
		content = string(src[reg.StartByte:reg.EndByte])
	}
	return Block{
		Kind:       "range",
		StartByte:  reg.StartByte,
		EndByte:    reg.EndByte,
		StartLine:  reg.StartLine,
		EndLine:    reg.EndLine,
		RegionHash: region.HashRegion(src, reg.StartByte, reg.EndByte),
		Content:    content,
	}, nil
}
