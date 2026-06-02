// Package lsp bridges an LSP server's symbol operations to Båge's byte-range
// edit model. The load-bearing part is the pure conversion from LSP positions
// (zero-based line + UTF-16 code-unit character) to UTF-8 byte offsets, and from
// a protocol.WorkspaceEdit to a slice of locator.FileEdit. Per
// code_graph_architecture.md §10 the UTF-16↔UTF-8 conversion is centralized here
// at the single LSP boundary so the rest of Båge stays byte-addressed.
package lsp

import (
	"fmt"
	"unicode/utf16"
	"unicode/utf8"

	"go.lsp.dev/protocol"

	"github.com/hylla-io/bage/internal/locator"
)

// ByteOffset maps a zero-based LSP position to a UTF-8 byte offset within src.
//
// An LSP Position is (Line, Character) where Line counts '\n'-terminated lines
// from zero and Character counts UTF-16 code units from the line start. A rune in
// the astral planes (> 0xFFFF) is encoded as a surrogate pair and therefore
// counts as TWO UTF-16 code units; every other rune counts as one. Because Båge
// addresses regions by UTF-8 byte offset, this function walks src rune-by-rune,
// advancing the byte cursor by each rune's UTF-8 width while debiting the
// requested UTF-16 budget.
//
// Clamping follows the LSP spec: a Line beyond the last line resolves to end of
// src; a Character beyond the line's content resolves to the line end (the byte
// index of the terminating '\n', or end of src for the final line). The '\n'
// itself is never crossed by the character walk.
//
// Policy: the only rejected input is a malformed UTF-8 sequence encountered while
// consuming characters on the target line — Båge requires valid UTF-8 source, so
// rather than silently mis-count, ByteOffset returns an error. Empty src with a
// zero position yields offset 0.
func ByteOffset(src []byte, pos protocol.Position) (int, error) {
	// Phase 1: walk to the start byte of the target line by counting newlines.
	lineStart := 0
	line := uint32(0)
	for line < pos.Line {
		nl := indexByte(src, lineStart, '\n')
		if nl < 0 {
			// Line beyond EOF: clamp to end of src.
			return len(src), nil
		}
		lineStart = nl + 1
		line++
	}

	// Phase 2: consume pos.Character UTF-16 code units along this line, advancing
	// one rune at a time. Stop at the line's terminating '\n' or end of src.
	offset := lineStart
	var consumed uint32
	for consumed < pos.Character && offset < len(src) {
		if src[offset] == '\n' {
			// Character index past the line's content: clamp to line end.
			return offset, nil
		}
		r, size := utf8.DecodeRune(src[offset:])
		if r == utf8.RuneError && size == 1 {
			return 0, fmt.Errorf("lsp: malformed UTF-8 at byte %d", offset)
		}
		consumed += uint32(utf16Len(r))
		offset += size
	}
	return offset, nil
}

// indexByte returns the index of the first b at or after start in src, or -1.
func indexByte(src []byte, start int, b byte) int {
	for i := start; i < len(src); i++ {
		if src[i] == b {
			return i
		}
	}
	return -1
}

// utf16Len reports how many UTF-16 code units encode r: two for astral runes
// (> 0xFFFF, which form a surrogate pair), one otherwise. r is always a valid
// code point here (decoded from valid UTF-8), so utf16.RuneLen never returns -1;
// the fallback to 1 is defensive only.
func utf16Len(r rune) int {
	if n := utf16.RuneLen(r); n > 0 {
		return n
	}
	return 1
}

// WorkspaceEditToFileEdits flattens a protocol.WorkspaceEdit into a slice of
// locator.FileEdit. Both representations are honored: the legacy we.Changes map
// (DocumentURI → []TextEdit) and the versioned we.DocumentChanges
// ([]TextDocumentEdit). For each TextEdit the file's current bytes are obtained
// via the injected read function and the edit's UTF-16 Range is converted to
// UTF-8 StartByte/EndByte via ByteOffset, centralizing the boundary conversion.
//
// read is injected so callers (and tests) control file access; it is invoked at
// most once per distinct file URI. URIs are resolved to filesystem paths via
// DocumentURI.Filename(). Edits are returned grouped by file in a deterministic
// order: Changes first (in URI iteration order is non-deterministic, so files are
// not sorted here — ApplyFileEdits reverse-sorts per file by offset), then
// DocumentChanges. An error from read or from ByteOffset aborts and is wrapped.
func WorkspaceEditToFileEdits(we protocol.WorkspaceEdit, read func(path string) ([]byte, error)) ([]locator.FileEdit, error) {
	var out []locator.FileEdit
	cache := make(map[string][]byte)

	srcFor := func(path string) ([]byte, error) {
		if b, ok := cache[path]; ok {
			return b, nil
		}
		b, err := read(path)
		if err != nil {
			return nil, fmt.Errorf("lsp: read %q: %w", path, err)
		}
		cache[path] = b
		return b, nil
	}

	convert := func(path string, edits []protocol.TextEdit) error {
		src, err := srcFor(path)
		if err != nil {
			return err
		}
		for _, e := range edits {
			start, err := ByteOffset(src, e.Range.Start)
			if err != nil {
				return fmt.Errorf("lsp: %q start: %w", path, err)
			}
			end, err := ByteOffset(src, e.Range.End)
			if err != nil {
				return fmt.Errorf("lsp: %q end: %w", path, err)
			}
			out = append(out, locator.FileEdit{
				Path:      path,
				StartByte: start,
				EndByte:   end,
				NewText:   e.NewText,
			})
		}
		return nil
	}

	for docURI, edits := range we.Changes {
		if err := convert(docURI.Filename(), edits); err != nil {
			return nil, err
		}
	}
	for _, tde := range we.DocumentChanges {
		if err := convert(tde.TextDocument.URI.Filename(), tde.Edits); err != nil {
			return nil, err
		}
	}
	return out, nil
}
