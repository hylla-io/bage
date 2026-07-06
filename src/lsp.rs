//! LSP bridge — a synchronous JSON-RPC client over a language server's stdio
//! plus the pure conversion from LSP positions to Båge's byte-range edit model.
//!
//! Port of Go `internal/lsp`. The load-bearing part is [`byte_offset`] (LSP
//! UTF-16 code-unit positions → UTF-8 byte offsets, surrogate-pair aware) and
//! [`workspace_edit_to_file_edits`] (a `WorkspaceEdit` flattened to
//! [`FileEdit`]s), centralizing the UTF-16↔UTF-8 conversion at the single LSP
//! boundary so the rest of Båge stays byte-addressed. [`Client`] is glue: it
//! runs on std threads (no async runtime), with a reader thread that parses
//! Content-Length framing and routes responses to pending calls and
//! `textDocument/publishDiagnostics` notifications into a bounded queue that
//! DROPS on overflow rather than ever blocking the read loop.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use lsp_types as lt;
use serde_json::{Value, json};
use thiserror::Error;

use crate::edit::FileEdit;

/// Depth of the publishDiagnostics queue. A server may publish several rounds
/// (initial + refined) before a `diagnostics` call collects; a small buffer
/// keeps the read loop from blocking without unbounded growth. Excess
/// notifications past the buffer are dropped (the latest authoritative set is
/// what matters), never blocking the read loop.
const DIAG_BUFFER: usize = 8;

/// Bounds how long `rename` waits for a still-indexing language server to
/// become ready before giving up.
const DEFAULT_RENAME_DEADLINE: Duration = Duration::from_secs(30);

/// Pause between rename attempts while waiting for the server to become ready.
const DEFAULT_RENAME_RETRY: Duration = Duration::from_millis(300);

