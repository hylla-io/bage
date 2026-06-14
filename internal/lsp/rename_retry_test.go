package lsp

import (
	"context"
	"fmt"
	"net"
	"sync/atomic"
	"testing"
	"time"

	"go.lsp.dev/jsonrpc2"
	"go.lsp.dev/protocol"
)

// TestRenameRetriesUntilServerReady proves Rename does not give up the first time
// a still-indexing server (e.g. rust-analyzer on a cold crate) answers a rename
// with an error. The fake server rejects the first two rename requests, then
// returns a real WorkspaceEdit; Rename must retry and surface that edit.
func TestRenameRetriesUntilServerReady(t *testing.T) {
	clientConn, serverConn := net.Pipe()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	var calls atomic.Int32
	go runFakeRenameServer(ctx, serverConn, func() (protocol.WorkspaceEdit, bool) {
		n := calls.Add(1)
		if n <= 2 {
			return protocol.WorkspaceEdit{}, false
		}
		return readyRenameEdit(), true
	})

	c, err := NewClientFromConn(ctx, clientConn)
	if err != nil {
		t.Fatalf("NewClientFromConn: %v", err)
	}
	c.renameRetry = 5 * time.Millisecond
	c.renameDeadline = 2 * time.Second
	defer closeRenameClient(t, c)

	if err := c.Initialize(ctx, protocol.DocumentURI("file:///work")); err != nil {
		t.Fatalf("Initialize: %v", err)
	}

	we, err := c.Rename(ctx, "/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
	if err != nil {
		t.Fatalf("Rename after retries: %v", err)
	}
	if !workspaceEditHasChanges(we) {
		t.Fatalf("expected a non-empty WorkspaceEdit, got %+v", we)
	}
	if got := calls.Load(); got < 3 {
		t.Fatalf("expected >= 3 rename attempts (2 not-ready + 1 ready), got %d", got)
	}
}

// TestRenameRetriesOnEmptyEdit proves an empty but non-error rename response is
// also treated as not-ready and retried, since some servers answer a rename
// during indexing with an empty edit rather than an error.
func TestRenameRetriesOnEmptyEdit(t *testing.T) {
	clientConn, serverConn := net.Pipe()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	var calls atomic.Int32
	go runFakeRenameServer(ctx, serverConn, func() (protocol.WorkspaceEdit, bool) {
		n := calls.Add(1)
		if n <= 2 {
			return protocol.WorkspaceEdit{}, true
		}
		return readyRenameEdit(), true
	})

	c, err := NewClientFromConn(ctx, clientConn)
	if err != nil {
		t.Fatalf("NewClientFromConn: %v", err)
	}
	c.renameRetry = 5 * time.Millisecond
	c.renameDeadline = 2 * time.Second
	defer closeRenameClient(t, c)

	if err := c.Initialize(ctx, protocol.DocumentURI("file:///work")); err != nil {
		t.Fatalf("Initialize: %v", err)
	}

	we, err := c.Rename(ctx, "/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
	if err != nil {
		t.Fatalf("Rename after empty retries: %v", err)
	}
	if !workspaceEditHasChanges(we) {
		t.Fatalf("expected a non-empty WorkspaceEdit, got %+v", we)
	}
	if got := calls.Load(); got < 3 {
		t.Fatalf("expected >= 3 rename attempts, got %d", got)
	}
}

// TestRenameDeadlineExceeded proves the retry loop is bounded: a server that never
// becomes ready makes Rename fail once the deadline is spent rather than hang.
func TestRenameDeadlineExceeded(t *testing.T) {
	clientConn, serverConn := net.Pipe()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	go runFakeRenameServer(ctx, serverConn, func() (protocol.WorkspaceEdit, bool) {
		return protocol.WorkspaceEdit{}, false
	})

	c, err := NewClientFromConn(ctx, clientConn)
	if err != nil {
		t.Fatalf("NewClientFromConn: %v", err)
	}
	c.renameRetry = 5 * time.Millisecond
	c.renameDeadline = 80 * time.Millisecond
	defer closeRenameClient(t, c)

	if err := c.Initialize(ctx, protocol.DocumentURI("file:///work")); err != nil {
		t.Fatalf("Initialize: %v", err)
	}

	if _, err := c.Rename(ctx, "/work/main.rs", "fn main() {}\n", 0, 3, "renamed"); err == nil {
		t.Fatalf("expected Rename to fail after deadline, got nil error")
	}
}

// readyRenameEdit is a minimal non-empty WorkspaceEdit a ready server returns.
func readyRenameEdit() protocol.WorkspaceEdit {
	return protocol.WorkspaceEdit{
		Changes: map[protocol.DocumentURI][]protocol.TextEdit{
			protocol.DocumentURI("file:///work/main.rs"): {
				{
					Range: protocol.Range{
						Start: protocol.Position{Line: 0, Character: 3},
						End:   protocol.Position{Line: 0, Character: 7},
					},
					NewText: "renamed",
				},
			},
		},
	}
}

// closeRenameClient closes c with a short bounded context.
func closeRenameClient(t *testing.T, c *Client) {
	t.Helper()
	closeCtx, cc := context.WithTimeout(context.Background(), time.Second)
	defer cc()
	_ = c.Close(closeCtx)
}

// runFakeRenameServer runs a minimal LSP server that answers initialize and
// didOpen, and delegates each textDocument/rename to next: next returns
// (edit, ok); ok=false makes the server answer with a JSON-RPC error (a
// not-yet-ready server) while ok=true returns edit as the rename result.
func runFakeRenameServer(ctx context.Context, conn net.Conn, next func() (protocol.WorkspaceEdit, bool)) {
	srv := jsonrpc2.NewConn(jsonrpc2.NewStream(conn))
	srv.Go(ctx, func(reqCtx context.Context, reply jsonrpc2.Replier, req jsonrpc2.Request) error {
		switch req.Method() {
		case protocol.MethodInitialize:
			return reply(reqCtx, &protocol.InitializeResult{Capabilities: protocol.ServerCapabilities{}}, nil)
		case protocol.MethodInitialized:
			return reply(reqCtx, nil, nil)
		case protocol.MethodTextDocumentDidOpen:
			return reply(reqCtx, nil, nil)
		case protocol.MethodTextDocumentRename:
			edit, ok := next()
			if !ok {
				return reply(reqCtx, nil, fmt.Errorf("server still indexing: no references found"))
			}
			return reply(reqCtx, &edit, nil)
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
