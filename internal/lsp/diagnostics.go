package lsp

import (
	"context"
	"fmt"

	"go.lsp.dev/protocol"
	"go.lsp.dev/uri"
)

// Diagnostic is one server-reported problem, flattened into Båge's reporting
// shape: a human severity string, the 1-based line/col range of the offending
// span, the message, and the diagnostic source (e.g. "compiler", "staticcheck").
// It is what `bage diagnose --lsp` surfaces per textDocument/publishDiagnostics
// entry (SPEC §10.5). Lines and columns are 1-based, converted from the LSP wire
// protocol's 0-based positions.
type Diagnostic struct {
	// Severity is the human label ("Error", "Warning", "Information", "Hint", or
	// a numeric fallback for an unknown code).
	Severity string
	// Source names the diagnostic's origin (server-provided; may be "").
	Source string
	// Message is the diagnostic text.
	Message string
	// StartLine is the 1-based start line of the diagnostic range.
	StartLine int
	// StartCol is the 1-based start column of the diagnostic range.
	StartCol int
	// EndLine is the 1-based end line of the diagnostic range.
	EndLine int
	// EndCol is the 1-based end column of the diagnostic range.
	EndCol int
}

// Diagnostics opens path in the language server (textDocument/didOpen with the
// given content) and collects the first textDocument/publishDiagnostics
// notification the server pushes for that document, mapping each entry to a
// Diagnostic in Båge's reporting shape. Unlike Rename, the result is delivered as
// a server→client NOTIFICATION (not a request response), so it is gathered from
// the read-loop handler via the Client's diags channel.
//
// The caller must have completed Initialize first. Diagnostics blocks until the
// server publishes (the common case — a server publishes once it has analyzed the
// opened document) or ctx is done; on ctx expiry it returns the context error
// wrapped, so a server that never publishes surfaces as a clear timeout rather
// than a hang. An empty publish (a clean file) returns an empty, non-nil slice.
func (c *Client) Diagnostics(ctx context.Context, path, content string) ([]Diagnostic, error) {
	docURI := uri.File(path)
	c.ver++
	open := &protocol.DidOpenTextDocumentParams{
		TextDocument: protocol.TextDocumentItem{
			URI:        docURI,
			LanguageID: languageIDForPath(path),
			Version:    c.ver,
			Text:       content,
		},
	}
	if err := c.conn.Notify(ctx, protocol.MethodTextDocumentDidOpen, open); err != nil {
		return nil, fmt.Errorf("lsp: didOpen %q: %w", path, err)
	}

	select {
	case raw := <-c.diags:
		out := make([]Diagnostic, 0, len(raw))
		for _, d := range raw {
			out = append(out, toDiagnostic(d))
		}
		return out, nil
	case <-ctx.Done():
		return nil, fmt.Errorf("lsp: awaiting diagnostics for %q: %w", path, ctx.Err())
	}
}

// toDiagnostic flattens an LSP protocol.Diagnostic into Båge's reporting shape,
// converting the 0-based wire positions to 1-based line/col.
func toDiagnostic(d protocol.Diagnostic) Diagnostic {
	return Diagnostic{
		Severity:  d.Severity.String(),
		Source:    d.Source,
		Message:   d.Message,
		StartLine: int(d.Range.Start.Line) + 1,
		StartCol:  int(d.Range.Start.Character) + 1,
		EndLine:   int(d.Range.End.Line) + 1,
		EndCol:    int(d.Range.End.Character) + 1,
	}
}