/// Per-request bound on waiting for a JSON-RPC response, so a dead server
/// surfaces as a timeout rather than a hang (the Go client relies on a
/// caller-supplied context for the same purpose).
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors surfaced by the LSP boundary.
#[derive(Debug, Error)]
pub enum LspError {
    /// The source contains an invalid UTF-8 sequence at the given byte.
    #[error("lsp: malformed UTF-8 at byte {0}")]
    MalformedUtf8(usize),
    /// Reading a file referenced by a WorkspaceEdit failed.
    #[error("lsp: read {path:?}: {source}")]
    Read {
        /// The file that could not be read.
        path: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// `new_stdio` was given an empty server command.
    #[error("lsp: empty server command")]
    EmptyCommand,
    /// Spawning the language-server subprocess failed.
    #[error("lsp: start {command:?}: {source}")]
    Spawn {
        /// The program that failed to start.
        command: String,
        /// The underlying spawn error.
        source: io::Error,
    },
    /// The server answered a request with a JSON-RPC error.
    #[error("lsp: {method}: {message}")]
    Rpc {
        /// The request method.
        method: String,
        /// The server's error message.
        message: String,
    },
    /// The connection closed while a request was outstanding.
    #[error("lsp: {method}: connection closed")]
    Closed {
        /// The request method.
        method: String,
    },
    /// No response arrived within the per-call bound.
    #[error("lsp: {method}: no response after {after:?}")]
    Timeout {
        /// The request method.
        method: String,
        /// How long the call waited.
        after: Duration,
    },
    /// The rename retry loop exhausted its deadline against a server that
    /// never became ready.
    #[error("lsp: rename {path:?}: not ready after {after:?}: {last}")]
    RenameDeadline {
        /// The file the rename targeted.
        path: String,
        /// The configured rename deadline.
        after: Duration,
        /// The last not-ready error observed.
        last: String,
    },
    /// The server never published diagnostics within the timeout.
    #[error("lsp: awaiting diagnostics for {path:?}: no publish after {after:?}")]
    DiagnosticsTimeout {
        /// The file diagnostics were awaited for.
        path: String,
        /// How long the wait lasted.
        after: Duration,
    },
    /// Writing to or managing the transport failed.
    #[error("lsp: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Pure conversion (Go convert.go)
// ---------------------------------------------------------------------------

/// Maps a zero-based LSP position to a UTF-8 byte offset within `src`.
///
/// An LSP position is (line, character) where `line` counts '\n'-terminated
/// lines from zero and `character` counts UTF-16 code units from the line
/// start: an astral rune (> U+FFFF) is a surrogate pair and counts as TWO
/// units, everything else as one. The walk advances whole chars, debiting the
/// UTF-16 budget, so a budget landing inside a surrogate pair clamps forward
/// to the char's end.
///
/// Clamping follows the LSP spec: a line beyond the last resolves to the end
/// of `src`; a character beyond the line's content resolves to the line end
/// (the terminating '\n' is never crossed). The only rejected input is a
/// malformed UTF-8 sequence encountered while consuming characters on the
/// target line.
pub fn byte_offset(src: &[u8], line: u32, character: u32) -> Result<usize, LspError> {
    // Phase 1: walk to the start byte of the target line by counting newlines.
    let mut line_start = 0usize;
    let mut ln = 0u32;
    while ln < line {
        match src[line_start..].iter().position(|&b| b == b'\n') {
            // Line beyond EOF: clamp to end of src.
            None => return Ok(src.len()),
            Some(nl) => {
                line_start += nl + 1;
                ln += 1;
            }
        }
    }

    // Phase 2: consume `character` UTF-16 code units along this line,
    // advancing one char at a time. Stop at the terminating '\n' or EOF.
    let mut offset = line_start;
    let mut consumed = 0u32;
    while consumed < character && offset < src.len() {
        if src[offset] == b'\n' {
            // Character index past the line's content: clamp to line end.
            return Ok(offset);
        }
        let (c, size) = decode_char(&src[offset..]).ok_or(LspError::MalformedUtf8(offset))?;
        consumed += c.len_utf16() as u32;
        offset += size;
    }
    Ok(offset)
}

/// Decodes the first UTF-8 char in `bytes`, returning it with its byte width,
/// or `None` for an invalid or truncated sequence (the Rust shape of Go's
/// `utf8.DecodeRune` returning `(RuneError, 1)`).
fn decode_char(bytes: &[u8]) -> Option<(char, usize)> {
    let take = bytes.len().min(4);
    let valid = match std::str::from_utf8(&bytes[..take]) {
        Ok(s) => s,
        Err(e) if e.valid_up_to() > 0 => {
            std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("validated prefix")
        }
        Err(_) => return None,
    };
    valid.chars().next().map(|c| (c, c.len_utf8()))
}

/// Flattens a `WorkspaceEdit` into [`FileEdit`]s. Both representations are
/// honored: the legacy `changes` map and the versioned `document_changes`
/// (resource operations inside `DocumentChanges::Operations` carry no text
/// edits and are skipped). For each text edit the file's current bytes are
/// obtained via the injected `read` function (invoked at most once per
/// distinct file) and the edit's UTF-16 range is converted to byte offsets via
/// [`byte_offset`]. URIs resolve to filesystem paths with percent-decoding.
/// Order mirrors Go: `changes` first (map iteration order — downstream
/// `splice_edits` sorts per file), then `document_changes`.
pub fn workspace_edit_to_file_edits(
    we: &lt::WorkspaceEdit,
    mut read: impl FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<Vec<FileEdit>, LspError> {
    let mut out: Vec<FileEdit> = Vec::new();
    let mut cache: HashMap<String, Vec<u8>> = HashMap::new();

    let mut convert =
        |path: &str, edits: &[lt::TextEdit], out: &mut Vec<FileEdit>| -> Result<(), LspError> {
            if !cache.contains_key(path) {
                let bytes = read(path).map_err(|source| LspError::Read {
                    path: path.to_string(),
                    source,
                })?;
                cache.insert(path.to_string(), bytes);
            }
            let src = &cache[path];
            for e in edits {
                let start = byte_offset(src, e.range.start.line, e.range.start.character)?;
                let end = byte_offset(src, e.range.end.line, e.range.end.character)?;
                out.push(FileEdit {
                    path: path.to_string(),
                    start_byte: start,
                    end_byte: end,
                    new_text: e.new_text.clone(),
                });
            }
            Ok(())
        };

    if let Some(changes) = &we.changes {
        for (uri, edits) in changes {
            convert(&uri_to_path(uri), edits, &mut out)?;
        }
    }
    if let Some(doc_changes) = &we.document_changes {
        let tdes: Vec<&lt::TextDocumentEdit> = match doc_changes {
            lt::DocumentChanges::Edits(edits) => edits.iter().collect(),
            lt::DocumentChanges::Operations(ops) => ops
                .iter()
                .filter_map(|op| match op {
                    lt::DocumentChangeOperation::Edit(tde) => Some(tde),
                    lt::DocumentChangeOperation::Op(_) => None,
                })
                .collect(),
        };
        for tde in tdes {
            let edits: Vec<lt::TextEdit> = tde
                .edits
                .iter()
                .map(|e| match e {
                    lt::OneOf::Left(te) => te.clone(),
                    lt::OneOf::Right(annotated) => annotated.text_edit.clone(),
                })
                .collect();
            convert(&uri_to_path(&tde.text_document.uri), &edits, &mut out)?;
        }
    }
    Ok(out)
}

/// Reports whether `we` carries at least one edit. An empty `WorkspaceEdit`
/// from a rename means the server has not yet resolved the symbol's
/// references (still indexing); [`Client::rename`] treats that as not-ready
/// and retries.
fn workspace_edit_has_changes(we: &lt::WorkspaceEdit) -> bool {
    if we.changes.as_ref().is_some_and(|m| !m.is_empty()) {
        return true;
    }
    match &we.document_changes {
        Some(lt::DocumentChanges::Edits(v)) => !v.is_empty(),
        Some(lt::DocumentChanges::Operations(v)) => !v.is_empty(),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// URI helpers (Go go.lsp.dev/uri equivalents)
// ---------------------------------------------------------------------------

/// Builds a `file://` URI for `path`, percent-encoding bytes outside the URI
/// path-safe set (mirrors Go `uri.File`: a space becomes `%20`, `#` becomes
/// `%23`, while `/`, `+`, `=`, `:` and friends stay literal).
pub fn file_uri(path: &str) -> lt::Uri {
    let mut out = String::with_capacity(path.len() + 7);
    out.push_str("file://");
    for &b in path.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'/'
                    | b'$'
                    | b'&'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
                    | b'!'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.parse().expect("percent-encoded file URI is valid")
}

/// Resolves a `file://` URI back to a filesystem path, percent-decoding the
/// escaped bytes (mirrors Go `DocumentURI.Filename`). A non-file URI is
/// returned as-is minus its scheme, best-effort.
pub fn uri_to_path(uri: &lt::Uri) -> String {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://").unwrap_or(s);
    // Strip a non-empty authority (file://host/path); file:///path has an
    // empty authority and starts directly with '/'.
    let path = if rest.starts_with('/') {
        rest
    } else {
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => rest,
        }
    };
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                bytes.push(b);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Maps a file path's extension to the LSP `textDocument` languageId the
/// server expects in didOpen. A wrong languageId makes a server skip
/// analysis; unknown extensions fall back to "plaintext".
fn language_id_for_path(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "go" => "go",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "jsx" => "javascript",
        "rs" => "rust",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "swift" => "swift",
        "json" => "json",
        "html" => "html",
        "css" => "css",
        _ => "plaintext",
    }
}

// ---------------------------------------------------------------------------
// Diagnostics reporting shape (Go diagnostics.go)
// ---------------------------------------------------------------------------

/// One server-reported problem, flattened into Båge's reporting shape: a
/// human severity string, the 1-based line/col range of the offending span,
/// the message, and the diagnostic source (e.g. "compiler"). Lines and
/// columns are 1-based, converted from the LSP wire protocol's 0-based
/// positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Human severity label ("Error", "Warning", "Information", "Hint", or a
    /// numeric fallback for an unknown code; empty when the server omits it).
    pub severity: String,
    /// The diagnostic's origin (server-provided; may be empty).
    pub source: String,
    /// The diagnostic text.
    pub message: String,
    /// 1-based start line of the diagnostic range.
    pub start_line: usize,
    /// 1-based start column of the diagnostic range.
    pub start_col: usize,
    /// 1-based end line of the diagnostic range.
    pub end_line: usize,
    /// 1-based end column of the diagnostic range.
    pub end_col: usize,
}

/// Flattens an LSP diagnostic into Båge's reporting shape, converting the
/// 0-based wire positions to 1-based line/col.
fn to_diagnostic(d: &lt::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: severity_label(d.severity),
        source: d.source.clone().unwrap_or_default(),
        message: d.message.clone(),
        start_line: d.range.start.line as usize + 1,
        start_col: d.range.start.character as usize + 1,
        end_line: d.range.end.line as usize + 1,
        end_col: d.range.end.character as usize + 1,
    }
}

/// Maps an LSP severity to its human label, with a numeric fallback for
/// unknown codes and an empty string when the server omitted it.
fn severity_label(sev: Option<lt::DiagnosticSeverity>) -> String {
    let Some(sev) = sev else {
        return String::new();
    };
    match sev {
        lt::DiagnosticSeverity::ERROR => "Error".to_string(),
        lt::DiagnosticSeverity::WARNING => "Warning".to_string(),
        lt::DiagnosticSeverity::INFORMATION => "Information".to_string(),
        lt::DiagnosticSeverity::HINT => "Hint".to_string(),
        other => serde_json::to_value(other)
            .map(|v| v.to_string())
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC framing
// ---------------------------------------------------------------------------

/// Writes one Content-Length-framed JSON-RPC message.
fn write_frame(w: &mut dyn Write, msg: &Value) -> io::Result<()> {
    let body =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Reads one Content-Length-framed message body. Returns `Ok(None)` on clean
/// EOF. A frame without a parseable Content-Length yields an empty body,
/// which the caller's JSON parse rejects and skips — the read loop never
/// panics on a malformed peer.
fn read_frame(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let Some(len) = content_length else {
        return Ok(Some(Vec::new()));
    };
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

// ---------------------------------------------------------------------------
// Client (Go client.go)
// ---------------------------------------------------------------------------

/// A JSON-RPC response payload routed from the read loop to a waiting call.
enum RpcOutcome {
    /// The `result` member (possibly `Value::Null`).
    Ok(Value),
    /// The server's `error.message`.
    Err(String),
    /// The connection closed before a response arrived.
    Closed,
}

/// The table of in-flight requests, keyed by request id.
type PendingMap = Arc<Mutex<HashMap<u64, Sender<RpcOutcome>>>>;

/// A thin synchronous LSP client over a spawned language-server subprocess
/// (or any Read/Write pair via [`Client::from_conn`]). It exposes only the
/// minimal surface Båge needs: lifecycle (`initialize`/`close`), symbol
/// `rename` with a still-indexing retry loop, and `diagnostics`. All
/// byte-offset conversion lives in the pure functions above; this type is
/// glue. A `Client` is not safe for concurrent use.
pub struct Client {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: PendingMap,
    diags: Receiver<Vec<lt::Diagnostic>>,
    next_id: u64,
    ver: i32,
    child: Option<Child>,
    /// Bounds how long `rename` retries a still-indexing server (overridable
    /// in tests).
    pub rename_deadline: Duration,
    /// Pause between rename attempts (overridable in tests).
    pub rename_retry: Duration,
    /// Per-request response bound.
    pub call_timeout: Duration,
}

impl Client {
    /// Spawns the LSP server described by `command` (e.g. `["gopls"]`) and
    /// wires the client over its stdio. The read loop starts immediately;
    /// incoming server-to-client requests are answered with method-not-found,
    /// sufficient for the rename path. Call [`Client::close`] to release the
    /// subprocess (Drop kills it as a backstop).
    pub fn new_stdio(command: &[String]) -> Result<Client, LspError> {
        let (program, args) = command.split_first().ok_or(LspError::EmptyCommand)?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| LspError::Spawn {
                command: program.clone(),
                source,
            })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut client = Client::from_conn(stdout, stdin);
        client.child = Some(child);
        Ok(client)
    }

    /// Wires a client over an arbitrary bidirectional transport — the single
    /// transport seam: `new_stdio` supplies a subprocess's stdio while tests
    /// (or a socket caller) supply any Read/Write pair. The read loop starts
    /// immediately on its own thread.
    pub fn from_conn(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> Client {
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(writer)));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (diag_tx, diag_rx) = mpsc::sync_channel(DIAG_BUFFER);
        {
            let writer = Arc::clone(&writer);
            let pending = Arc::clone(&pending);
            thread::spawn(move || read_loop(Box::new(reader), writer, pending, diag_tx));
        }
        Client {
            writer,
            pending,
            diags: diag_rx,
            next_id: 0,
            ver: 0,
            child: None,
            rename_deadline: DEFAULT_RENAME_DEADLINE,
            rename_retry: DEFAULT_RENAME_RETRY,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// Sends one request and blocks for its response (bounded by `timeout`).
    fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, LspError> {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);

        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = write_frame(lock(&self.writer).as_mut(), &req) {
            lock(&self.pending).remove(&id);
            return Err(LspError::Io(e));
        }

        match rx.recv_timeout(timeout) {
            Ok(RpcOutcome::Ok(v)) => Ok(v),
            Ok(RpcOutcome::Err(message)) => Err(LspError::Rpc {
                method: method.to_string(),
                message,
            }),
            Ok(RpcOutcome::Closed) | Err(RecvTimeoutError::Disconnected) => Err(LspError::Closed {
                method: method.to_string(),
            }),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.pending).remove(&id);
                Err(LspError::Timeout {
                    method: method.to_string(),
                    after: timeout,
                })
            }
        }
    }

    /// Sends one notification (no response expected).
    fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        write_frame(lock(&self.writer).as_mut(), &msg).map_err(LspError::Io)
    }

    /// Performs the LSP initialize/initialized handshake rooted at `root_uri`
    /// (a `file://` URI for the workspace root).
    pub fn initialize(&mut self, root_uri: &str) -> Result<(), LspError> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {"workspaceEdit": {"documentChanges": true}}
            },
        });
        self.call("initialize", params, self.call_timeout)?;
        self.notify("initialized", json!({}))
    }

    /// Opens `path` in the server via `textDocument/didOpen` with the given
    /// authoritative content.
    fn did_open(&mut self, path: &str, content: &str) -> Result<(), LspError> {
        self.ver += 1;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": file_uri(path),
                    "languageId": language_id_for_path(path),
                    "version": self.ver,
                    "text": content,
                },
            }),
        )
    }

    /// Opens the file at `path` (sending `content` via didOpen so the server
    /// has an authoritative view), requests a `textDocument/rename` of the
    /// symbol at the zero-based (line, col) UTF-16 position, and returns the
    /// server's `WorkspaceEdit` — convert it to byte offsets with
    /// [`workspace_edit_to_file_edits`].
    ///
    /// A language server still building its index (e.g. rust-analyzer on a
    /// cold crate) answers a rename before it can resolve references — either
    /// with an error or with an empty edit. Neither is a real "no references"
    /// verdict, so this retries until the server is ready or
    /// `rename_deadline` is spent, pausing `rename_retry` between attempts.
    pub fn rename(
        &mut self,
        path: &str,
        content: &str,
        line: u32,
        col: u32,
        new_name: &str,
    ) -> Result<lt::WorkspaceEdit, LspError> {
        self.did_open(path, content)?;
        let params = json!({
            "textDocument": {"uri": file_uri(path)},
            "position": {"line": line, "character": col},
            "newName": new_name,
        });

        let deadline = Instant::now() + self.rename_deadline;
        let mut last: String;
        loop {
            match self.call("textDocument/rename", params.clone(), self.call_timeout) {
                Err(e) => last = e.to_string(),
                Ok(v) => match serde_json::from_value::<Option<lt::WorkspaceEdit>>(v) {
                    Ok(Some(we)) if workspace_edit_has_changes(&we) => return Ok(we),
                    Ok(_) => last = "server returned no edits (still indexing?)".to_string(),
                    Err(e) => last = format!("decode WorkspaceEdit: {e}"),
                },
            }
            if Instant::now() > deadline {
                return Err(LspError::RenameDeadline {
                    path: path.to_string(),
                    after: self.rename_deadline,
                    last,
                });
            }
            thread::sleep(self.rename_retry);
        }
    }

    /// Opens `path` in the server (didOpen with `content`) and collects the
    /// first `textDocument/publishDiagnostics` notification the server pushes,
    /// mapping each entry into Båge's reporting shape. The result arrives as
    /// a server→client NOTIFICATION (not a request response), so it is
    /// gathered from the read loop via the bounded diagnostics queue. Blocks
    /// until the server publishes or `timeout` elapses; an empty publish (a
    /// clean file) returns an empty vec.
    pub fn diagnostics(
        &mut self,
        path: &str,
        content: &str,
        timeout: Duration,
    ) -> Result<Vec<Diagnostic>, LspError> {
        self.did_open(path, content)?;
        match self.diags.recv_timeout(timeout) {
            Ok(raw) => Ok(raw.iter().map(to_diagnostic).collect()),
            Err(_) => Err(LspError::DiagnosticsTimeout {
                path: path.to_string(),
                after: timeout,
            }),
        }
    }

    /// Requests an orderly LSP shutdown (shutdown + exit) and reaps the
    /// subprocess, killing it if it does not exit promptly. Best-effort: a
    /// failed shutdown still proceeds to exit and reaping, and the first
    /// error encountered is returned.
    pub fn close(&mut self) -> Result<(), LspError> {
        let shutdown = self.call("shutdown", Value::Null, Duration::from_secs(2));
        let exit = self.notify("exit", Value::Null);
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        match (shutdown, exit) {
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

impl Drop for Client {
    /// Backstop: kill and reap the server subprocess if `close` was skipped.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Locks a mutex, recovering the guard from a poisoned lock — a panicked
/// sibling thread must not cascade panics through the client.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The connection's read loop: parses Content-Length frames and routes each
/// message. Responses are matched to pending requests by id;
/// `textDocument/publishDiagnostics` notifications are forwarded into the
/// bounded diagnostics queue with `try_send` so a full buffer DROPS the
/// message and never blocks the loop; other server→client requests are
/// answered with method-not-found; malformed messages are skipped. On
/// connection loss every pending call is failed with `Closed`.
fn read_loop(
    reader: Box<dyn Read + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: PendingMap,
    diags: SyncSender<Vec<lt::Diagnostic>>,
) {
    let mut r = BufReader::new(reader);
    // Run until EOF or a transport error: the connection is gone.
    while let Ok(Some(body)) = read_frame(&mut r) {
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            continue; // Malformed body: skip, never stall the loop.
        };
        let Some(obj) = msg.as_object() else { continue };

        if let Some(method) = obj.get("method").and_then(Value::as_str) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = obj.get("params")
                    && let Ok(p) =
                        serde_json::from_value::<lt::PublishDiagnosticsParams>(params.clone())
                {
                    // Buffer full: drop rather than block the read loop.
                    let _ = diags.try_send(p.diagnostics);
                }
                continue;
            }
            // A server→client REQUEST (has an id) gets method-not-found; a
            // notification is silently acknowledged.
            if let Some(id) = obj.get("id") {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {method}")},
                });
                let _ = write_frame(lock(&writer).as_mut(), &resp);
            }
            continue;
        }

        // A response: route to the waiting call by id.
        if let Some(id) = obj.get("id").and_then(Value::as_u64)
            && let Some(tx) = lock(&pending).remove(&id)
        {
            let outcome = match obj.get("error") {
                Some(e) if !e.is_null() => RpcOutcome::Err(
                    e.get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| e.to_string()),
                ),
                _ => RpcOutcome::Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(outcome);
        }
    }
    // Connection gone: fail any callers still waiting.
    for (_, tx) in lock(&pending).drain() {
        let _ = tx.send(RpcOutcome::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::splice_edits;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ---- byte_offset (Go convert_test.go TestByteOffset) ----

    #[test]
    fn byte_offset_table() {
        let cases: &[(&str, &str, u32, u32, usize)] = &[
            // ASCII, single line.
            ("ascii start", "hello", 0, 0, 0),
            ("ascii mid", "hello", 0, 3, 3),
            ("ascii end", "hello", 0, 5, 5),
            // Multi-byte UTF-8: "é" is 2 bytes, 1 UTF-16 unit.
            ("utf8 before accent", "café", 0, 3, 3),
            ("utf8 after accent", "café", 0, 4, 5),
            ("utf8 accent first", "é=2", 0, 1, 2),
            // Astral rune: "𝛂" U+1D6C2 is 4 UTF-8 bytes, 2 UTF-16 units.
            ("astral past pair", "𝛂x", 0, 2, 4),
            // A budget of 1 unit cannot split the surrogate pair: the walk
            // advances the whole rune, clamping forward to byte 4.
            ("astral inside pair clamps forward", "𝛂x", 0, 1, 4),
            ("emoji past pair", "😀!", 0, 2, 4),
            // Multi-line.
            ("line 1 start", "ab\ncd\nef", 1, 0, 3),
            ("line 1 mid", "ab\ncd\nef", 1, 1, 4),
            ("line 2 start", "ab\ncd\nef", 2, 0, 6),
            ("line 2 end", "ab\ncd\nef", 2, 2, 8),
            ("multibyte on line 1", "x\né!", 1, 1, 4),
            // Clamping.
            ("char past line end mid-file", "ab\ncd", 0, 99, 2),
            ("char past line end last line", "ab\ncd", 1, 99, 5),
            ("line past EOF", "ab\ncd", 9, 0, 5),
            ("line just past EOF no trailing nl", "ab", 1, 0, 2),
            ("empty trailing line", "ab\n", 1, 0, 3),
            // Edge: empty src.
            ("empty src zero pos", "", 0, 0, 0),
            ("empty src line past", "", 3, 0, 0),
        ];
        for &(name, src, line, ch, want) in cases {
            let got = byte_offset(src.as_bytes(), line, ch)
                .unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
            assert_eq!(got, want, "{name}: byte_offset({src:?}, {line}, {ch})");
        }
    }

    #[test]
    fn byte_offset_malformed_utf8() {
        // 0xFF is never a valid UTF-8 lead byte: walking onto it on the
        // target line must be rejected rather than silently mis-counted.
        let src = [b'a', 0xFF, b'b'];
        assert!(matches!(
            byte_offset(&src, 0, 2),
            Err(LspError::MalformedUtf8(1))
        ));
        // A position that stops before the bad byte is fine.
        assert_eq!(byte_offset(&src, 0, 1).unwrap(), 1);
    }

    // ---- edge cases (Go edge_test.go) ----

    #[test]
    fn byte_offset_crlf() {
        // "ab\r\ncd" — the '\r' is ordinary one-unit one-byte content; only
        // the '\n' terminates the line.
        let src = b"ab\r\ncd";
        for &(name, line, ch, want) in &[
            ("before cr", 0u32, 2u32, 2usize),
            ("cr counts as one unit", 0, 3, 3),
            // Past the content clamps to the terminating '\n' (byte 3).
            ("past content clamps to nl", 0, 99, 3),
            ("next line start", 1, 0, 4),
            ("next line mid", 1, 1, 5),
        ] {
            assert_eq!(byte_offset(src, line, ch).unwrap(), want, "{name}");
        }
    }

    #[test]
    fn byte_offset_leading_bom() {
        // A leading UTF-8 BOM (EF BB BF, U+FEFF) is one UTF-16 code unit
        // occupying three bytes.
        let mut src = vec![0xEF, 0xBB, 0xBF];
        src.extend_from_slice(b"xy");
        assert_eq!(src.len(), 5, "fixture sanity");
        for &(name, ch, want) in &[
            ("at bom", 0u32, 0usize),
            ("after bom (one unit, three bytes)", 1, 3),
            ("after x", 2, 4),
            ("after y", 3, 5),
        ] {
            assert_eq!(byte_offset(&src, 0, ch).unwrap(), want, "{name}");
        }
    }

    #[test]
    fn byte_offset_astral_boundary() {
        // "😀😁" — each emoji is 4 bytes / 2 units; the only rune boundaries
        // are at char 0, 2, and 4. Char 2 lands exactly on the seam.
        let src = "😀😁".as_bytes();
        assert_eq!(src.len(), 8, "fixture sanity");
        assert_eq!(byte_offset(src, 0, 0).unwrap(), 0);
        assert_eq!(byte_offset(src, 0, 2).unwrap(), 4);
        assert_eq!(byte_offset(src, 0, 4).unwrap(), 8);
    }

    // ---- workspace_edit_to_file_edits (Go convert_test.go) ----

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lt::Range {
        lt::Range {
            start: lt::Position {
                line: sl,
                character: sc,
            },
            end: lt::Position {
                line: el,
                character: ec,
            },
        }
    }

    fn text_edit(sl: u32, sc: u32, el: u32, ec: u32, new_text: &str) -> lt::TextEdit {
        lt::TextEdit {
            range: range(sl, sc, el, ec),
            new_text: new_text.to_string(),
        }
    }

    fn doc_edit(uri: &lt::Uri, edits: Vec<lt::TextEdit>) -> lt::TextDocumentEdit {
        lt::TextDocumentEdit {
            text_document: lt::OptionalVersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: None,
            },
            edits: edits.into_iter().map(lt::OneOf::Left).collect(),
        }
    }

    /// Deterministic order for comparing flattened edits regardless of the
    /// changes map's iteration order.
    fn sort_edits(edits: &mut [FileEdit]) {
        edits.sort_by(|a, b| {
            (&a.path, a.start_byte, &a.new_text).cmp(&(&b.path, b.start_byte, &b.new_text))
        });
    }

    fn fixture_read(path: &str) -> io::Result<Vec<u8>> {
        match path {
            // "func café()" — é is 2 bytes (8–9), so char 9 is past it.
            "/tmp/foo.go" => Ok(b"func caf\xc3\xa9()\n".to_vec()),
            // "x := 𝛂" — 𝛂 is bytes 5..9, chars 5..7.
            "/tmp/bar.go" => Ok("x := 𝛂\n".as_bytes().to_vec()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "not found")),
        }
    }

    fn edit(path: &str, start: usize, end: usize, new_text: &str) -> FileEdit {
        FileEdit {
            path: path.to_string(),
            start_byte: start,
            end_byte: end,
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn workspace_edit_changes_map_single_file_multiple_edits() {
        let foo = file_uri("/tmp/foo.go");
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([(
                foo,
                vec![text_edit(0, 0, 0, 4, "FUNC"), text_edit(0, 5, 0, 9, "cafe")],
            )])),
            ..Default::default()
        };
        let mut got = workspace_edit_to_file_edits(&we, fixture_read).unwrap();
        sort_edits(&mut got);
        assert_eq!(
            got,
            vec![
                edit("/tmp/foo.go", 0, 4, "FUNC"),
                // char 5..9 spans "café" → bytes 5..10.
                edit("/tmp/foo.go", 5, 10, "cafe"),
            ]
        );
    }

    #[test]
    fn workspace_edit_multiple_files_in_changes() {
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([
                (file_uri("/tmp/foo.go"), vec![text_edit(0, 0, 0, 4, "F")]),
                (file_uri("/tmp/bar.go"), vec![text_edit(0, 0, 0, 1, "y")]),
            ])),
            ..Default::default()
        };
        let mut got = workspace_edit_to_file_edits(&we, fixture_read).unwrap();
        sort_edits(&mut got);
        assert_eq!(
            got,
            vec![
                edit("/tmp/bar.go", 0, 1, "y"),
                edit("/tmp/foo.go", 0, 4, "F")
            ]
        );
    }

    #[test]
    fn workspace_edit_astral_range() {
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([(
                file_uri("/tmp/bar.go"),
                vec![text_edit(0, 5, 0, 7, "Z")],
            )])),
            ..Default::default()
        };
        let got = workspace_edit_to_file_edits(&we, fixture_read).unwrap();
        assert_eq!(got, vec![edit("/tmp/bar.go", 5, 9, "Z")]);
    }

    #[test]
    fn workspace_edit_document_changes_form() {
        let foo = file_uri("/tmp/foo.go");
        let we = lt::WorkspaceEdit {
            document_changes: Some(lt::DocumentChanges::Edits(vec![doc_edit(
                &foo,
                vec![text_edit(0, 5, 0, 9, "cafe")],
            )])),
            ..Default::default()
        };
        let got = workspace_edit_to_file_edits(&we, fixture_read).unwrap();
        assert_eq!(got, vec![edit("/tmp/foo.go", 5, 10, "cafe")]);
    }

    #[test]
    fn workspace_edit_changes_and_document_changes_combined() {
        let bar = file_uri("/tmp/bar.go");
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([(
                file_uri("/tmp/foo.go"),
                vec![text_edit(0, 0, 0, 4, "F")],
            )])),
            document_changes: Some(lt::DocumentChanges::Edits(vec![doc_edit(
                &bar,
                vec![text_edit(0, 0, 0, 1, "y")],
            )])),
            ..Default::default()
        };
        let mut got = workspace_edit_to_file_edits(&we, fixture_read).unwrap();
        sort_edits(&mut got);
        assert_eq!(
            got,
            vec![
                edit("/tmp/bar.go", 0, 1, "y"),
                edit("/tmp/foo.go", 0, 4, "F")
            ]
        );
    }

    #[test]
    fn workspace_edit_read_error_is_wrapped() {
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([(
                file_uri("/tmp/missing.go"),
                vec![text_edit(0, 0, 0, 1, "z")],
            )])),
            ..Default::default()
        };
        let err = workspace_edit_to_file_edits(&we, fixture_read).unwrap_err();
        assert!(matches!(err, LspError::Read { .. }), "got {err}");
    }

    // Go edge_test.go TestWorkspaceEditDualSourceOverlapRejected: the same
    // URI in BOTH changes AND document_changes emits duplicate FileEdits (no
    // dedupe here — that is a downstream apply concern), and the splice layer
    // REJECTS the overlap rather than applying it twice.
    #[test]
    fn workspace_edit_dual_source_overlap_rejected() {
        let uri = file_uri("/tmp/dup.go");
        let src = b"func old() {}\n";
        let read = |path: &str| -> io::Result<Vec<u8>> {
            assert_eq!(path, "/tmp/dup.go", "unexpected read");
            Ok(src.to_vec())
        };
        let edits = vec![text_edit(0, 5, 0, 8, "new")]; // "old"
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([(uri.clone(), edits.clone())])),
            document_changes: Some(lt::DocumentChanges::Edits(vec![doc_edit(&uri, edits)])),
            ..Default::default()
        };
        let got = workspace_edit_to_file_edits(&we, read).unwrap();
        assert_eq!(got.len(), 2, "expected duplicate edits from dual source");
        assert_eq!(got[0], got[1]);
        // The load-bearing guarantee: splice rejects the overlap.
        let err = splice_edits(src, &got).unwrap_err();
        assert!(err.to_string().contains("overlap"), "got {err}");
    }

    // Go edge_test.go TestWorkspaceEditURIDecodesSpecialPath: URI→path
    // decoding round-trips a path with a space and percent-prone characters.
    #[test]
    fn workspace_edit_uri_decodes_special_path() {
        let want_path = "/tmp/my dir/a+b#c.go";
        let uri = file_uri(want_path);
        assert!(
            uri.as_str().contains("%20"),
            "fixture sanity: space percent-encoded in {uri:?}"
        );
        let read_paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let rp = Arc::clone(&read_paths);
        let read = move |path: &str| -> io::Result<Vec<u8>> {
            rp.lock().unwrap().push(path.to_string());
            Ok(b"var x = 1\n".to_vec())
        };
        let we = lt::WorkspaceEdit {
            changes: Some(HashMap::from([(uri, vec![text_edit(0, 4, 0, 5, "y")])])),
            ..Default::default()
        };
        let got = workspace_edit_to_file_edits(&we, read).unwrap();
        assert_eq!(read_paths.lock().unwrap().as_slice(), [want_path]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, want_path);
    }

    // ---- in-memory duplex transport for client tests ----

    struct PipeWriter {
        tx: Sender<Vec<u8>>,
    }

    impl Write for PipeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.tx
                .send(buf.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "peer gone"))?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct PipeReader {
        rx: Receiver<Vec<u8>>,
        buf: Vec<u8>,
        pos: usize,
    }

    impl Read for PipeReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            while self.pos >= self.buf.len() {
                match self.rx.recv() {
                    Ok(chunk) => {
                        self.buf = chunk;
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0), // peer gone = EOF
                }
            }
            let n = out.len().min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn pipe() -> (PipeWriter, PipeReader) {
        let (tx, rx) = mpsc::channel();
        (
            PipeWriter { tx },
            PipeReader {
                rx,
                buf: Vec::new(),
                pos: 0,
            },
        )
    }

    /// Builds an in-memory duplex connection: returns the client's
    /// (reader, writer) and the server's (reader, writer).
    fn conn_pair() -> ((PipeReader, PipeWriter), (PipeReader, PipeWriter)) {
        let (c2s_w, c2s_r) = pipe();
        let (s2c_w, s2c_r) = pipe();
        ((s2c_r, c2s_w), (c2s_r, s2c_w))
    }

    fn reply_ok(w: &mut PipeWriter, id: &Value, result: Value) {
        write_frame(w, &json!({"jsonrpc": "2.0", "id": id, "result": result})).unwrap();
    }

    fn reply_err(w: &mut PipeWriter, id: &Value, message: &str) {
        write_frame(
            w,
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32803, "message": message}}),
        )
        .unwrap();
    }

    /// A minimal fake LSP server: answers initialize/shutdown, pushes
    /// `diags_on_open` publishDiagnostics notifications on every didOpen, and
    /// delegates each textDocument/rename to `on_rename` (`Ok` → result,
    /// `Err` → JSON-RPC error, the not-yet-ready server shape).
    fn spawn_fake_server(
        server_conn: (PipeReader, PipeWriter),
        mut on_rename: impl FnMut() -> Result<Value, String> + Send + 'static,
        diags_on_open: Vec<Value>,
    ) {
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                match method {
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    "initialized" => {}
                    "textDocument/didOpen" => {
                        for params in &diags_on_open {
                            write_frame(
                                &mut w,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "method": "textDocument/publishDiagnostics",
                                    "params": params,
                                }),
                            )
                            .unwrap();
                        }
                    }
                    "textDocument/rename" => match on_rename() {
                        Ok(result) => reply_ok(&mut w, &id, result),
                        Err(message) => reply_err(&mut w, &id, &message),
                    },
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });
    }

    /// A minimal non-empty WorkspaceEdit result a ready server returns.
    fn ready_rename_edit() -> Value {
        json!({
            "changes": {
                "file:///work/main.rs": [{
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 7},
                    },
                    "newText": "renamed",
                }],
            },
        })
    }

    fn test_client(conn: (PipeReader, PipeWriter)) -> Client {
        let (reader, writer) = conn;
        let mut c = Client::from_conn(reader, writer);
        c.rename_retry = Duration::from_millis(5);
        c.rename_deadline = Duration::from_secs(2);
        c.call_timeout = Duration::from_secs(2);
        c
    }

    // ---- rename retry loop (Go rename_retry_test.go) ----

    #[test]
    fn rename_retries_until_server_ready() {
        let (client_conn, server_conn) = conn_pair();
        let calls = Arc::new(AtomicU32::new(0));
        let server_calls = Arc::clone(&calls);
        spawn_fake_server(
            server_conn,
            move || {
                let n = server_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 2 {
                    Err("server still indexing: no references found".to_string())
                } else {
                    Ok(ready_rename_edit())
                }
            },
            Vec::new(),
        );

        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        let we = c
            .rename("/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
            .expect("rename after retries");
        assert!(workspace_edit_has_changes(&we));
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "expected >= 3 attempts (2 not-ready + 1 ready), got {}",
            calls.load(Ordering::SeqCst)
        );
        let _ = c.close();
    }

    #[test]
    fn rename_retries_on_empty_edit() {
        // An empty but non-error rename response is also not-ready: some
        // servers answer during indexing with an empty edit, not an error.
        let (client_conn, server_conn) = conn_pair();
        let calls = Arc::new(AtomicU32::new(0));
        let server_calls = Arc::clone(&calls);
        spawn_fake_server(
            server_conn,
            move || {
                let n = server_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 2 {
                    Ok(json!({}))
                } else {
                    Ok(ready_rename_edit())
                }
            },
            Vec::new(),
        );

        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        let we = c
            .rename("/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
            .expect("rename after empty retries");
        assert!(workspace_edit_has_changes(&we));
        assert!(calls.load(Ordering::SeqCst) >= 3);
        let _ = c.close();
    }

    #[test]
    fn rename_deadline_exceeded() {
        // The retry loop is bounded: a server that never becomes ready makes
        // rename fail once the deadline is spent rather than hang.
        let (client_conn, server_conn) = conn_pair();
        spawn_fake_server(
            server_conn,
            || Err("server still indexing".to_string()),
            Vec::new(),
        );

        let mut c = test_client(client_conn);
        c.rename_deadline = Duration::from_millis(80);
        c.initialize("file:///work").unwrap();
        let err = c
            .rename("/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
            .unwrap_err();
        assert!(matches!(err, LspError::RenameDeadline { .. }), "got {err}");
        let _ = c.close();
    }

    // ---- diagnostics (Go diagnostics_test.go TestDiagnosticsInMemoryFake) ----

    #[test]
    fn diagnostics_in_memory_fake() {
        let (client_conn, server_conn) = conn_pair();
        let diag = json!({
            "uri": "file:///work/main.go",
            "diagnostics": [{
                "range": {
                    "start": {"line": 2, "character": 5},
                    "end": {"line": 2, "character": 10},
                },
                "severity": 1,
                "source": "fakelint",
                "message": "undefined: wobble",
            }],
        });
        spawn_fake_server(server_conn, || Ok(json!({})), vec![diag]);

        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        let got = c
            .diagnostics("/work/main.go", "package main\n", Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            got,
            vec![Diagnostic {
                severity: "Error".to_string(),
                source: "fakelint".to_string(),
                message: "undefined: wobble".to_string(),
                // 1-based reporting positions from 0-based wire positions.
                start_line: 3,
                start_col: 6,
                end_line: 3,
                end_col: 11,
            }]
        );
        let _ = c.close();
    }

    #[test]
    fn diagnostics_buffer_drops_on_full() {
        // The read loop forwards publishDiagnostics with try_send into a
        // queue of depth 8: a server flooding 20 rounds before anyone
        // collects must have the excess DROPPED, never blocking the loop.
        let (client_conn, server_conn) = conn_pair();
        let flood: Vec<Value> = (0..20)
            .map(|i| {
                json!({
                    "uri": "file:///work/main.go",
                    "diagnostics": [{
                        "range": {
                            "start": {"line": i, "character": 0},
                            "end": {"line": i, "character": 1},
                        },
                        "message": format!("round {i}"),
                    }],
                })
            })
            .collect();
        spawn_fake_server(server_conn, || Ok(json!({})), flood);

        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        c.did_open("/work/main.go", "package main\n").unwrap();
        // Sync point: a request round-trip proves the read loop has processed
        // every notification the server wrote before its reply.
        c.call("shutdown", Value::Null, Duration::from_secs(2))
            .unwrap();

        let mut received = 0;
        while c.diags.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(
            received, DIAG_BUFFER,
            "buffer must cap at {DIAG_BUFFER}, dropping the rest"
        );
    }

    // ---- resilience ----

    #[test]
    fn malformed_messages_are_skipped() {
        // Garbage frames (invalid JSON, bogus framing) must not kill the read
        // loop: a subsequent well-formed response still routes.
        let (client_conn, server_conn) = conn_pair();
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => {
                        // Invalid JSON body, correctly framed.
                        w.write_all(b"Content-Length: 9\r\n\r\nnot json!").unwrap();
                        // A frame with no Content-Length header at all.
                        w.write_all(b"X-Nonsense: yes\r\n\r\n").unwrap();
                        // A non-object JSON body.
                        write_frame(&mut w, &json!(["array", "not", "object"])).unwrap();
                        // Then the real response.
                        reply_ok(&mut w, &id, json!({"capabilities": {}}));
                    }
                    "exit" => break,
                    _ if !id.is_null() => reply_ok(&mut w, &id, Value::Null),
                    _ => {}
                }
            }
        });

        let mut c = test_client(client_conn);
        c.initialize("file:///work")
            .expect("initialize survives malformed frames");
        let _ = c.close();
    }

    #[test]
    fn call_fails_closed_when_connection_drops() {
        // A server that vanishes mid-request must fail the pending call with
        // Closed, not hang until timeout.
        let (client_conn, server_conn) = conn_pair();
        let (reader, w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            // Read one request then drop both ends.
            let _ = read_frame(&mut r);
            drop(w);
            drop(r);
        });
        let mut c = test_client(client_conn);
        let err = c
            .call("initialize", json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(err, LspError::Closed { .. }), "got {err}");
    }

    // ---- helpers ----

    #[test]
    fn language_id_mapping() {
        for (path, want) in [
            ("/a/b.go", "go"),
            ("x.py", "python"),
            ("x.ts", "typescript"),
            ("x.tsx", "typescriptreact"),
            ("x.js", "javascript"),
            ("x.jsx", "javascript"),
            ("x.rs", "rust"),
            ("x.java", "java"),
            ("x.c", "c"),
            ("x.h", "c"),
            ("x.cpp", "cpp"),
            ("x.cs", "csharp"),
            ("x.rb", "ruby"),
            ("x.swift", "swift"),
            ("x.json", "json"),
            ("x.html", "html"),
            ("x.css", "css"),
            ("x.weird", "plaintext"),
            ("noext", "plaintext"),
        ] {
            assert_eq!(language_id_for_path(path), want, "{path}");
        }
    }

    #[test]
    fn file_uri_round_trip() {
        for path in ["/tmp/a.go", "/tmp/my dir/a+b#c.go", "/tmp/café/λ.rs"] {
            let uri = file_uri(path);
            assert_eq!(uri_to_path(&uri), path, "round-trip {path:?} via {uri:?}");
        }
        assert_eq!(file_uri("/tmp/a b").as_str(), "file:///tmp/a%20b");
        assert_eq!(file_uri("/t/a#b.go").as_str(), "file:///t/a%23b.go");
    }
}
