package bage

import "github.com/hylla-io/bage/internal/region"

// Block is one Outline Symbol enriched with its content anchor and, optionally,
// its raw bytes. RegionHash is the region_hash that anchors the block by content
// (byte-identical to what Hylla stores per node); Content is the source slice for
// the block's byte range, populated only when ReadBlocks is asked to include it.
// Block embeds Symbol, so every Symbol field (Kind, Name, byte/line ranges) is
// promoted onto the Block.
type Block struct {
	Symbol
	// RegionHash anchors the block by content; see region.HashRegion.
	RegionHash string `json:"region_hash"`
	// Content is the raw source for the block's byte range; "" unless requested.
	Content string `json:"content,omitempty"`
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
			Symbol:     sym,
			RegionHash: region.HashRegion(src, sym.StartByte, sym.EndByte),
			Content:    content,
		})
	}
	return blocks
}
