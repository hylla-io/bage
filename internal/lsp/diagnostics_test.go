package lsp

import (
	"context"
	"net"
	"testing"
	"time"

	"go.lsp.dev/jsonrpc2"
	"go.lsp.dev/protocol"
)

// TestDiagnosticsInMemoryFake exercises the Client.Diagnostics path end to end
// without a real language server or a container: an in-memory fake server speaks
// JSON-RPC over the other end of a net.Pipe, answers initialize, and — on
// didOpen — pushes a canned textDocument/publishDiagnostics notification. The
// Client must collect that notification and map each Diagnostic to the reported
// shape (severity, 1-based line/col range, message, source). This is the cheap,
// Docker-free tier the real-server suite (mage lsp) complements.
func TestDiagnosticsInMemoryFake(t *testing.T) {
	clientConn, serverConn := net.Pipe()

	const docPath = "/work/main.go"
	wantDiag := protocol.Diagnostic{
		Range: protocol.Range{
			Start: protocol.Position{Line: 2, Character: 5},
			End:   protocol.Position{Line: 2, Character: 10},
		},
		Severity: protocol.DiagnosticSeverityError,
		Source:   "fakelint",
		Message:  "undefined: wobble",
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Stand up the fake server on serverConn and let it run for the test.
	go runFakeDiagServer(ctx, serverConn, docPath, wantDiag)

	c, err := NewClientFromConn(ctx, clientConn)
	if err != nil {
		t.Fatalf("NewClientFromConn: %v", err)
	}
	defer func() {
		closeCtx, cc := context.WithTimeout(context.Background(), time.Second)
		defer cc()
		_ = c.Close(closeCtx)
	}()

	if err := c.Initialize(ctx, protocol.DocumentURI("file:///work")); err != nil {
		t.Fatalf("Initialize: %v", err)
	}

	got, err := c.Diagnostics(ctx, docPath, "package main\n")
	if err != nil {
		t.Fatalf("Diagnostics: %v", err)
	}
	if len(got) != 1 {
		t.Fatalf("expected 1 diagnostic, got %d: %+v", len(got), got)
	}
	d := got[0]
	// Reported range is 1-based line/col (the contract's reporting convention),
	// converted from the server's 0-based LSP positions.
	if d.StartLine != 3 || d.StartCol != 6 {
		t.Fatalf("start position: want line=3 col=6, got line=%d col=%d", d.StartLine, d.StartCol)
	}
	if d.EndLine != 3 || d.EndCol != 11 {
		t.Fatalf("end position: want line=3 col=11, got line=%d col=%d", d.EndLine, d.EndCol)
	}
	if d.Severity != "Error" {
		t.Fatalf("severity: want Error, got %q", d.Severity)
	}
	if d.Source != "fakelint" {
		t.Fatalf("source: want fakelint, got %q", d.Source)
	}
	if d.Message != "undefined: wobble" {
		t.Fatalf("message: want %q, got %q", "undefined: wobble", d.Message)
	}
}

// runFakeDiagServer runs a minimal LSP server over conn: it answers initialize
// (and shutdown) as a request, and on a textDocument/didOpen notification it
// pushes a single textDocument/publishDiagnostics notification carrying diag for
// docPath. Every other request is answered with method-not-found, every other
// notification is acknowledged. It returns when ctx is cancelled or the
// connection drops.
func runFakeDiagServer(ctx context.Context, conn net.Conn, docPath string, diag protocol.Diagnostic) {
	srv := jsonrpc2.NewConn(jsonrpc2.NewStream(conn))
	srv.Go(ctx, func(reqCtx context.Context, reply jsonrpc2.Replier, req jsonrpc2.Request) error {
		switch req.Method() {
		case protocol.MethodInitialize:
			return reply(reqCtx, &protocol.InitializeResult{
				Capabilities: protocol.ServerCapabilities{},
			}, nil)
		case protocol.MethodInitialized:
			return reply(reqCtx, nil, nil)
		case protocol.MethodTextDocumentDidOpen:
			// Acknowledge the notification, then push the canned diagnostics.
			if err := reply(reqCtx, nil, nil); err != nil {
				return err
			}
			params := &protocol.PublishDiagnosticsParams{
				URI:         protocol.DocumentURI("file://" + docPath),
				Diagnostics: []protocol.Diagnostic{diag},
			}
			return srv.Notify(reqCtx, protocol.MethodTextDocumentPublishDiagnostics, params)
		case protocol.MethodShutdown:
			return reply(reqCtx, nil, nil)
		case protocol.MethodExit:
			return reply(reqCtx, nil, nil)
		default:
			return jsonrpc2.MethodNotFoundHandler(reqCtx, reply, req)
		}
	})
	<-ctx.Done()
}
