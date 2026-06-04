package lsp

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"

	"go.lsp.dev/jsonrpc2"
	"go.lsp.dev/protocol"
	"go.lsp.dev/uri"
)

// Client is a thin LSP client over a spawned language-server subprocess. It
// speaks JSON-RPC 2.0 (LSP framing) across the server's stdio and exposes only
// the minimal surface Båge needs: lifecycle (Initialize/Close) and symbol rename.
// All byte-offset conversion lives in the pure functions in convert.go; this type
// is glue. A Client is not safe for concurrent use.
type Client struct {
	cmd  *exec.Cmd
	conn jsonrpc2.Conn
	stds io.Closer // combined stdio pipe closer
	ver  int32
	// diags carries server→client textDocument/publishDiagnostics notifications
	// from the read-loop handler to a waiting Diagnostics call. It is buffered so
	// a server that publishes before anyone is collecting does not block the read
	// loop. nil only on a zero-value Client (constructors always set it).
	diags chan []protocol.Diagnostic
}

// serverIO bundles a subprocess's stdin (writer) and stdout (reader) into a
// single io.ReadWriteCloser suitable for jsonrpc2.NewStream.
type serverIO struct {
	io.ReadCloser  // stdout
	io.WriteCloser // stdin
}

// Close closes both ends of the subprocess stdio pipe.
func (s serverIO) Close() error {
	rerr := s.ReadCloser.Close()
	werr := s.WriteCloser.Close()
	if werr != nil {
		return werr
	}
	return rerr
}

// NewClient spawns the LSP server described by command (e.g. []string{"gopls"})
// and wires a JSON-RPC connection over its stdio. The connection's read loop is
// started immediately; incoming server-to-client requests (e.g. window/logMessage)
// are answered with method-not-found, which is sufficient for the rename path.
// The returned Client must be Closed to release the subprocess.
func NewClient(ctx context.Context, command []string) (*Client, error) {
	if len(command) == 0 {
		return nil, fmt.Errorf("lsp: empty server command")
	}
	cmd := exec.CommandContext(ctx, command[0], command[1:]...)

	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, fmt.Errorf("lsp: stdin pipe: %w", err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, fmt.Errorf("lsp: stdout pipe: %w", err)
	}
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("lsp: start %q: %w", command[0], err)
	}

	rwc := serverIO{ReadCloser: stdout, WriteCloser: stdin}
	c := newClientWithTransport(ctx, rwc)
	c.cmd = cmd
	return c, nil
}

// newClientWithTransport wires a Client over an arbitrary bidirectional
// transport. This is the single transport seam: NewClient supplies a spawned
// subprocess's stdio, while a TCP/socket caller (e.g. the containerized-server
// integration test) supplies a net.Conn. Both satisfy io.ReadWriteCloser, and
// jsonrpc2.NewStream is agnostic to which one it is given. The read loop is
// started immediately and server→client requests are answered with
// method-not-found, sufficient for the rename path. The caller is responsible
// for setting cmd when a subprocess backs the transport; for a socket transport
// cmd stays nil and Close skips the subprocess wait.
func newClientWithTransport(ctx context.Context, rwc io.ReadWriteCloser) *Client {
	conn := jsonrpc2.NewConn(jsonrpc2.NewStream(rwc))
	c := &Client{
		conn:  conn,
		stds:  rwc,
		diags: make(chan []protocol.Diagnostic, diagBuffer),
	}
	conn.Go(ctx, c.handle)
	return c
}

// diagBuffer is the depth of the publishDiagnostics queue. A server may publish
// several rounds (initial + refined) before a Diagnostics call collects; a small
// buffer keeps the read loop from blocking without unbounded growth. Excess
// notifications past the buffer are dropped (the latest authoritative set is what
// matters), never blocking the connection's read loop.
const diagBuffer = 8

// handle is the Client's JSON-RPC read-loop handler. It intercepts the
// server→client textDocument/publishDiagnostics NOTIFICATION (pushed after
// didOpen, not as a request response), decodes its diagnostics, and forwards them
// to any waiting Diagnostics call via c.diags; the notification is then
// acknowledged. Every other request falls through to MethodNotFoundHandler, the
// same minimal behavior the rename path relies on. A decode failure or a full
// diags buffer is non-fatal: the notification is acknowledged and dropped so the
// read loop never stalls.
func (c *Client) handle(ctx context.Context, reply jsonrpc2.Replier, req jsonrpc2.Request) error {
	if req.Method() == protocol.MethodTextDocumentPublishDiagnostics {
		var params protocol.PublishDiagnosticsParams
		if err := json.Unmarshal(req.Params(), &params); err == nil {
			select {
			case c.diags <- params.Diagnostics:
			default: // buffer full: drop rather than block the read loop.
			}
		}
		return reply(ctx, nil, nil)
	}
	return jsonrpc2.MethodNotFoundHandler(ctx, reply, req)
}

// NewClientFromConn wires a Client over an already-established bidirectional
// connection — typically a net.Conn dialed to a language server listening on a
// TCP socket (e.g. a containerized gopls started with `gopls -listen`). Unlike
// NewClient there is no local subprocess, so Close tears down only the JSON-RPC
// connection and the supplied transport; lifecycle of the remote server is the
// caller's responsibility. conn is adopted by the Client and closed by Close.
func NewClientFromConn(ctx context.Context, conn io.ReadWriteCloser) (*Client, error) {
	if conn == nil {
		return nil, fmt.Errorf("lsp: nil connection")
	}
	return newClientWithTransport(ctx, conn), nil
}

// Initialize performs the LSP initialize/initialized handshake rooted at rootURI
// (a file:// URI for the workspace root).
func (c *Client) Initialize(ctx context.Context, rootURI protocol.DocumentURI) error {
	params := &protocol.InitializeParams{
		ProcessID: int32(os.Getpid()),
		RootURI:   rootURI,
		Capabilities: protocol.ClientCapabilities{
			Workspace: &protocol.WorkspaceClientCapabilities{
				WorkspaceEdit: &protocol.WorkspaceClientCapabilitiesWorkspaceEdit{
					DocumentChanges: true,
				},
			},
		},
	}
	var res protocol.InitializeResult
	if _, err := c.conn.Call(ctx, protocol.MethodInitialize, params, &res); err != nil {
		return fmt.Errorf("lsp: initialize: %w", err)
	}
	if err := c.conn.Notify(ctx, protocol.MethodInitialized, &protocol.InitializedParams{}); err != nil {
		return fmt.Errorf("lsp: initialized: %w", err)
	}
	return nil
}

// Rename opens the file at path, requests a textDocument/rename of the symbol at
// the zero-based (line, col) UTF-16 position, and returns the server's
// WorkspaceEdit. col is a UTF-16 code-unit offset per the LSP spec; convert the
// result to byte offsets with WorkspaceEditToFileEdits. content is the file's
// current text, sent via textDocument/didOpen so the server has an authoritative
// view before the rename.
func (c *Client) Rename(ctx context.Context, path, content string, line, col uint32, newName string) (protocol.WorkspaceEdit, error) {
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
		return protocol.WorkspaceEdit{}, fmt.Errorf("lsp: didOpen %q: %w", path, err)
	}

	params := &protocol.RenameParams{
		TextDocumentPositionParams: protocol.TextDocumentPositionParams{
			TextDocument: protocol.TextDocumentIdentifier{URI: docURI},
			Position:     protocol.Position{Line: line, Character: col},
		},
		NewName: newName,
	}
	var we protocol.WorkspaceEdit
	if _, err := c.conn.Call(ctx, protocol.MethodTextDocumentRename, params, &we); err != nil {
		return protocol.WorkspaceEdit{}, fmt.Errorf("lsp: rename %q: %w", path, err)
	}
	return we, nil
}

// languageIDForPath maps a file path's extension to the LSP textDocument
// languageId the server expects in didOpen. A wrong languageId (e.g. sending
// "go" for a .py file) makes a language server skip analysis, so this is what
// lets one Client drive any server. Unknown extensions fall back to "plaintext",
// which is harmless for servers that key off the file path rather than the ID.
func languageIDForPath(path string) protocol.LanguageIdentifier {
	switch filepath.Ext(path) {
	case ".go":
		return protocol.GoLanguage
	case ".py":
		return protocol.PythonLanguage
	case ".ts":
		return protocol.TypeScriptLanguage
	case ".tsx":
		return protocol.LanguageIdentifier("typescriptreact")
	case ".js", ".jsx":
		return protocol.JavaScriptLanguage
	case ".rs":
		return protocol.LanguageIdentifier("rust")
	case ".java":
		return protocol.LanguageIdentifier("java")
	case ".c", ".h":
		return protocol.CLanguage
	case ".cc", ".cpp", ".cxx", ".hpp":
		return protocol.CppLanguage
	case ".cs":
		return protocol.LanguageIdentifier("csharp")
	case ".rb":
		return protocol.LanguageIdentifier("ruby")
	case ".swift":
		return protocol.LanguageIdentifier("swift")
	case ".json":
		return protocol.JSONLanguage
	case ".html":
		return protocol.LanguageIdentifier("html")
	case ".css":
		return protocol.LanguageIdentifier("css")
	default:
		return protocol.LanguageIdentifier("plaintext")
	}
}

// Close requests an orderly LSP shutdown (shutdown + exit), closes the
// connection and stdio, and waits for the subprocess to exit. Errors from each
// stage are joined into the returned error; a best-effort shutdown still proceeds
// to Close the connection.
func (c *Client) Close(ctx context.Context) error {
	var firstErr error
	record := func(err error) {
		if err != nil && firstErr == nil {
			firstErr = err
		}
	}

	if c.conn != nil {
		if _, err := c.conn.Call(ctx, protocol.MethodShutdown, nil, nil); err != nil {
			record(fmt.Errorf("lsp: shutdown: %w", err))
		}
		if err := c.conn.Notify(ctx, protocol.MethodExit, nil); err != nil {
			record(fmt.Errorf("lsp: exit: %w", err))
		}
		if err := c.conn.Close(); err != nil {
			record(fmt.Errorf("lsp: close conn: %w", err))
		}
	}
	if c.stds != nil {
		if err := c.stds.Close(); err != nil {
			record(fmt.Errorf("lsp: close stdio: %w", err))
		}
	}
	if c.cmd != nil && c.cmd.Process != nil {
		if err := c.cmd.Wait(); err != nil {
			record(fmt.Errorf("lsp: wait: %w", err))
		}
	}
	return firstErr
}
