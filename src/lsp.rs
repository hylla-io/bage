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
//!
//! CAUSALITY LIMIT of warm-pool push diagnostics (ruled DL-64/DL-65, accepted).
//! The push model ([`textDocument/publishDiagnostics`]) carries no token tying a
//! publish to the edit that caused it. The [ordering barrier][`BARRIER_METHOD`]
//! guarantees ARRIVAL order — every publish the server emitted BEFORE answering
//! the barrier lands ahead of the marker — but NOT CAUSALITY: a spec-legal
//! debounced server may flush a publish CAUSED by a pre-barrier edit AFTER it
//! answers the barrier, so [`Client::diagnostics`] can return that later
//! (possibly stale-clean) round as authoritative. This is best-effort by
//! construction; there is no push-side fix. The STRUCTURAL close is the
//! PULL-based [`textDocument/diagnostic`] request (LSP 3.17) — the caller pulls
//! and the server answers WITH a result-id, making the round causally bound —
//! routed to the B2 request-leg work. Servers lacking pull support keep the
//! arrival-ordered best-effort behavior here.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use lsp_types as lt;
use serde_json::{Value, json};
use thiserror::Error;

use crate::edit::FileEdit;

/// Max `publishDiagnostics` rounds buffered for a pending `diagnostics` call.
/// A server may publish several rounds (initial + refined, plus a warm-reuse
/// `didClose` "clear") before the call collects; this bounds the buffer so the
/// read loop neither blocks nor grows unboundedly.
///
/// RESERVED BARRIER SLOT (DL-65 item 2). The diagnostics `sync_channel` is
/// sized `DIAG_BUFFER + 1`, and publishes are accounted SEPARATELY (see
/// [`Client::diag_publishes`]) and capped at `DIAG_BUFFER`, so the extra slot is
/// ALWAYS free for the single in-band [`DiagMsg::Barrier`] marker. A publish
/// flood can therefore never occupy the marker's capacity: dropping the marker
/// on a full buffer used to drain the authoritative post-barrier publish and
/// raise a false [`LspError::DiagnosticsTimeout`] on a HEALTHY warm server.
/// [`read_loop`] stays strictly non-blocking (`try_send`).
///
/// DROP-NEWEST reality for PUBLISHES: once `DIAG_BUFFER` publishes are
/// outstanding the read loop drops the NEWEST arriving publish rather than
/// block; older buffered rounds linger and [`Client::diagnostics`] drains them
/// by ORDER — the barrier marker, NOT version tags, splits stale from
/// authoritative (see [`Client::drain_to_barrier`]). NOTE (B2): a drop-OLDEST
/// ring (retain the freshest publishes) is the smarter policy, deferred to the
/// B2 readiness/observability work.
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
    /// A request was issued after [`LspPool::shutdown`]; the pool is terminal
    /// and never silently respawns a server once closed.
    #[error("lsp: pool shut down")]
    PoolShutdown,
    /// A pooled server cell was invalidated (concurrently evicted/removed)
    /// while the pool is still LIVE — DISTINCT from [`LspError::PoolShutdown`]
    /// (which is reserved for a terminally-closed pool). [`LspPool::with_client`]
    /// retries this against a fresh reservation; it is an internal retry signal,
    /// never surfaced to callers on the happy path.
    #[error("lsp: pooled server cell invalidated; retrying")]
    CellInvalidated,
}

/// Classifies a transport-fatal error: the connection is gone, so a pooled
/// server can never serve again and its entry must be invalidated (next
/// acquire respawns). `Closed` = read loop hit EOF; `Io` = a write failed
/// (e.g. `BrokenPipe` to a killed child). `Timeout`/`Rpc`/`RenameDeadline`
/// are NOT fatal — the server is alive, just slow or refusing one request.
fn is_fatal_transport(e: &LspError) -> bool {
    matches!(e, LspError::Closed { .. } | LspError::Io(_))
}

/// Stable-within-process content fingerprint used to decide whether a
/// bage-generated `compile_commands.json` is still the file we wrote before
/// removing it on close — never clobber a database a caller replaced.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
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
    uri_str_to_path(uri.as_str())
}

/// String-level body of [`uri_to_path`], usable where only the raw URI text
/// is at hand (the initialize root URI).
fn uri_str_to_path(s: &str) -> String {
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
// Workspace priming + clangd compilation database (issue #23)
// ---------------------------------------------------------------------------

/// Cap on how many same-language sibling files workspace priming will
/// `didOpen` before a rename, bounding both the filesystem walk and the
/// number of notifications sent to the server.
const PRIME_FILE_CAP: usize = 200;

/// Environment variable that disables workspace priming when set to `"1"`.
pub const NO_PRIME_ENV: &str = "BAGE_LSP_NO_PRIME";

/// Basename of the clang compilation database consulted (and, when missing,
/// generated) for clangd.
const COMPILE_COMMANDS: &str = "compile_commands.json";

/// Reports whether the server command runs clangd. Detection is a substring
/// scan over every token so wrappers survive it: `clangd`, `clangd-18`,
/// `/usr/bin/clangd`, and `docker run … sh -c "… exec clangd"` all match.
fn command_is_clangd(command: &[String]) -> bool {
    command.iter().any(|t| t.contains("clangd"))
}

/// Walks `root` depth-first collecting the files `keep` accepts, capped at
/// `cap` results. Hidden (dot-prefixed) directories plus `target/` and
/// `node_modules/` are skipped, entries are visited in sorted order for
/// determinism, and unreadable directories are silently ignored.
fn walk_files(root: &Path, cap: usize, keep: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if p.is_dir() {
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                stack.push(p);
            } else if keep(&p) {
                out.push(p);
                if out.len() >= cap {
                    return out;
                }
            }
        }
    }
    out
}

/// Discovers source files under `root` whose extension maps to the given LSP
/// `languageId`, bounded by `cap` — the candidate set workspace priming
/// `didOpen`s before a rename.
fn discover_same_language_files(root: &Path, language_id: &str, cap: usize) -> Vec<PathBuf> {
    walk_files(root, cap, &|p| {
        language_id_for_path(&p.to_string_lossy()) == language_id
    })
}

/// Ensures `root` has a `compile_commands.json` for clangd. Without a
/// compilation database clangd treats every file as an isolated translation
/// unit and a rename never crosses TUs. When the database already exists (or
/// no C/C++ sources are found) nothing is written and `Ok(None)` is
/// returned; otherwise a minimal database covering every `.c`/`.cc`/`.cpp`/
/// `.cxx` file under `root` is written — one `{directory, file, command:
/// "clang -c <file>"}` entry each — and `Ok(Some((path, hash)))` is returned
/// (`hash` fingerprints the bytes written) so the creator
/// ([`Client::initialize`] via [`Client::close`]/Drop) removes the file it
/// added ONLY while it still matches — never clobbering a caller's
/// replacement. A pre-existing database is never touched or removed.
///
/// OWNERSHIP CAVEAT (honest note): the database is keyed by `root`, but the
/// pool keys servers by (root, language). Today only clangd (C/C++) generates
/// one, so per-root ownership is unambiguous. If a future mixed-language root
/// ran two clangd servers over the same directory, the first to close would
/// remove a database the second still relies on (only when it still matches
/// what bage wrote). Robust shared-root ownership (refcount / per-server temp
/// database) is DESIGN-ROUTED to B2 (mixed-language-root work), not solved here.
pub fn ensure_compile_commands(root: &Path) -> io::Result<Option<(PathBuf, u64)>> {
    let db = root.join(COMPILE_COMMANDS);
    if db.exists() {
        return Ok(None);
    }
    let tus = walk_files(root, usize::MAX, &|p| {
        matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("c" | "cc" | "cpp" | "cxx")
        )
    });
    if tus.is_empty() {
        return Ok(None);
    }
    let entries: Vec<Value> = tus
        .iter()
        .map(|f| {
            json!({
                "directory": root.to_string_lossy(),
                "file": f.to_string_lossy(),
                "command": format!("clang -c {}", f.to_string_lossy()),
            })
        })
        .collect();
    let body = serde_json::to_vec_pretty(&entries)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let hash = content_hash(&body);
    fs::write(&db, body)?;
    Ok(Some((db, hash)))
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

/// Method of the diagnostics ORDERING BARRIER request (DL-64 #1). A `$/`-prefixed
/// request the LSP spec lets a server answer with `MethodNotFound` — a
/// side-effect-free ping every server MUST answer IN ORDER after the didOpen (and
/// any warm-reuse didClose "clear") it just processed. Its response is the FIFO
/// point that splits stale/clear publishes (pre-barrier) from the authoritative
/// one (post-barrier), so `diagnostics` never counts clears or guesses versions.
const BARRIER_METHOD: &str = "$/bage/barrier";

/// A message routed from [`read_loop`] to [`Client::diagnostics`] over the
/// bounded diagnostics channel, in strict server FIFO order.
enum DiagMsg {
    /// A `textDocument/publishDiagnostics` params payload (uri + version +
    /// diagnostics).
    Publish(lt::PublishDiagnosticsParams),
    /// In-band ORDERING BARRIER marker (DL-64 #1): emitted the instant the read
    /// loop routes the response to the barrier request whose id equals the armed
    /// [`Client::barrier_arm`]. Forwarded on the SAME FIFO channel as publishes,
    /// so every publish the server emitted BEFORE answering the barrier is
    /// enqueued ahead of it and every one AFTER lands behind — the order-based
    /// split `diagnostics` uses to drain stale/clear rounds and return the
    /// authoritative publish. Carries the id so a marker from a prior
    /// (timed-out) barrier is ignored, never mistaken for this call's.
    Barrier(u64),
}

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
    /// Bounded FIFO of [`DiagMsg`] — `publishDiagnostics` params (uri + version +
    /// diagnostics) interleaved with the in-band barrier marker. Carrying the uri
    /// lets [`Client::diagnostics`] match the requested file and drain a warm
    /// server's stale publishes for others; the marker splits pre- from
    /// post-barrier rounds.
    diags: Receiver<DiagMsg>,
    /// Count of [`DiagMsg::Publish`] messages currently in `diags`, the budget
    /// that reserves the barrier slot (see [`DIAG_BUFFER`]). [`read_loop`] (the
    /// SOLE producer) increments only after a successful `try_send` and refuses
    /// to send a publish once this reaches `DIAG_BUFFER`; [`Client::diagnostics`]
    /// (via [`Client::drain_to_barrier`], the sole publish consumer) decrements
    /// on every publish it drains. Single-producer + subtract-only-consumer, and
    /// the channel itself orders the messages, so `Relaxed` suffices.
    diag_publishes: Arc<AtomicUsize>,
    /// Request id of the diagnostics ORDERING BARRIER currently awaited (0 =
    /// none). Set by [`Client::diagnostics`] before it issues the barrier ping;
    /// [`read_loop`] forwards a [`DiagMsg::Barrier`] onto `diags` when it routes
    /// the matching response. Shared with the read loop; ids are monotonic so a
    /// stale arm never matches a future response, and the read loop only ever
    /// `try_send`s the marker (never blocks — the DROP-on-overflow invariant).
    barrier_arm: Arc<AtomicU64>,
    /// Persistent reader-death flag, set true by [`read_loop`] the instant it
    /// exits (EOF or transport error). Observed at the top of `call` AND
    /// `diagnostics` so a request issued AFTER the read side died fails FATAL
    /// (`Closed`) immediately instead of writing into a corpse and blocking the
    /// full call/rename deadline — the half-dead escape (stdout EOF while the
    /// pending map is empty) that let `rename` burn 30s and return a non-fatal
    /// `Timeout`/`RenameDeadline`, leaving the pooled cell `Ready` forever.
    dead: Arc<AtomicBool>,
    next_id: u64,
    ver: i32,
    child: Option<Child>,
    /// The full server command (set by `new_stdio`, empty for `from_conn`),
    /// kept for clangd detection.
    command: Vec<String>,
    /// Workspace root recorded at initialize — the base directory workspace
    /// priming walks before a rename.
    root: Option<PathBuf>,
    /// URIs currently `didOpen` on the server. On warm reuse a re-open of an
    /// already-open doc `didClose`s first, so a pooled server never receives a
    /// spec-illegal duplicate `didOpen`.
    open_docs: HashSet<String>,
    /// A compile_commands.json bage generated for clangd (path + content
    /// fingerprint at creation), removed again on close/Drop ONLY if the file
    /// still matches — never clobber a caller's replacement. `None` when the
    /// database pre-existed or was never needed.
    created_compile_commands: Option<(PathBuf, u64)>,
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
        client.command = command.to_vec();
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
        // `+ 1` reserves a slot that the publish accounting (`diag_publishes`,
        // capped at DIAG_BUFFER) can never claim, so the barrier marker is never
        // dropped on a full buffer (DL-65 item 2).
        let (diag_tx, diag_rx) = mpsc::sync_channel(DIAG_BUFFER + 1);
        let diag_publishes = Arc::new(AtomicUsize::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let barrier_arm = Arc::new(AtomicU64::new(0));
        {
            let writer = Arc::clone(&writer);
            let pending = Arc::clone(&pending);
            let dead = Arc::clone(&dead);
            let barrier_arm = Arc::clone(&barrier_arm);
            let diag_publishes = Arc::clone(&diag_publishes);
            thread::spawn(move || {
                read_loop(
                    Box::new(reader),
                    writer,
                    pending,
                    diag_tx,
                    diag_publishes,
                    dead,
                    barrier_arm,
                )
            });
        }
        Client {
            writer,
            pending,
            diags: diag_rx,
            diag_publishes,
            barrier_arm,
            dead,
            next_id: 0,
            ver: 0,
            child: None,
            command: Vec::new(),
            root: None,
            open_docs: HashSet::new(),
            created_compile_commands: None,
            rename_deadline: DEFAULT_RENAME_DEADLINE,
            rename_retry: DEFAULT_RENAME_RETRY,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// Sends one request and blocks for its response (bounded by `timeout`).
    fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, LspError> {
        // Reader dead: the read loop already exited, so no response can ever
        // arrive. Writing + blocking would burn the full `timeout` and return a
        // non-fatal `Timeout` (the half-dead escape). Fail FATAL now so the
        // pool invalidates + respawns the corpse.
        if self.dead.load(Ordering::Acquire) {
            return Err(LspError::Closed {
                method: method.to_string(),
            });
        }
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);
        // TOCTOU re-check (DL-64 #2): the reader may have hit EOF — setting `dead`
        // and DRAINING the pending map — AFTER the top-of-fn check but BEFORE this
        // insert, leaving our freshly-inserted waiter unreachable by that drain.
        // A write + block would then burn the full `timeout` and return a
        // non-fatal `Timeout` (the half-dead escape). Re-checking the flag under
        // the just-inserted entry closes the window: `Release`/`Acquire` ordering
        // means observing `dead == false` here guarantees the reader's drain has
        // not run yet and WILL fail our waiter; observing `true` means we self-
        // remove and fail FATAL now, never writing into a corpse.
        if self.dead.load(Ordering::Acquire) {
            lock(&self.pending).remove(&id);
            return Err(LspError::Closed {
                method: method.to_string(),
            });
        }

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
    /// (a `file://` URI for the workspace root). The root path is recorded
    /// as the base for workspace priming, and when the server command runs
    /// clangd a missing compile_commands.json is generated at the root first
    /// (see [`ensure_compile_commands`]; a generation failure is swallowed —
    /// clangd then just stays single-TU, exactly as before).
    pub fn initialize(&mut self, root_uri: &str) -> Result<(), LspError> {
        let root = PathBuf::from(uri_str_to_path(root_uri));
        if command_is_clangd(&self.command)
            && let Ok(Some(created)) = ensure_compile_commands(&root)
        {
            self.created_compile_commands = Some(created);
        }
        self.root = Some(root);
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
    /// authoritative content. On a warm server that already holds this doc
    /// open (pooled reuse), a `textDocument/didClose` is sent first: the LSP
    /// spec forbids a duplicate `didOpen`, so re-open = close-then-open.
    fn did_open(&mut self, path: &str, content: &str) -> Result<(), LspError> {
        let uri = file_uri(path).to_string();
        if self.open_docs.contains(&uri) {
            self.did_close_uri(&uri)?;
        }
        self.ver += 1;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id_for_path(path),
                    "version": self.ver,
                    "text": content,
                },
            }),
        )?;
        self.open_docs.insert(uri);
        Ok(())
    }

    /// Sends `textDocument/didClose` for an already-open `uri` and forgets it,
    /// so the next `did_open` of the same doc is a fresh open (the warm-reuse
    /// duplicate-`didOpen` fix).
    fn did_close_uri(&mut self, uri: &str) -> Result<(), LspError> {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        self.open_docs.remove(uri);
        Ok(())
    }

    /// Opens the file at `path` (sending `content` via didOpen so the server
    /// has an authoritative view), primes the server with the target's
    /// same-language sibling files (see [`Client::prime_workspace`]),
    /// requests a `textDocument/rename` of the symbol at the zero-based
    /// (line, col) UTF-16 position, and returns the server's `WorkspaceEdit`
    /// — convert it to byte offsets with [`workspace_edit_to_file_edits`].
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
        self.prime_workspace(path);
        let params = json!({
            "textDocument": {"uri": file_uri(path)},
            "position": {"line": line, "character": col},
            "newName": new_name,
        });

        let deadline = Instant::now() + self.rename_deadline;
        let mut last: String;
        loop {
            match self.call("textDocument/rename", params.clone(), self.call_timeout) {
                // Fatal transport (connection gone / write failed): the server
                // is DEAD, not merely still-indexing. Break the retry loop
                // immediately with the TYPED error — never stringify a corpse
                // into `last` and burn the full 30s rename deadline retrying it
                // (MIN-1). `with_client` then invalidates + respawns.
                Err(e) if is_fatal_transport(&e) => return Err(e),
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

    /// Primes the server with the rename target's same-language sibling
    /// files under the workspace root recorded at initialize, `didOpen`ing
    /// each with its disk content. Servers that only consider OPEN documents
    /// (pyright) need this for a rename to reach cross-file references;
    /// full-workspace servers (gopls, rust-analyzer) simply ignore the
    /// redundant opens, so priming runs unconditionally — bounded by
    /// [`PRIME_FILE_CAP`], skipping hidden dirs, `target/` and
    /// `node_modules/` — unless [`NO_PRIME_ENV`]=1 disables it. Best-effort:
    /// unreadable files and notify failures are skipped (a dead transport
    /// still surfaces on the rename request itself).
    fn prime_workspace(&mut self, target: &str) {
        if std::env::var(NO_PRIME_ENV).is_ok_and(|v| v == "1") {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let lang = language_id_for_path(target);
        if lang == "plaintext" {
            return;
        }
        for p in discover_same_language_files(&root, lang, PRIME_FILE_CAP) {
            let ps = p.to_string_lossy().into_owned();
            if ps == target {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let _ = self.did_open(&ps, &text);
        }
    }

    /// Opens `path` in the server (didOpen with `content`) and collects the
    /// `textDocument/publishDiagnostics` notification FOR `path`, mapping each
    /// entry into Båge's reporting shape. The result arrives as a
    /// server→client NOTIFICATION (not a request response), gathered from the
    /// read loop via the bounded diagnostics queue. Blocks until the
    /// authoritative publish for `path` arrives or `timeout` elapses; an empty
    /// publish (a clean file) returns an empty vec.
    ///
    /// ORDERING BARRIER (DL-64 #1). After the (possibly warm) `did_open`, a
    /// synchronous [`BARRIER_METHOD`] round-trip is issued; [`read_loop`] marks
    /// its response in-band on the FIFO diagnostics channel. Every publish
    /// enqueued BEFORE that marker is stale by construction — a prior file's, a
    /// superseded round, or the version-less "clear" a warm re-open's `didClose`
    /// emits (possibly SEVERAL, e.g. an interleaved rename's `didClose` plus this
    /// call's) — and is drained. The FIRST same-uri publish AFTER the marker is
    /// this file's answer. Order-based only: never counting clears nor guessing
    /// versions (the lazy 1-clear counter this replaces missed a second
    /// outstanding clear — false-clean `[]` — and misdrained a version-less clean
    /// publish as a "clear" — false [`LspError::DiagnosticsTimeout`]).
    ///
    /// CAUSALITY LIMIT (accepted, ruled DL-65). The barrier proves ARRIVAL
    /// order, not CAUSALITY: a debounced server may flush a pre-barrier-caused
    /// publish AFTER answering the barrier, so a returned round can be
    /// stale-clean on a warm server (module docs). Best-effort by construction;
    /// the causal close is the pull-based [`textDocument/diagnostic`] request
    /// (LSP 3.17), routed to B2. The inline-publisher path (a server that
    /// publishes SYNCHRONOUSLY on `didOpen`, before answering the barrier) has no
    /// post-barrier round to return here and degrades to an HONEST
    /// [`LspError::DiagnosticsTimeout`] (PR-B shape) rather than a false-clean —
    /// the retry-safe, never-silently-wrong outcome.
    pub fn diagnostics(
        &mut self,
        path: &str,
        content: &str,
        timeout: Duration,
    ) -> Result<Vec<Diagnostic>, LspError> {
        // Reader dead: fail FATAL rather than block for `timeout` (parity with
        // `call`). The diagnostics sender is also dropped when the read loop
        // exits, but the explicit flag check makes the fatal outcome immediate
        // even before a `didOpen` write is attempted into the corpse.
        if self.dead.load(Ordering::Acquire) {
            return Err(LspError::Closed {
                method: "textDocument/publishDiagnostics".to_string(),
            });
        }
        let want = file_uri(path).as_str().to_string();
        self.did_open(path, content)?;
        // Arm + issue the barrier. The read loop stamps a `DiagMsg::Barrier` at
        // the exact FIFO point this response occupies among the publishes.
        self.next_id += 1;
        let barrier_id = self.next_id;
        self.barrier_arm.store(barrier_id, Ordering::Release);
        let req = json!({
            "jsonrpc": "2.0", "id": barrier_id, "method": BARRIER_METHOD, "params": Value::Null,
        });
        let write_res = write_frame(lock(&self.writer).as_mut(), &req);
        let out = match write_res {
            // Write failed: the transport is gone. FATAL (parity with `call`).
            Err(e) => Err(LspError::Io(e)),
            Ok(()) => self.drain_to_barrier(&want, path, barrier_id, timeout),
        };
        // Disarm unconditionally so a late barrier response cannot stamp a stale
        // marker against a FUTURE call's channel.
        self.barrier_arm.store(0, Ordering::Release);
        out
    }

    /// Drains the diagnostics FIFO for the barrier awaited under `barrier_id`:
    /// discards every message ahead of the [`DiagMsg::Barrier`] marker (stale
    /// rounds, other files, warm-reuse clears) and returns the FIRST `uri`-match
    /// publish AFTER it. A marker for a DIFFERENT (prior, timed-out) barrier is
    /// ignored. `Disconnected` = read loop hit EOF → FATAL `Closed` (so
    /// `with_client` invalidates + respawns the corpse, never a non-fatal
    /// timeout that leaves it pooled `Ready`); exhausting `timeout` = a live but
    /// quiet server → non-fatal, retry-safe `DiagnosticsTimeout`.
    fn drain_to_barrier(
        &self,
        want: &str,
        path: &str,
        barrier_id: u64,
        timeout: Duration,
    ) -> Result<Vec<Diagnostic>, LspError> {
        let deadline = Instant::now() + timeout;
        let mut past_barrier = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LspError::DiagnosticsTimeout {
                    path: path.to_string(),
                    after: timeout,
                });
            }
            match self.diags.recv_timeout(remaining) {
                Ok(DiagMsg::Barrier(id)) => {
                    // Our barrier reached: everything ahead was pre-barrier and
                    // already drained; the next uri-match publish is the answer.
                    if id == barrier_id {
                        past_barrier = true;
                    }
                    continue;
                }
                // Pre-barrier (stale/other-file/clear) OR a post-barrier publish
                // for a DIFFERENT file: drain and keep waiting. Only a post-
                // barrier same-uri publish is authoritative — including an empty
                // one (a genuinely clean file → `Ok([])`).
                Ok(DiagMsg::Publish(p)) => {
                    // Consumed a publish from the channel: free its slot in the
                    // accounting so the read loop can buffer another (and the
                    // reserved barrier slot stays reserved). See `diag_publishes`.
                    self.diag_publishes.fetch_sub(1, Ordering::Relaxed);
                    if !past_barrier || p.uri.as_str() != want {
                        continue;
                    }
                    return Ok(p.diagnostics.iter().map(to_diagnostic).collect());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(LspError::DiagnosticsTimeout {
                        path: path.to_string(),
                        after: timeout,
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(LspError::Closed {
                        method: "textDocument/publishDiagnostics".to_string(),
                    });
                }
            }
        }
    }

    /// Requests an orderly LSP shutdown (shutdown + exit) and reaps the
    /// subprocess, killing it if it does not exit promptly. Removes a
    /// compile_commands.json bage generated for clangd — but only while it
    /// still matches what bage wrote (a pre-existing OR caller-replaced
    /// database is never touched). Best-effort: a failed shutdown still
    /// proceeds to exit and reaping, and the first error encountered is
    /// returned.
    pub fn close(&mut self) -> Result<(), LspError> {
        remove_generated_compile_commands(self.created_compile_commands.take());
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

    /// OS process id of the spawned server, or `None` for a `from_conn`
    /// client (no owned subprocess). Lets the pool / an observability layer
    /// identify and verify reaping of the server child — the orphan-prevention
    /// invariant the pool leans on.
    pub fn server_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

impl Drop for Client {
    /// Backstop: kill and reap the server subprocess if `close` was skipped,
    /// and remove a compile_commands.json bage created (only if it still
    /// matches what bage wrote).
    fn drop(&mut self) {
        remove_generated_compile_commands(self.created_compile_commands.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Removes a bage-generated `compile_commands.json` ONLY while its bytes still
/// hash to what bage wrote — a caller may have replaced the database, and
/// clobbering foreign content is never acceptable. A read failure or hash
/// mismatch leaves the file untouched.
fn remove_generated_compile_commands(created: Option<(PathBuf, u64)>) {
    if let Some((db, hash)) = created
        && fs::read(&db)
            .map(|b| content_hash(&b) == hash)
            .unwrap_or(false)
    {
        let _ = fs::remove_file(db);
    }
}

// ---------------------------------------------------------------------------
// Persistent server pool (B1)
// ---------------------------------------------------------------------------

/// Default idle window before [`LspPool::evict_idle`] tears a warm server
/// down: long enough to keep a server hot across a burst of edits, short
/// enough not to strand a language server indefinitely.
pub const DEFAULT_POOL_IDLE_TTL: Duration = Duration::from_secs(300);

/// Default cap on concurrently-live servers — bounds spawned subprocesses so
/// a many-root session never fans out unboundedly; at cap the least-recently
/// -used server is evicted to make room.
pub const DEFAULT_POOL_MAX_SERVERS: usize = 8;

/// Whether a pooled server is usable yet. The readiness signal B2 (hover /
/// goto-def / find-refs) gates on: `Starting` = reserved and the initialize
/// handshake is in flight (spawned OUTSIDE the pool map lock so it is
/// observable), so NO request is legal; `Ready` = initialized, requests may be
/// issued (a server may still be warming its cross-file index past this point
/// — `rename` already retries that still-indexing window, and B2 will treat
/// `Ready` as the floor past which a request is worth making); `Shutdown` =
/// terminal, the server was evicted/closed and is never reused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Readiness {
    /// Reserved; initialize in flight (not yet acknowledged). No request legal.
    Starting,
    /// Initialize acknowledged; the server is serving requests.
    Ready,
    /// Evicted/closed; terminal. A cell in this state is never handed out.
    Shutdown,
}

/// Identity of a pooled server: one language server per (workspace root,
/// language). Distinct roots or languages get distinct servers; repeat
/// callers on the same pair reuse the one warm server.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct PoolKey {
    /// Workspace root the server was initialized at.
    root: PathBuf,
    /// LSP `languageId` (from [`language_id_for_path`]).
    language: String,
}

/// One pooled server plus its bookkeeping. `client` is `Mutex<Option<Client>>`
/// so concurrent requests to the same key SERIALIZE onto the single connection
/// (a [`Client`] is not safe for concurrent use) AND the lock doubles as the
/// once-only init guard: spawn+initialize run under it OUTSIDE the pool map
/// lock (`None` until the handshake completes), so a slow handshake blocks
/// neither other keys nor a `readiness` observer. `readiness` is the B2
/// gating signal; `last_used` drives idle + LRU eviction; `leases` COUNTS
/// in-flight requests so a busy server is never evicted out from under one.
struct PooledServer {
    /// The live connection, `None` between reservation and a completed
    /// handshake; the mutex is the single-server serialization + init guard.
    client: Mutex<Option<Client>>,
    /// B2-facing readiness signal (observable as `Starting` during init).
    readiness: Mutex<Readiness>,
    /// Last time a request ran against this server (idle/LRU input).
    last_used: Mutex<Instant>,
    /// Count of in-flight requests holding this server. A COUNTER, not a bool,
    /// and incremented UNDER the map lock at acquire time (MIN-4/DL-63 #4): a
    /// bool set post-acquire left a window where a just-returned `Ready` cell
    /// was still evictable, and — worse — a same-key handoff (T1 drop clearing
    /// T2's lease) stomped a live lease to `false`, un-pinning a busy server.
    /// A per-request increment/decrement pair keeps concurrent leases on one
    /// key independent; `> 0` == leased == never LRU/idle-evicted.
    leases: Mutex<u32>,
}

impl PooledServer {
    /// A fresh reservation: no client yet, `Starting`, unleased (0 in-flight).
    fn reserved() -> PooledServer {
        PooledServer {
            client: Mutex::new(None),
            readiness: Mutex::new(Readiness::Starting),
            last_used: Mutex::new(Instant::now()),
            leases: Mutex::new(0),
        }
    }
}

/// Whether a pooled cell may be evicted RIGHT NOW: NO in-flight lease (`leases
/// == 0` — no request would be stranded) AND fully `Ready` (a `Starting`
/// reservation is mid-handshake — evicting it strands the spawn and
/// double-spawns the key; a `Shutdown` cell is already terminal). Shared by
/// [`evict_lru`] and [`LspPool::evict_idle`] so the exemption set stays
/// identical (MIN-4/DL-63 #4).
fn evictable(s: &PooledServer) -> bool {
    *lock(&s.leases) == 0 && *lock(&s.readiness) == Readiness::Ready
}

/// RAII release for ONE lease previously taken under the map lock (in
/// [`LspPool::acquire`]). Drop decrements the lease count and stamps last-used,
/// so a panic unwinding out of a request closure cannot leak a lease and pin
/// the entry unevictable forever (MIN-4). The INCREMENT is deliberately NOT
/// here — it happens under the map lock at acquire time (DL-63 #4) to close the
/// acquire→guard window; this guard owns only the paired decrement.
struct LeaseGuard(Arc<PooledServer>);

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        *lock(&self.0.last_used) = Instant::now();
        let mut n = lock(&self.0.leases);
        // Fail LOUD in debug on a double-release (DL-64 #5): every increment
        // (under the map lock in `acquire`) is paired with exactly ONE decrement,
        // so reaching 0 here means a leak/double-release bug — assert rather than
        // silently `saturating_sub` past it. Release builds trust the invariant.
        debug_assert!(*n > 0, "lease double-release: decrement at zero");
        *n -= 1;
    }
}

/// A persistent language-server pool keyed by (root, language): it replaces
/// the old spawn-per-call model, keeping servers warm across requests while
/// guaranteeing every spawned child is reaped on drop/shutdown (no orphans),
/// bounding concurrently-live servers, and evicting idle ones. Requests run
/// through [`LspPool::with_client`] / [`LspPool::with_client_for_file`] under
/// the per-server lock. `Send`+`Sync` (the spawn factory is `Send`+`Sync`,
/// every field is a `Mutex`), so a pool is shareable across threads — the
/// substrate a future persistent MCP/GDD-IDE session drives.
pub struct LspPool {
    /// Spawns one fresh (un-initialized) server connection. The production
    /// factory is `Client::new_stdio(command)`; tests inject an in-memory
    /// transport. Boxed `Fn` so the pool stays a concrete type.
    spawn: Box<dyn Fn() -> Result<Client, LspError> + Send + Sync>,
    /// Idle window for [`LspPool::evict_idle`].
    idle_ttl: Duration,
    /// Hard cap on concurrently-live servers (>= 1). At cap a new key evicts LRU
    /// EVICTABLE (idle, `Ready`) servers in a LOOP until strictly under the cap
    /// BEFORE inserting (DL-63 #1) — so a pool that once overshot really shrinks
    /// back on the next new-key acquire (the prior evict-one-insert-one was
    /// net-zero and pinned the pool at the overshoot). When EVERY pooled server
    /// is leased/handshaking the loop finds no victim and the pool transiently
    /// overshoots the cap by this one reservation; that overshoot is NOT
    /// reclaimed the instant a lease frees — it shrinks at the NEXT eviction
    /// pass that finds a free victim (a subsequent new-key acquire once a lease
    /// freed, or [`LspPool::evict_idle`] after `idle_ttl`). See [`evict_lru`].
    max_servers: usize,
    /// The warm servers, keyed by identity.
    servers: Mutex<HashMap<PoolKey, Arc<PooledServer>>>,
    /// Set by [`LspPool::shutdown`]: terminal. Once closed, `acquire` returns
    /// [`LspError::PoolShutdown`] instead of silently respawning.
    closed: AtomicBool,
}

impl LspPool {
    /// Production pool: servers spawned via [`Client::new_stdio`] with
    /// `command`, default idle TTL and server cap.
    pub fn new(command: Vec<String>) -> LspPool {
        LspPool::with_config(command, DEFAULT_POOL_IDLE_TTL, DEFAULT_POOL_MAX_SERVERS)
    }

    /// Production pool with an explicit idle window and server cap.
    pub fn with_config(command: Vec<String>, idle_ttl: Duration, max_servers: usize) -> LspPool {
        let spawn = move || Client::new_stdio(&command);
        LspPool::from_spawn(Box::new(spawn), idle_ttl, max_servers)
    }

    /// Seam constructor over an arbitrary spawn factory (tests wire in-memory
    /// transports; production goes through [`LspPool::with_config`]). The cap
    /// is floored at 1 so the pool can always hold the server it just spawned.
    fn from_spawn(
        spawn: Box<dyn Fn() -> Result<Client, LspError> + Send + Sync>,
        idle_ttl: Duration,
        max_servers: usize,
    ) -> LspPool {
        LspPool {
            spawn,
            idle_ttl,
            max_servers: max_servers.max(1),
            servers: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Acquires (or spawns+initializes, once) the server for (`root`,
    /// `language`) and runs `f` against it under the per-server lock, so
    /// concurrent callers on the same key serialize onto ONE server. The
    /// spawn+initialize happens OUTSIDE the map lock (only the reservation is
    /// under it), so a key is spawned at most once under a stampede without
    /// blocking other keys.
    ///
    /// Self-healing: if `f` fails with a fatal transport error (the pooled
    /// connection died — e.g. the server was killed), the dead entry is
    /// invalidated and `f` is retried ONCE against a freshly respawned server.
    /// This restores the old spawn-per-call resilience — a dead server never
    /// poisons the key for the pool's lifetime — hence `f: Fn` (may run
    /// twice). `rename`/`diagnostics` are idempotent, so a retry is safe.
    pub fn with_client<T>(
        &self,
        root: &Path,
        language: &str,
        f: impl Fn(&mut Client) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        let key = PoolKey {
            root: root.to_path_buf(),
            language: language.to_string(),
        };
        // Bounded self-healing (DL-63 #6). Two transient faults each cost one
        // respawn+retry: a live-pool cell invalidation (a concurrent evict
        // marked this reservation `Shutdown` — [`LspError::CellInvalidated`],
        // an INTERNAL retry signal) and a fatal transport error (the pooled
        // connection died). `MAX_ATTEMPTS = 2` heals a single concurrent
        // invalidation/death, bounded so a pathological storm cannot loop.
        // `CellInvalidated` NEVER escapes on the happy path: it retries. On
        // exhaustion the surfaced error is HONEST (DL-64 #4): `PoolShutdown` ONLY
        // when the pool actually closed under us; on a still-LIVE pool the
        // UNDERLYING fatal transport error that drove the invalidation (remembered
        // in `last_fatal`), else the retry signal itself — never a lying
        // `PoolShutdown` on a live pool. A fatal transport error on the LAST
        // attempt surfaces AS itself.
        const MAX_ATTEMPTS: u32 = 2;
        let mut last_fatal: Option<LspError> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let (result, server) = match self.run_once(&key, &f) {
                Ok(rs) => rs,
                Err(LspError::CellInvalidated) => {
                    // A live-pool evict race. Terminal ONLY if the pool closed;
                    // otherwise retry against a fresh reservation.
                    if self.closed.load(Ordering::Acquire) {
                        return Err(LspError::PoolShutdown);
                    }
                    if attempt == MAX_ATTEMPTS {
                        // Retries spent on a LIVE pool: surface the underlying
                        // fatal error if the invalidation was death-driven, else
                        // the honest retry signal — never a false `PoolShutdown`.
                        return Err(last_fatal.take().unwrap_or(LspError::CellInvalidated));
                    }
                    continue;
                }
                // A typed spawn/handshake error (incl. terminal `PoolShutdown`)
                // is authoritative — never retried, never masked.
                Err(e) => return Err(e),
            };
            match result {
                Err(e) if is_fatal_transport(&e) => {
                    // Dead connection: invalidate so no later caller reuses it.
                    self.remove_cell(&key, &server);
                    if attempt == MAX_ATTEMPTS {
                        // Retries spent: surface the fatal transport error.
                        return Err(e);
                    }
                    // Remember it: a following-attempt `CellInvalidated` exhaustion
                    // then surfaces THIS real cause instead of a bare signal.
                    last_fatal = Some(e);
                    drop(server);
                    // else loop: respawn + retry once.
                }
                other => return other,
            }
        }
        // Every `attempt == MAX_ATTEMPTS` arm above returns; unreachable.
        unreachable!("with_client retry loop returns on the final attempt")
    }

    /// [`LspPool::with_client`] with the key derived from a file the way the
    /// old spawn-per-call paths did: root = the file's parent (or "."),
    /// language = [`language_id_for_path`]. The single migration entrypoint
    /// for `rename` and `diagnostics`.
    pub fn with_client_for_file<T>(
        &self,
        file: &Path,
        f: impl Fn(&mut Client) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        let root = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let language = language_id_for_path(&file.to_string_lossy()).to_string();
        self.with_client(&root, &language, f)
    }

    /// One acquire+run pass: acquires the (initialized) server — ALREADY leased
    /// (the lease was taken under the map lock inside [`LspPool::acquire`], so
    /// there is no evictable window between selecting the cell and pinning it) —
    /// runs `f`, stamps last-used, and releases the lease. Returns `f`'s result
    /// plus the server Arc so the caller can invalidate it on a fatal transport
    /// error.
    fn run_once<T>(
        &self,
        key: &PoolKey,
        f: &impl Fn(&mut Client) -> Result<T, LspError>,
    ) -> Result<(Result<T, LspError>, Arc<PooledServer>), LspError> {
        let server = self.acquire(key)?;
        // RAII release for the lease `acquire` already took under the map lock:
        // the guard decrements it (and stamps last-used) on drop — including an
        // UNWINDING PANIC out of `f`, which must never leak a lease and pin the
        // entry unevictable forever (MIN-4). `f`'s panic still propagates.
        let guard = LeaseGuard(Arc::clone(&server));
        let result = {
            let mut slot = lock(&server.client);
            let client = slot
                .as_mut()
                .expect("acquire guarantees an initialized client");
            f(client)
        };
        drop(guard);
        Ok((result, server))
    }

    /// The B2 readiness signal for a key, or `None` when no server is pooled
    /// for it yet.
    pub fn readiness(&self, root: &Path, language: &str) -> Option<Readiness> {
        let key = PoolKey {
            root: root.to_path_buf(),
            language: language.to_string(),
        };
        lock(&self.servers).get(&key).map(|s| *lock(&s.readiness))
    }

    /// Number of warm servers currently pooled.
    pub fn len(&self) -> usize {
        lock(&self.servers).len()
    }

    /// Whether the pool holds no warm servers.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Explicit maintenance entrypoint (NOT auto-scheduled — the pool runs no
    /// background reaper; a future B2/GDD session loop drives it, and `Drop`
    /// reaps everything regardless): closes and removes every server idle for
    /// at least `idle_ttl`, returning how many were evicted. In-flight (leased)
    /// servers are skipped — never torn down under an active request.
    pub fn evict_idle(&self) -> usize {
        let now = Instant::now();
        // Drain the stale entries UNDER the map lock, then release it before
        // closing: `close_server` blocks up to ~5s (shutdown RPC + child reap)
        // and holding the global map lock across it stalled every other key's
        // acquire (MIN-3).
        let drained: Vec<Arc<PooledServer>> = {
            let mut map = lock(&self.servers);
            let stale: Vec<PoolKey> = map
                .iter()
                .filter(|(_, s)| {
                    evictable(s) && now.duration_since(*lock(&s.last_used)) >= self.idle_ttl
                })
                .map(|(k, _)| k.clone())
                .collect();
            let mut out = Vec::with_capacity(stale.len());
            for k in &stale {
                if let Some(s) = map.remove(k) {
                    *lock(&s.readiness) = Readiness::Shutdown;
                    out.push(s);
                }
            }
            out
        };
        let n = drained.len();
        for s in drained {
            close_server(s);
        }
        n
    }

    /// Orderly teardown: mark the pool closed (terminal — `acquire` then
    /// returns [`LspError::PoolShutdown`] rather than silently respawning) and
    /// close+remove every pooled server. Called by `Drop`; also an explicit
    /// shutdown API.
    ///
    /// In-flight handshake (DL-63 #5): a reservation whose spawn+initialize is
    /// still running when shutdown drains the map keeps its own client `Arc`, so
    /// `close_server` here no-ops on it (not the last ref); [`ensure_ready`]
    /// then observes the `closed` flag it set, closes its freshly-spawned client
    /// itself, and surfaces `PoolShutdown` — never installing a live server into
    /// the now-terminal pool.
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        // Drain under the lock, close AFTER releasing it (MIN-3): a
        // ~5s-per-server teardown must not hold the global map lock and stall a
        // concurrent `readiness`/`acquire`.
        let drained: Vec<Arc<PooledServer>> = {
            let mut map = lock(&self.servers);
            map.drain()
                .map(|(_, s)| {
                    *lock(&s.readiness) = Readiness::Shutdown;
                    s
                })
                .collect()
        };
        for s in drained {
            close_server(s);
        }
    }

    /// Acquires the initialized server for `key`, reserving+spawning one when
    /// absent, and LEASES it (increment under the map lock) before returning so
    /// there is no evictable window between selecting the cell and pinning it
    /// (DL-63 #4). Phase 1 (map lock): find-or-reserve the cell (a `Starting`
    /// placeholder) and take the lease, evicting LRU idle servers to make room
    /// at cap. Phase 2 (per-cell lock, map lock RELEASED): spawn+initialize
    /// exactly once — so a slow handshake blocks neither other keys nor a
    /// `readiness` observer, and a concurrent stampede on one key still spawns a
    /// single server. A failed handshake releases the lease and removes the
    /// poisoned reservation so the cap is not consumed. The caller
    /// ([`LspPool::run_once`]) owns the paired lease RELEASE via [`LeaseGuard`].
    fn acquire(&self, key: &PoolKey) -> Result<Arc<PooledServer>, LspError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(LspError::PoolShutdown);
        }
        let (cell, evicted) = {
            let mut map = lock(&self.servers);
            // Re-check under the lock: shutdown may have raced in.
            if self.closed.load(Ordering::Acquire) {
                return Err(LspError::PoolShutdown);
            }
            let (cell, evicted) = if let Some(s) = map.get(key) {
                (Arc::clone(s), Vec::new())
            } else {
                // At cap, evict evictable servers in a LOOP until strictly under
                // the cap BEFORE inserting (DL-63 #1). The prior evict-ONE
                // -insert-one was net-zero: once the pool overshot to N it stayed
                // at N forever (each new-key acquire evicted 1 and added 1). When
                // EVERY server is leased there is no victim and the loop stops,
                // so a fully-busy pool still transiently overshoots by this one
                // reservation — reclaimed at the NEXT acquire once a lease frees.
                let mut evicted = Vec::new();
                while map.len() >= self.max_servers {
                    match evict_lru(&mut map) {
                        Some(victim) => evicted.push(victim),
                        None => break, // all leased/handshaking → transient overshoot
                    }
                }
                let s = Arc::new(PooledServer::reserved());
                map.insert(key.clone(), Arc::clone(&s));
                (s, evicted)
            };
            // Take the lease WHILE still holding the map lock: a concurrent
            // `evict_lru` in another acquire now sees `leases > 0` and skips it.
            *lock(&cell.leases) += 1;
            (cell, evicted)
        };
        // Close LRU victims AFTER releasing the map lock (MIN-3): a ~5s teardown
        // each must not block every other key spawning here.
        //
        // RECLAIM-LATENCY (DL-64 #6, honest note): the close is SERIAL, so a
        // loop-evict of several victims charges the sum of their teardowns to
        // THIS acquire — up to ~5s per UNRESPONSIVE victim (`close_server` waits
        // out the shutdown RPC + child reap). Acceptable for the MVP (eviction is
        // rare and off the warm-hit path); deferring the teardown to a background
        // reaper + surfacing reclaim latency is B2 observability work.
        for victim in evicted {
            close_server(victim);
        }
        if let Err(e) = self.ensure_ready(&cell, key) {
            // Release the lease we took, then invalidate the poisoned/terminal
            // reservation so the cap is not consumed by a dead cell.
            {
                let mut n = lock(&cell.leases);
                // Paired with the increment above under the map lock; fail LOUD
                // in debug on an unbalanced release (DL-64 #5).
                debug_assert!(*n > 0, "lease double-release: decrement at zero");
                *n -= 1;
            }
            self.remove_cell(key, &cell);
            return Err(e);
        }
        Ok(cell)
    }

    /// Spawns+initializes `cell`'s connection exactly once, under the per-cell
    /// client lock (map lock already released). A concurrent caller holding
    /// the same `Arc` blocks here until the first finishes, then observes
    /// `Ready` and reuses the connection. A `Shutdown` cell is refused.
    ///
    /// Post-handshake shutdown re-check (DL-63 #5): the handshake can be slow,
    /// and [`LspPool::shutdown`] may flip the pool terminal WHILE it runs. If it
    /// did, installing the freshly-spawned client would resurrect a live server
    /// into a closed pool AND overwrite readiness to `Ready` on a cell shutdown
    /// already drained from the map. So the pool's `closed` flag is re-checked
    /// under the cell lock before install: on a race, the new client is closed
    /// and `PoolShutdown` surfaced instead.
    fn ensure_ready(&self, cell: &Arc<PooledServer>, key: &PoolKey) -> Result<(), LspError> {
        let mut slot = lock(&cell.client);
        match *lock(&cell.readiness) {
            Readiness::Ready => return Ok(()),
            // A `Shutdown` cell here means a concurrent evict/remove invalidated
            // THIS reservation while the pool is live (NOT a pool shutdown):
            // signal a retryable cell-invalidation so `with_client` respawns,
            // never a spurious terminal `PoolShutdown` on a live pool (MIN-2).
            Readiness::Shutdown => return Err(LspError::CellInvalidated),
            Readiness::Starting => {}
        }
        let mut client = (self.spawn)()?;
        client.initialize(&file_uri(&key.root.to_string_lossy()).to_string())?;
        // Install UNDER the map lock, re-checking `closed` AND cell membership
        // together (DL-64 #3): the handshake is slow, and between the old bare
        // `closed`-load and the install a concurrent `shutdown` (drains the map,
        // sets `closed`) OR an evict/`remove_cell` (unmaps this very Arc) could
        // land — stranding a live client on a terminal-or-orphaned cell that a
        // request then runs against. Serializing the checks with the install on
        // the one lock `shutdown`/evict must also take closes the window:
        //   - pool closed  → close the fresh client, surface `PoolShutdown`;
        //   - cell unmapped (a concurrent evict replaced/removed it while live)
        //                   → orderly-close the fresh client, surface the
        //                     retryable `CellInvalidated` so `with_client`
        //                     respawns — never a spurious terminal `PoolShutdown`.
        {
            let map = lock(&self.servers);
            if self.closed.load(Ordering::Acquire) {
                drop(map);
                let _ = client.close();
                return Err(LspError::PoolShutdown);
            }
            match map.get(key) {
                Some(existing) if Arc::ptr_eq(existing, cell) => {}
                _ => {
                    drop(map);
                    let _ = client.close();
                    return Err(LspError::CellInvalidated);
                }
            }
            *slot = Some(client);
            *lock(&cell.readiness) = Readiness::Ready;
        }
        Ok(())
    }

    /// Invalidates `cell` for `key`: marks it `Shutdown` and removes it from
    /// the map, but ONLY when it is still the mapped Arc (a concurrent respawn
    /// may have replaced it — never evict the fresh one). Used on a fatal
    /// transport error and on a failed handshake; the next acquire respawns.
    fn remove_cell(&self, key: &PoolKey, cell: &Arc<PooledServer>) {
        let removed = {
            let mut map = lock(&self.servers);
            match map.get(key) {
                Some(existing) if Arc::ptr_eq(existing, cell) => {
                    *lock(&cell.readiness) = Readiness::Shutdown;
                    map.remove(key)
                }
                _ => None,
            }
        };
        if let Some(s) = removed {
            close_server(s);
        }
    }
}

impl Drop for LspPool {
    /// Reap every warm server so no language-server child outlives the pool.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Removes the least-recently-used EVICTABLE server from `map` (marking it
/// `Shutdown`) and returns the drained `Arc` for the caller to close OUTSIDE
/// the map lock (MIN-3) — never closes under the lock itself. Leased
/// (`leases > 0`) and non-`Ready` (`Starting` mid-handshake / `Shutdown`) cells
/// are exempt via [`evictable`]: evicting one strands its connection/handshake
/// and double-spawns the key.
///
/// Called in a LOOP by [`LspPool::acquire`] at cap (DL-63 #1), draining victims
/// until the map is under the cap. When every server is exempt there is no
/// victim (`None`) and the caller's new reservation transiently overshoots the
/// cap; that overshoot shrinks at a LATER eviction pass that finds a free
/// victim: a subsequent new-key acquire once a lease freed, or
/// [`LspPool::evict_idle`] once a server has been idle `idle_ttl`. A no-op
/// (`None`) on an empty map or an all-exempt map.
fn evict_lru(map: &mut HashMap<PoolKey, Arc<PooledServer>>) -> Option<Arc<PooledServer>> {
    let victim = map
        .iter()
        .filter(|(_, s)| evictable(s))
        .min_by_key(|(_, s)| *lock(&s.last_used))
        .map(|(k, _)| k.clone())?;
    let s = map.remove(&victim)?;
    *lock(&s.readiness) = Readiness::Shutdown;
    Some(s)
}

/// Closes the server behind `server` when this is its last reference (an
/// in-flight request holding another Arc reaps it via `Client`'s Drop
/// backstop on return). A never-initialized reservation (`None` client) has
/// nothing to close. Best-effort: a shutdown error is swallowed — the OS
/// child is still killed+reaped by [`Client::close`]/Drop, which is the
/// no-orphan guarantee that matters.
fn close_server(server: Arc<PooledServer>) {
    if let Some(inner) = Arc::into_inner(server)
        && let Some(mut client) = inner
            .client
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        let _ = client.close();
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
/// connection loss every pending call is failed with `Closed` AND the shared
/// `dead` flag is set so a LATER request (issued when the pending map is empty,
/// so nothing would disconnect its channel) fails fast in `call`/`diagnostics`
/// instead of blocking the full deadline against a corpse.
fn read_loop(
    reader: Box<dyn Read + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: PendingMap,
    diags: SyncSender<DiagMsg>,
    diag_publishes: Arc<AtomicUsize>,
    dead: Arc<AtomicBool>,
    barrier_arm: Arc<AtomicU64>,
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
                    // Forward the whole params (uri + version + diagnostics) so
                    // the consumer can match the requested file. Publishes are
                    // capped at DIAG_BUFFER (separate from the reserved barrier
                    // slot): at the cap, DROP the newest rather than block OR
                    // steal the marker's capacity. Increment only on a real send
                    // so the count mirrors the channel (see `diag_publishes`).
                    if diag_publishes.load(Ordering::Relaxed) < DIAG_BUFFER
                        && diags.try_send(DiagMsg::Publish(p)).is_ok()
                    {
                        diag_publishes.fetch_add(1, Ordering::Relaxed);
                    }
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

        // A response: route to the waiting call by id, and — if it answers the
        // armed diagnostics barrier — stamp an in-band `DiagMsg::Barrier` marker
        // at this exact FIFO point (DL-64 #1). `try_send` never blocks the read
        // loop, and the RESERVED slot (channel sized DIAG_BUFFER + 1, publishes
        // capped at DIAG_BUFFER) guarantees room for the single outstanding
        // marker even under a publish flood (DL-65 item 2) — no false
        // `DiagnosticsTimeout` on a healthy warm server. The `arm != 0` guard
        // avoids matching the never-issued request id 0.
        if let Some(id) = obj.get("id").and_then(Value::as_u64) {
            let arm = barrier_arm.load(Ordering::Acquire);
            if arm != 0 && arm == id {
                let _ = diags.try_send(DiagMsg::Barrier(id));
            }
            if let Some(tx) = lock(&pending).remove(&id) {
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
    }
    // Connection gone: mark the reader dead BEFORE failing waiters, so a
    // request racing in right now observes the flag rather than writing into a
    // corpse. `Release` pairs with the `Acquire` loads in `call`/`diagnostics`.
    dead.store(true, Ordering::Release);
    // Fail any callers still waiting.
    for (_, tx) in lock(&pending).drain() {
        let _ = tx.send(RpcOutcome::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::splice_edits;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

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

    /// A `publishDiagnostics` frame tagged with an explicit document `version`
    /// and a single diagnostic carrying `message` — lets version-ordering tests
    /// distinguish a stale prior-version publish from the current one.
    fn publish(uri: &str, version: i64, message: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "version": version,
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1},
                    },
                    "message": message,
                }],
            },
        })
    }

    /// A minimal fake LSP server: answers initialize/shutdown, delegates each
    /// textDocument/rename to `on_rename` (`Ok` → result, `Err` → JSON-RPC error,
    /// the not-yet-ready server shape), and — modeling a REAL server's async
    /// diagnostics round — publishes `diags_on_open` only AFTER answering the
    /// ordering-barrier ping ([`BARRIER_METHOD`]), so they land POST-barrier for
    /// [`Client::diagnostics`] (never a synchronous-on-didOpen shortcut real
    /// servers don't take).
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
                    m if m == BARRIER_METHOD => {
                        // Answer the barrier first (method-not-found is spec-legal
                        // for `$/`), THEN emit the diagnostics round so it is
                        // ordered POST-barrier on the FIFO channel.
                        reply_err(&mut w, &id, "method not found");
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

    #[test]
    fn rename_breaks_deadline_loop_on_fatal_transport() {
        // MIN-1: a server that DIES mid-rename must break the retry loop
        // IMMEDIATELY with the typed fatal transport error — never stringify it
        // into `last` and burn the full rename deadline retrying a corpse.
        // Pre-fix looped until the deadline and returned `RenameDeadline`.
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
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    // Die mid-rename: drop the write side and exit (no reply),
                    // so the client's outstanding rename call sees `Closed`.
                    "textDocument/rename" => {
                        drop(w);
                        break;
                    }
                    _ => {}
                }
            }
        });
        let mut c = test_client(client_conn); // rename_deadline = 2s
        c.initialize("file:///work").unwrap();
        let started = Instant::now();
        let err = c
            .rename("/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
            .unwrap_err();
        assert!(
            is_fatal_transport(&err),
            "fatal transport surfaced, never RenameDeadline: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "must return fast, never burn the rename deadline: {:?}",
            started.elapsed()
        );
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
        // Trigger the async flood: the barrier ping makes the fake emit its 20
        // rounds (post-barrier, as a real server would). Its reply is a spec-legal
        // method-not-found, ignored here.
        let _ = c.call(BARRIER_METHOD, Value::Null, Duration::from_secs(2));
        // Sync point: a second round-trip proves the read loop processed every
        // flood notification the server wrote before this reply.
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

    #[test]
    fn read_loop_reserves_barrier_slot_under_publish_flood() {
        // DL-65 item 2 DISCRIMINATOR: on a warm server a publish flood can fill
        // the buffer BEFORE the barrier response arrives. The marker must NEVER
        // be dropped while the pool is healthy (a dropped marker → the drain
        // never crosses the barrier → false `DiagnosticsTimeout`). Drives the
        // REAL `read_loop` synchronously (no concurrent drain → deterministic):
        // feed `DIAG_BUFFER + 4` pre-barrier publishes, then the armed barrier
        // response, to EOF; the marker must survive on the reserved slot.
        //
        // Pre-fix (channel sized DIAG_BUFFER, no reservation): the 8 publishes
        // fill the buffer and the marker `try_send` is dropped — this test finds
        // NO barrier and fails.
        let barrier_id: u64 = 7;
        let mut framed: Vec<u8> = Vec::new();
        for i in 0..(DIAG_BUFFER + 4) {
            let p = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": "file:///work/main.go",
                    "diagnostics": [{
                        "range": {
                            "start": {"line": i, "character": 0},
                            "end": {"line": i, "character": 1},
                        },
                        "message": format!("round {i}"),
                    }],
                },
            });
            write_frame(&mut framed, &p).unwrap();
        }
        // The barrier response lands AFTER the buffer is already full.
        write_frame(
            &mut framed,
            &json!({"jsonrpc": "2.0", "id": barrier_id, "result": Value::Null}),
        )
        .unwrap();

        let reader = std::io::Cursor::new(framed);
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(Vec::<u8>::new())));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (diag_tx, diag_rx) = mpsc::sync_channel(DIAG_BUFFER + 1);
        let diag_publishes = Arc::new(AtomicUsize::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let barrier_arm = Arc::new(AtomicU64::new(barrier_id));
        // Runs to EOF (Cursor) then returns; no consumer draining concurrently.
        read_loop(
            Box::new(reader),
            writer,
            pending,
            diag_tx,
            diag_publishes,
            dead,
            barrier_arm,
        );

        let mut publishes = 0;
        let mut markers = Vec::new();
        while let Ok(msg) = diag_rx.try_recv() {
            match msg {
                DiagMsg::Publish(_) => publishes += 1,
                DiagMsg::Barrier(id) => markers.push(id),
            }
        }
        assert_eq!(
            markers,
            vec![barrier_id],
            "the barrier marker must survive the flood on its reserved slot"
        );
        assert_eq!(
            publishes, DIAG_BUFFER,
            "publishes stay capped at {DIAG_BUFFER}, never stealing the reserved slot"
        );
    }

    #[test]
    fn diagnostics_matches_requested_file_draining_stale() {
        // MIN (uri mismatch): a warm server may have a prior file's
        // diagnostics still queued; `diagnostics(fileB)` must drain the stale
        // fileA publish and return fileB's, never another file's. Pre-fix
        // returned the first publish regardless of uri.
        //
        // The fake publishes TWO copies (tagged with the opened uri) POST-barrier
        // per open, so after reading fileA one stale A-publish remains queued
        // when fileB is requested.
        let (client_conn, server_conn) = conn_pair();
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            let mut last_uri = String::new();
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    "textDocument/didOpen" => {
                        last_uri = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    m if m == BARRIER_METHOD => {
                        reply_err(&mut w, &id, "method not found");
                        for _ in 0..2 {
                            write_frame(
                                &mut w,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "method": "textDocument/publishDiagnostics",
                                    "params": {
                                        "uri": last_uri,
                                        "diagnostics": [{
                                            "range": {
                                                "start": {"line": 0, "character": 0},
                                                "end": {"line": 0, "character": 1},
                                            },
                                            "message": last_uri,
                                        }],
                                    },
                                }),
                            )
                            .unwrap();
                        }
                    }
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });

        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        // fileA: consume one of its two publishes; one stale A-publish remains.
        let a = c
            .diagnostics("/work/a.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        let want_a = file_uri("/work/a.rs").as_str().to_string();
        assert!(a.iter().all(|d| d.message == want_a), "fileA diags: {a:?}");
        // fileB: the queue now holds [A(stale), B, B]; must skip A, return B.
        let b = c
            .diagnostics("/work/b.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        let want_b = file_uri("/work/b.rs").as_str().to_string();
        assert!(!b.is_empty(), "fileB must yield its own diagnostics");
        assert!(
            b.iter().all(|d| d.message == want_b),
            "must return fileB's diagnostics, not the stale fileA publish: {b:?}"
        );
        let _ = c.close();
    }

    #[test]
    fn diagnostics_drains_pre_barrier_stale_same_uri_publish() {
        // Order-based staleness (DL-64 #1, was MIN-6 version-aware): a SAME-uri
        // publish emitted BEFORE the barrier is stale by construction and drained,
        // regardless of version; only the FIRST same-uri publish AFTER the barrier
        // is returned. The fake emits a "STALE" round on didOpen (pre-barrier) and
        // the "CURRENT" round on the barrier (post-barrier) — no version guessing.
        let (client_conn, server_conn) = conn_pair();
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            let mut last_uri = String::new();
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    "textDocument/didOpen" => {
                        let uri = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        // Pre-barrier stale round.
                        write_frame(&mut w, &publish(&uri, 1, "STALE")).unwrap();
                        last_uri = uri;
                    }
                    m if m == BARRIER_METHOD => {
                        reply_err(&mut w, &id, "method not found");
                        // Post-barrier authoritative round.
                        write_frame(&mut w, &publish(&last_uri, 2, "CURRENT")).unwrap();
                    }
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });

        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        let got = c
            .diagnostics("/work/main.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        assert!(!got.is_empty(), "the post-barrier publish is returned");
        assert!(
            got.iter().all(|d| d.message == "CURRENT"),
            "must drain the pre-barrier stale publish, return only current: {got:?}"
        );
        let _ = c.close();
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

    // ---- workspace priming + compile_commands (issue #23) ----

    /// Serializes tests that read or write [`NO_PRIME_ENV`] so the process
    /// environment mutation cannot race a concurrent priming rename.
    static PRIME_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn discover_same_language_files_skips_hidden_and_vendor_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["pkg", ".hidden", "target", "node_modules"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in [
            "a.py",
            "pkg/b.py",
            ".hidden/c.py",
            "target/d.py",
            "node_modules/e.py",
        ] {
            std::fs::write(root.join(f), "x = 1\n").unwrap();
        }
        std::fs::write(root.join("notes.md"), "# not python\n").unwrap();

        let got = discover_same_language_files(root, "python", PRIME_FILE_CAP);
        let mut names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["a.py", "b.py"],
            "hidden/target/node_modules and other languages must be excluded"
        );
    }

    #[test]
    fn discover_same_language_files_caps_result_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.py")), "x = 1\n").unwrap();
        }
        let got = discover_same_language_files(dir.path(), "python", 3);
        assert_eq!(got.len(), 3, "discovery must stop at the cap");
    }

    #[test]
    fn ensure_compile_commands_generates_minimal_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("deep")).unwrap();
        for f in ["main.c", "util.cc", "deep/x.cpp"] {
            std::fs::write(root.join(f), "// tu\n").unwrap();
        }
        std::fs::write(root.join("skip.h"), "// header\n").unwrap();
        std::fs::write(root.join("skip.py"), "x = 1\n").unwrap();

        let (created, created_hash) = ensure_compile_commands(root)
            .unwrap()
            .expect("database must be created");
        assert_eq!(created, root.join(COMPILE_COMMANDS));

        let body = std::fs::read(&created).unwrap();
        assert_eq!(
            content_hash(&body),
            created_hash,
            "returned hash must fingerprint the written bytes"
        );
        let entries: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            entries.len(),
            3,
            "one entry per TU, headers/other langs skipped"
        );
        let mut files: Vec<String> = entries
            .iter()
            .map(|e| e["file"].as_str().unwrap().to_string())
            .collect();
        files.sort();
        let want: Vec<String> = ["deep/x.cpp", "main.c", "util.cc"]
            .iter()
            .map(|f| root.join(f).to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, want);
        for e in &entries {
            assert_eq!(e["directory"].as_str().unwrap(), root.to_string_lossy());
            let cmd = e["command"].as_str().unwrap();
            assert!(cmd.starts_with("clang -c "), "got command {cmd:?}");
        }

        // Second call: the database now exists, so nothing is created and
        // the content is untouched.
        assert!(ensure_compile_commands(root).unwrap().is_none());
        assert_eq!(std::fs::read(&created).unwrap(), body);
    }

    #[test]
    fn ensure_compile_commands_no_sources_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_compile_commands(dir.path()).unwrap().is_none());
        assert!(!dir.path().join(COMPILE_COMMANDS).exists());
    }

    #[test]
    fn close_removes_bage_created_compile_commands() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(COMPILE_COMMANDS);
        std::fs::write(&db, b"[]").unwrap();

        let (client_conn, server_conn) = conn_pair();
        spawn_fake_server(server_conn, || Ok(json!({})), Vec::new());
        let mut c = test_client(client_conn);
        c.created_compile_commands = Some((db.clone(), content_hash(b"[]")));
        let _ = c.close();
        assert!(!db.exists(), "close must remove the database bage created");
    }

    #[test]
    fn close_preserves_caller_replaced_compile_commands() {
        // A database whose content no longer matches what bage wrote (a caller
        // replaced it) must be LEFT UNTOUCHED — never clobber foreign content.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(COMPILE_COMMANDS);
        std::fs::write(&db, b"[]").unwrap();

        let (client_conn, server_conn) = conn_pair();
        spawn_fake_server(server_conn, || Ok(json!({})), Vec::new());
        let mut c = test_client(client_conn);
        // Fingerprint the ORIGINAL bytes bage "wrote", then the caller replaces
        // the file with different content.
        c.created_compile_commands = Some((db.clone(), content_hash(b"[]")));
        std::fs::write(&db, b"[{\"caller\":\"owned\"}]").unwrap();
        let _ = c.close();
        assert!(
            db.exists(),
            "a caller-replaced database must not be removed"
        );
        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"[{\"caller\":\"owned\"}]",
            "content must be left intact"
        );
    }

    /// A fake server that records every didOpen URI, for priming assertions.
    fn spawn_recording_server(
        server_conn: (PipeReader, PipeWriter),
        opened: Arc<Mutex<Vec<String>>>,
    ) {
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    "textDocument/didOpen" => {
                        if let Some(uri) = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                        {
                            opened.lock().unwrap().push(uri.to_string());
                        }
                    }
                    "textDocument/rename" => reply_ok(&mut w, &id, ready_rename_edit()),
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });
    }

    /// Shared fixture for the priming tests: a root with the rename target,
    /// one same-language sibling, and one other-language file. Returns the
    /// didOpen URIs recorded across one full rename.
    fn prime_fixture_opened_uris() -> (tempfile::TempDir, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("lib.py");
        std::fs::write(&target, "def greet():\n    return 1\n").unwrap();
        std::fs::write(root.join("other.py"), "from lib import greet\n").unwrap();
        std::fs::write(root.join("notes.md"), "# not python\n").unwrap();

        let (client_conn, server_conn) = conn_pair();
        let opened = Arc::new(Mutex::new(Vec::new()));
        spawn_recording_server(server_conn, Arc::clone(&opened));
        let mut c = test_client(client_conn);
        c.initialize(&file_uri(root.to_str().unwrap()).to_string())
            .unwrap();
        c.rename(
            target.to_str().unwrap(),
            "def greet():\n    return 1\n",
            0,
            4,
            "hello",
        )
        .expect("rename against recording fake");
        // The rename response is a sync point: every didOpen notification was
        // written to the pipe before the rename request, so the server has
        // recorded them all by the time it answers.
        let _ = c.close();
        let uris = opened.lock().unwrap().clone();
        (dir, uris)
    }

    #[test]
    fn rename_primes_same_language_siblings() {
        let _guard = PRIME_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (dir, opened) = prime_fixture_opened_uris();
        let root = dir.path();
        let target_uri = file_uri(root.join("lib.py").to_str().unwrap()).to_string();
        let sibling_uri = file_uri(root.join("other.py").to_str().unwrap()).to_string();
        let md_uri = file_uri(root.join("notes.md").to_str().unwrap()).to_string();

        assert_eq!(
            opened.first(),
            Some(&target_uri),
            "target must be opened first with authoritative content"
        );
        assert!(
            opened.contains(&sibling_uri),
            "same-language sibling must be primed, got {opened:?}"
        );
        assert!(
            !opened.contains(&md_uri),
            "other-language files must not be primed, got {opened:?}"
        );
        assert_eq!(
            opened.iter().filter(|u| **u == target_uri).count(),
            1,
            "target must not be re-opened by priming"
        );
    }

    #[test]
    fn rename_priming_disabled_by_env() {
        let _guard = PRIME_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: single-threaded with respect to this variable — every test
        // touching NO_PRIME_ENV holds PRIME_ENV_LOCK.
        unsafe { std::env::set_var(NO_PRIME_ENV, "1") };
        let (dir, opened) = prime_fixture_opened_uris();
        unsafe { std::env::remove_var(NO_PRIME_ENV) };
        let target_uri = file_uri(dir.path().join("lib.py").to_str().unwrap()).to_string();
        assert_eq!(
            opened,
            vec![target_uri],
            "with {NO_PRIME_ENV}=1 only the rename target may be opened"
        );
    }

    // ---- persistent server pool (B1) ----

    /// A pool whose spawn factory hands out in-memory fake servers (each a
    /// fresh `conn_pair` + `spawn_fake_server` answering initialize/rename),
    /// plus the spawn counter so tests can assert once-per-key spawning. Idle
    /// TTL is short so `evict_idle` is testable without long waits.
    fn fake_pool() -> (LspPool, Arc<AtomicUsize>) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            spawn_fake_server(server_conn, || Ok(ready_rename_edit()), Vec::new());
            Ok(test_client(client_conn))
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_millis(50), 8);
        (pool, spawns)
    }

    /// Drives one rename against the pool for `root`/`file` — the ready fake
    /// server always returns a non-empty edit, so this asserts success.
    fn pool_rename(pool: &LspPool, root: &str, file: &str) {
        pool.with_client(Path::new(root), "rust", |c| {
            c.rename(file, "fn main() {}\n", 0, 3, "renamed")
        })
        .expect("pool rename");
    }

    #[test]
    fn pool_spawns_once_per_key() {
        let (pool, spawns) = fake_pool();
        for _ in 0..3 {
            pool_rename(&pool, "/work", "/work/main.rs");
        }
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "same key reuses the one warm server"
        );
        assert_eq!(pool.len(), 1);
        // A distinct key spawns a second, independent server.
        pool_rename(&pool, "/other", "/other/main.rs");
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn pool_concurrent_requests_share_one_server() {
        let (pool, spawns) = fake_pool();
        let pool = Arc::new(pool);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                pool_rename(&p, "/work", "/work/main.rs");
            }));
        }
        for h in handles {
            h.join().expect("worker thread");
        }
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "concurrent same-key requests spawn exactly one server"
        );
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn pool_readiness_ready_after_acquire() {
        let (pool, _) = fake_pool();
        let root = Path::new("/work");
        assert_eq!(
            pool.readiness(root, "rust"),
            None,
            "no readiness before a server is pooled"
        );
        pool_rename(&pool, "/work", "/work/main.rs");
        assert_eq!(pool.readiness(root, "rust"), Some(Readiness::Ready));
    }

    #[test]
    fn pool_drop_reaps_server() {
        // Observation spy: the fake server flips `alive` false when its
        // connection tears down, proving the pool releases the server on drop.
        let alive = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&alive);
        let spawn = move || -> Result<Client, LspError> {
            let (client_conn, server_conn) = conn_pair();
            let (reader, mut w) = server_conn;
            let live = Arc::clone(&flag);
            live.store(true, Ordering::SeqCst);
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                while let Ok(Some(body)) = read_frame(&mut r) {
                    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                        "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                        "shutdown" => reply_ok(&mut w, &id, Value::Null),
                        "exit" => break,
                        _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                        _ => {}
                    }
                }
                live.store(false, Ordering::SeqCst); // connection gone = torn down
            });
            Ok(test_client(client_conn))
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_secs(60), 8);
        pool.with_client(Path::new("/work"), "rust", |_c| Ok(()))
            .expect("acquire server");
        assert!(alive.load(Ordering::SeqCst), "server up after acquire");
        drop(pool);
        for _ in 0..200 {
            if !alive.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !alive.load(Ordering::SeqCst),
            "server torn down when the pool drops"
        );
    }

    #[test]
    fn client_drop_reaps_subprocess() {
        // The OS-level no-orphan guarantee the pool leans on: a real spawned
        // child is killed+reaped on `Client` drop. `sleep` is a cheap
        // long-lived process; the pre-drop liveness assert also guards against
        // `kill` being unavailable (it would fail loudly, never false-pass).
        let client = match Client::new_stdio(&["sleep".to_string(), "30".to_string()]) {
            Ok(c) => c,
            Err(e) => {
                // Loud SKIP (never a silent false pass): `sleep` unavailable.
                eprintln!("SKIP client_drop_reaps_subprocess: `sleep` unavailable: {e}");
                return;
            }
        };
        let pid = client.server_pid().expect("spawned child has a pid");
        assert!(pid_alive(pid), "child running before drop");
        drop(client);
        let mut gone = false;
        for _ in 0..200 {
            if !pid_alive(pid) {
                gone = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(gone, "child {pid} reaped after Client drop");
    }

    #[test]
    fn pool_evict_idle_closes_stale_servers() {
        let (pool, spawns) = fake_pool(); // 50ms idle TTL
        pool_rename(&pool, "/work", "/work/main.rs");
        assert_eq!(pool.len(), 1);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(pool.evict_idle(), 1, "stale server evicted");
        assert_eq!(pool.len(), 0);
        // Re-acquire spawns a fresh server (the old one is gone).
        pool_rename(&pool, "/work", "/work/main.rs");
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn pool_bounded_evicts_lru_at_capacity() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            spawn_fake_server(server_conn, || Ok(ready_rename_edit()), Vec::new());
            Ok(test_client(client_conn))
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_secs(60), 2);
        pool_rename(&pool, "/a", "/a/main.rs");
        pool_rename(&pool, "/b", "/b/main.rs");
        assert_eq!(pool.len(), 2);
        thread::sleep(Duration::from_millis(2)); // make /a strictly newer than /b
        pool_rename(&pool, "/a", "/a/main.rs"); // /b is now the LRU
        pool_rename(&pool, "/c", "/c/main.rs"); // at cap → evicts LRU (/b)
        assert_eq!(pool.len(), 2, "server count stays bounded at the cap");
        assert!(
            pool.readiness(Path::new("/b"), "rust").is_none(),
            "LRU server /b was evicted"
        );
        assert!(pool.readiness(Path::new("/a"), "rust").is_some());
        assert!(pool.readiness(Path::new("/c"), "rust").is_some());
    }

    #[test]
    fn pool_respawns_after_server_dies_midlife() {
        // MAJOR (dead-server key poisoning): a pooled server that dies after
        // its first request must NOT poison the key. The next request through
        // the same key transparently respawns and succeeds, and readiness is
        // truthful (never stuck `Ready` on a corpse). Pre-fix: the second
        // request hit a broken pipe with no respawn and readiness stayed
        // `Ready` forever, so `pool_rename` would panic here.
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        // Each spawned fake answers exactly ONE rename, then exits — the
        // connection tears down, modeling a killed language server.
        let spawn = move || -> Result<Client, LspError> {
            counter.fetch_add(1, Ordering::SeqCst);
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
                        "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                        "textDocument/rename" => {
                            reply_ok(&mut w, &id, ready_rename_edit());
                            break; // die after the first rename
                        }
                        "shutdown" => reply_ok(&mut w, &id, Value::Null),
                        "exit" => break,
                        _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                        _ => {}
                    }
                }
            });
            Ok(test_client(client_conn))
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_secs(60), 8);
        pool_rename(&pool, "/work", "/work/main.rs"); // spawn #1, then it dies
        pool_rename(&pool, "/work", "/work/main.rs"); // must respawn+succeed
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            2,
            "dead server invalidated and respawned exactly once"
        );
        assert_eq!(
            pool.readiness(Path::new("/work"), "rust"),
            Some(Readiness::Ready),
            "readiness reflects the live respawned server"
        );
    }

    #[test]
    fn pool_readiness_starting_observable_during_init() {
        // Init runs OUTSIDE the map lock, so a concurrent observer sees
        // `Starting` while a server is handshaking. Pre-fix set `Ready` under
        // the map lock, making `Starting` structurally unobservable (a dead
        // variant) — `readiness` would block on the held map lock, then only
        // ever return `Ready`.
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&started);
        let rel = Arc::clone(&release);
        let spawn = move || -> Result<Client, LspError> {
            let (client_conn, server_conn) = conn_pair();
            let (reader, mut w) = server_conn;
            let s = Arc::clone(&s);
            let rel = Arc::clone(&rel);
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                while let Ok(Some(body)) = read_frame(&mut r) {
                    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                        "initialize" => {
                            // Announce mid-handshake, then stall until released.
                            s.store(true, Ordering::SeqCst);
                            while !rel.load(Ordering::SeqCst) {
                                thread::sleep(Duration::from_millis(1));
                            }
                            reply_ok(&mut w, &id, json!({"capabilities": {}}));
                        }
                        "shutdown" => reply_ok(&mut w, &id, Value::Null),
                        "exit" => break,
                        _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                        _ => {}
                    }
                }
            });
            Ok(test_client(client_conn))
        };
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_secs(60),
            8,
        ));
        let p = Arc::clone(&pool);
        let h = thread::spawn(move || {
            p.with_client(Path::new("/work"), "rust", |_c| Ok(()))
                .expect("acquire");
        });
        while !started.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            pool.readiness(Path::new("/work"), "rust"),
            Some(Readiness::Starting),
            "Starting must be observable while the handshake runs"
        );
        release.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert_eq!(
            pool.readiness(Path::new("/work"), "rust"),
            Some(Readiness::Ready),
            "handshake completion flips to Ready"
        );
    }

    #[test]
    fn pool_acquire_after_shutdown_is_typed_error() {
        // Shutdown is terminal: a post-shutdown acquire returns a typed error,
        // never a silent respawn. Pre-fix `shutdown` only drained the map, so
        // the next `with_client` respawned and returned Ok.
        let (pool, _) = fake_pool();
        pool_rename(&pool, "/work", "/work/main.rs");
        pool.shutdown();
        let err = pool
            .with_client(Path::new("/work"), "rust", |_c| Ok::<(), LspError>(()))
            .unwrap_err();
        assert!(matches!(err, LspError::PoolShutdown), "got {err}");
    }

    #[test]
    fn pool_never_evicts_in_flight_server() {
        // At cap, an at-capacity eviction must skip a leased (in-flight)
        // server — evicting a busy entry stranded its connection and let the
        // same key respawn a second live server. Pre-fix: `/a` (busy) was the
        // LRU victim and vanished from the map, so `readiness(/a)` went `None`.
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            spawn_fake_server(server_conn, || Ok(ready_rename_edit()), Vec::new());
            Ok(test_client(client_conn))
        };
        // Cap 1: acquiring a second key forces an eviction attempt.
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_secs(60),
            1,
        ));
        let running = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let p = Arc::clone(&pool);
        let run = Arc::clone(&running);
        let h = thread::spawn(move || {
            p.with_client(Path::new("/a"), "rust", |_c| {
                run.store(true, Ordering::SeqCst);
                let _ = release_rx.recv(); // hold the lease until released
                Ok::<(), LspError>(())
            })
            .expect("/a request");
        });
        while !running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        // /b at cap → eviction attempt must NOT evict the leased /a.
        pool.with_client(Path::new("/b"), "rust", |_c| Ok::<(), LspError>(()))
            .expect("/b request");
        assert!(
            pool.readiness(Path::new("/a"), "rust").is_some(),
            "an in-flight server must never be LRU-evicted"
        );
        release_tx.send(()).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn pool_respawns_on_dead_reader_diagnostics() {
        // M (class-escape): a pooled server whose READ side is dead (read loop
        // hit EOF → diagnostics sender dropped) must surface as FATAL `Closed`,
        // not a non-fatal `DiagnosticsTimeout`. `with_client` then invalidates
        // the corpse and respawns. Pre-fix `diagnostics` mapped the
        // disconnected channel to `DiagnosticsTimeout`, so the dead server
        // stayed pooled `Ready` forever and the request failed with no respawn.
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            let (reader, mut w) = server_conn;
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                // Ack initialize.
                if let Ok(Some(body)) = read_frame(&mut r)
                    && let Ok(msg) = serde_json::from_slice::<Value>(&body)
                {
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    reply_ok(&mut w, &id, json!({"capabilities": {}}));
                }
                if n == 0 {
                    // Poisoned server: drop the WRITE side so the client's read
                    // loop hits EOF (dead reader), then keep the READ side alive
                    // draining requests so client writes still succeed.
                    drop(w);
                    while let Ok(Some(_)) = read_frame(&mut r) {}
                } else {
                    // Healthy respawn: serve diagnostics POST-barrier (as a real
                    // server's async round would land).
                    let mut last_uri = String::new();
                    while let Ok(Some(body)) = read_frame(&mut r) {
                        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                            continue;
                        };
                        let id = msg.get("id").cloned().unwrap_or(Value::Null);
                        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                            "textDocument/didOpen" => {
                                last_uri = msg
                                    .pointer("/params/textDocument/uri")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                            }
                            m if m == BARRIER_METHOD => {
                                reply_err(&mut w, &id, "method not found");
                                write_frame(&mut w, &publish(&last_uri, 1, "live")).unwrap();
                            }
                            "shutdown" => reply_ok(&mut w, &id, Value::Null),
                            "exit" => break,
                            _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                            _ => {}
                        }
                    }
                }
            });
            Ok(test_client(client_conn))
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_secs(60), 8);
        let got = pool
            .with_client_for_file(Path::new("/work/main.rs"), |c| {
                c.diagnostics("/work/main.rs", "x\n", Duration::from_secs(2))
            })
            .expect("dead-reader server must be invalidated + respawned, then succeed");
        assert!(
            got.iter().all(|d| d.message == "live"),
            "served by the healthy respawn: {got:?}"
        );
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            2,
            "corpse invalidated + respawned exactly once"
        );
        assert_eq!(
            pool.readiness(Path::new("/work"), "rust"),
            Some(Readiness::Ready)
        );
    }

    #[test]
    fn pool_cell_invalidated_on_live_pool_retries_not_shutdown() {
        // MIN-2: a cell marked `Shutdown` by a concurrent evict/remove while the
        // pool is LIVE must be retried (respawned), never surfaced as the
        // terminal `PoolShutdown`. Pre-fix `ensure_ready` returned `PoolShutdown`
        // for any `Shutdown` cell, so a live-pool race spuriously failed the
        // acquire.
        let (pool, spawns) = fake_pool();
        let key = PoolKey {
            root: PathBuf::from("/work"),
            language: "rust".to_string(),
        };
        // Simulate the race outcome: a `Shutdown` reservation left mapped.
        {
            let cell = Arc::new(PooledServer::reserved());
            *lock(&cell.readiness) = Readiness::Shutdown;
            lock(&pool.servers).insert(key.clone(), cell);
        }
        pool.with_client(Path::new("/work"), "rust", |_c| Ok::<(), LspError>(()))
            .expect("live-pool cell invalidation must retry, not PoolShutdown");
        assert_eq!(
            pool.readiness(Path::new("/work"), "rust"),
            Some(Readiness::Ready)
        );
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "respawned exactly one fresh server past the invalidated cell"
        );
    }

    #[test]
    fn pool_lease_cleared_on_panic() {
        // MIN-4 (RAII lease): a panic inside the request closure must still
        // clear the lease, so the entry stays evictable. Pre-fix the lease
        // leaked `true` and pinned the server forever (evict_idle never
        // reclaimed it).
        let (pool, _) = fake_pool(); // 50ms idle TTL
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.with_client(Path::new("/work"), "rust", |_c| -> Result<(), LspError> {
                panic!("request closure blew up");
            })
        }));
        assert!(res.is_err(), "the panic propagates out of with_client");
        assert_eq!(pool.len(), 1, "the server is still pooled after the panic");
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            pool.evict_idle(),
            1,
            "a panic-cleared lease leaves the entry evictable"
        );
    }

    #[test]
    fn evict_lru_exempts_starting_cell() {
        // MIN-4 (Starting exempt): a cell mid-handshake (`Starting`, not yet
        // leased) must be exempt from LRU eviction — evicting it strands the
        // in-flight spawn. Even though the Starting cell is the OLDER (LRU)
        // entry, the only eligible victim is the newer `Ready` one.
        let mut map: HashMap<PoolKey, Arc<PooledServer>> = HashMap::new();
        let starting = Arc::new(PooledServer::reserved()); // Starting, last_used=now
        let ready = Arc::new(PooledServer::reserved());
        *lock(&ready.readiness) = Readiness::Ready;
        *lock(&ready.last_used) = Instant::now() + Duration::from_millis(50); // newer
        let s_key = PoolKey {
            root: PathBuf::from("/s"),
            language: "rust".to_string(),
        };
        let r_key = PoolKey {
            root: PathBuf::from("/r"),
            language: "rust".to_string(),
        };
        map.insert(s_key.clone(), Arc::clone(&starting));
        map.insert(r_key.clone(), Arc::clone(&ready));

        let victim = evict_lru(&mut map).expect("the Ready cell is evictable");
        assert_eq!(
            *lock(&victim.readiness),
            Readiness::Shutdown,
            "the drained victim is marked Shutdown"
        );
        assert!(
            map.contains_key(&s_key),
            "the Starting cell must survive eviction (not stranded mid-handshake)"
        );
        assert!(!map.contains_key(&r_key), "the Ready cell was the victim");
    }

    #[test]
    fn pool_cap_overshoots_when_all_leased_then_reclaims_on_evict() {
        // MIN-5 (cap honesty): with every server leased there is no eviction
        // victim, so a new key transiently overshoots the cap. A freed lease
        // does NOT itself reclaim — the overshoot shrinks only at a later
        // eviction pass (here `evict_idle`). Locks the documented behavior.
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            spawn_fake_server(server_conn, || Ok(ready_rename_edit()), Vec::new());
            Ok(test_client(client_conn))
        };
        // Cap 1, short idle TTL so evict_idle can reclaim.
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_millis(30),
            1,
        ));
        let running = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let p = Arc::clone(&pool);
        let run = Arc::clone(&running);
        let h = thread::spawn(move || {
            p.with_client(Path::new("/a"), "rust", |_c| {
                run.store(true, Ordering::SeqCst);
                let _ = release_rx.recv(); // hold the lease
                Ok::<(), LspError>(())
            })
            .expect("/a");
        });
        while !running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        // /b at cap while /a is leased: no victim → overshoot to 2.
        pool.with_client(Path::new("/b"), "rust", |_c| Ok::<(), LspError>(()))
            .expect("/b");
        assert_eq!(
            pool.len(),
            2,
            "all-leased eviction found no victim → cap overshoot"
        );
        // Free /a's lease — the overshoot is NOT reclaimed by that alone.
        release_tx.send(()).unwrap();
        h.join().unwrap();
        assert_eq!(
            pool.len(),
            2,
            "a freed lease does not itself reclaim the overshoot"
        );
        // A later eviction pass reclaims once servers are idle.
        thread::sleep(Duration::from_millis(50));
        assert!(pool.evict_idle() >= 1, "evict_idle reclaims the overshoot");
        assert!(
            pool.len() <= 1,
            "back within the cap after an eviction pass"
        );
    }

    #[test]
    fn pool_close_runs_outside_map_lock() {
        // MIN-3: `close_server` (up to ~5s: shutdown RPC + child reap) must run
        // OUTSIDE the global map lock. While one server is torn down, a
        // concurrent observer (`len`/`readiness`, both map-locked) must NOT
        // block. Pre-fix closed under the map lock, stalling every observer for
        // the full teardown.
        let spawn = move || -> Result<Client, LspError> {
            let (client_conn, server_conn) = conn_pair();
            let (reader, mut w) = server_conn;
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                while let Ok(Some(body)) = read_frame(&mut r) {
                    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    // Only answer initialize; never answer shutdown, so
                    // `Client::close` blocks on its 2s shutdown-call timeout,
                    // modeling a slow teardown.
                    if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                        reply_ok(&mut w, &id, json!({"capabilities": {}}));
                    }
                }
            });
            Ok(test_client(client_conn))
        };
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_secs(60),
            8,
        ));
        pool.with_client(Path::new("/a"), "rust", |_c| Ok::<(), LspError>(()))
            .expect("/a");

        let p = Arc::clone(&pool);
        let closing = thread::spawn(move || p.shutdown()); // blocks ~2s in close
        thread::sleep(Duration::from_millis(50)); // let shutdown reach the close phase
        let probe = Instant::now();
        let _ = pool.len(); // must not block on the map lock held across close
        assert!(
            probe.elapsed() < Duration::from_millis(500),
            "an observer must not block on the map lock during teardown: {:?}",
            probe.elapsed()
        );
        closing.join().unwrap();
    }

    #[test]
    fn warm_reuse_closes_before_reopen() {
        // MIN (duplicate didOpen): the second rename of the SAME file through
        // one warm pooled server must `didClose` the target before re-opening
        // it (LSP forbids a duplicate didOpen). Pre-fix sent a second didOpen
        // with no intervening didClose.
        let log: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let l = Arc::clone(&log);
        let spawn = move || -> Result<Client, LspError> {
            let (client_conn, server_conn) = conn_pair();
            spawn_open_close_recorder(server_conn, Arc::clone(&l));
            Ok(test_client(client_conn))
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_secs(60), 8);
        // Two renames of the same file → same warm server (/work, rust).
        for nn in ["a", "b"] {
            pool.with_client(Path::new("/work"), "rust", |c| {
                c.rename("/work/main.rs", "fn main() {}\n", 0, 3, nn)
            })
            .expect("warm rename");
        }
        let events = log.lock().unwrap().clone();
        let target = file_uri("/work/main.rs").to_string();
        let opens: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, (m, u))| m == "didOpen" && *u == target)
            .map(|(i, _)| i)
            .collect();
        let closes: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, (m, u))| m == "didClose" && *u == target)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(opens.len(), 2, "two renames = two didOpens: {events:?}");
        assert!(
            closes.iter().any(|&c| c > opens[0] && c < opens[1]),
            "a didClose must sit between the two didOpens: {events:?}"
        );
    }

    #[test]
    fn warm_real_server_double_rename() {
        // Env-gated real-server tier (rust-analyzer): render two renames
        // through ONE pooled server, proving the warm-reuse didOpen/didClose
        // handshake keeps a real server usable. Loud SKIP when rust-analyzer
        // is absent or the tier is not opted in — never a false pass.
        if std::env::var("BAGE_LSP_REAL_TEST").ok().as_deref() != Some("1") {
            eprintln!("SKIP warm_real_server_double_rename: set BAGE_LSP_REAL_TEST=1 to run");
            return;
        }
        if !command_on_path("rust-analyzer") {
            eprintln!("SKIP warm_real_server_double_rename: rust-analyzer not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.0.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let main_rs = root.join("src/main.rs");
        std::fs::write(
            &main_rs,
            "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
        )
        .unwrap();

        let pool = LspPool::new(vec!["rust-analyzer".to_string()]);
        let root_key = root.join("src");
        let content = std::fs::read_to_string(&main_rs).unwrap();
        // Two renames of `helper` through one warm server (same root+lang).
        for new_name in ["renamed_one", "renamed_two"] {
            let we = pool
                .with_client(&root_key, "rust", |c| {
                    c.rename(main_rs.to_str().unwrap(), &content, 0, 3, new_name)
                })
                .expect("warm real rename");
            assert!(
                workspace_edit_has_changes(&we),
                "rust-analyzer must resolve the rename"
            );
        }
    }

    /// Records `("didOpen"|"didClose", uri)` for every such notification, plus
    /// answers initialize/rename/shutdown — the observation spy for the
    /// warm-reuse didClose-before-reopen assertion.
    fn spawn_open_close_recorder(
        server_conn: (PipeReader, PipeWriter),
        log: Arc<Mutex<Vec<(String, String)>>>,
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
                    "textDocument/didOpen" | "textDocument/didClose" => {
                        if let Some(uri) = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                        {
                            let m = method.trim_start_matches("textDocument/").to_string();
                            log.lock().unwrap().push((m, uri.to_string()));
                        }
                    }
                    "textDocument/rename" => reply_ok(&mut w, &id, ready_rename_edit()),
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });
    }

    /// Whether `program` resolves on `PATH` (real-server test gating).
    fn command_on_path(program: &str) -> bool {
        std::process::Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn pool_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LspPool>();
    }

    /// Reports whether `pid` is a live process, via `kill -0` (POSIX; no extra
    /// dependency). A missing `kill` yields `false`, which the caller's
    /// pre-drop assert converts into a loud failure rather than a false pass.
    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // ---- DL-63 round-4 rulings ----

    #[test]
    fn rename_fatal_fast_on_dead_reader() {
        // DL-63 #2: a HALF-DEAD server (stdout EOF while the pending map is
        // empty) must make the next rename fail FATAL (`Closed`) immediately via
        // the persistent reader-dead flag — never write into the corpse and burn
        // the rename deadline returning a non-fatal `RenameDeadline`.
        let (client_conn, server_conn) = conn_pair();
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            // Ack initialize, then DROP the write side (client read loop hits EOF
            // = half-dead) while still draining reads so client writes succeed.
            if let Ok(Some(body)) = read_frame(&mut r)
                && let Ok(msg) = serde_json::from_slice::<Value>(&body)
            {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                reply_ok(&mut w, &id, json!({"capabilities": {}}));
            }
            drop(w);
            while let Ok(Some(_)) = read_frame(&mut r) {}
        });
        let mut c = test_client(client_conn);
        c.call_timeout = Duration::from_millis(50);
        c.initialize("file:///work").unwrap();
        // Deterministic: wait until the read loop observed EOF and set `dead`.
        for _ in 0..2000 {
            if c.dead.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            c.dead.load(Ordering::Acquire),
            "reader-dead flag set on EOF"
        );
        let started = Instant::now();
        let err = c
            .rename("/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
            .unwrap_err();
        assert!(
            is_fatal_transport(&err),
            "half-dead rename surfaces FATAL, never RenameDeadline: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "must fail fast via the dead flag, never burn the deadline: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn pool_respawns_on_dead_reader_rename() {
        // DL-63 #2 (pool leg): a warm server whose READ side died (half-dead)
        // must make the next rename FATAL so `with_client` invalidates + respawns
        // — never a non-fatal RenameDeadline leaving the corpse pooled `Ready`.
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            let (reader, mut w) = server_conn;
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                if let Ok(Some(body)) = read_frame(&mut r)
                    && let Ok(msg) = serde_json::from_slice::<Value>(&body)
                {
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    reply_ok(&mut w, &id, json!({"capabilities": {}}));
                }
                if n == 0 {
                    drop(w); // half-dead: read loop hits EOF
                    while let Ok(Some(_)) = read_frame(&mut r) {}
                } else {
                    while let Ok(Some(body)) = read_frame(&mut r) {
                        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                            continue;
                        };
                        let id = msg.get("id").cloned().unwrap_or(Value::Null);
                        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                            "textDocument/rename" => reply_ok(&mut w, &id, ready_rename_edit()),
                            "shutdown" => reply_ok(&mut w, &id, Value::Null),
                            "exit" => break,
                            _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                            _ => {}
                        }
                    }
                }
            });
            let mut c = test_client(client_conn);
            c.call_timeout = Duration::from_millis(50); // bound the racy first call
            Ok(c)
        };
        let pool = LspPool::from_spawn(Box::new(spawn), Duration::from_secs(60), 8);
        let we = pool
            .with_client_for_file(Path::new("/work/main.rs"), |c| {
                c.rename("/work/main.rs", "fn main() {}\n", 0, 3, "renamed")
            })
            .expect("dead-reader rename invalidated + respawned, then succeeds");
        assert!(workspace_edit_has_changes(&we));
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            2,
            "corpse invalidated + respawned exactly once"
        );
    }

    /// Emits a version-LESS empty "clear" publish for `uri` — the spec-shaped
    /// notification a server sends on `didClose`, ordered ahead of a warm
    /// re-open's real round on the FIFO channel.
    fn clear_publish(w: &mut PipeWriter, uri: &str) {
        write_frame(
            w,
            &json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": [] },
            }),
        )
        .unwrap();
    }

    #[test]
    fn diagnostics_probe_s2_rename_interleaved_warm_false_clean() {
        // PROBE S2 (DL-64 #1) — the shape the botched lazy 1-clear counter left
        // ALIVE: diagnostics → rename → diagnostics. Both the rename's didClose
        // AND the second diagnostics' didClose emit a version-less "clear", so TWO
        // clears are outstanding ahead of the re-open's real "boom" round. The
        // barrier drains ALL pre-barrier publishes (both clears) by ORDER and
        // returns the post-barrier "boom". Pre-fix drained exactly ONE clear and
        // returned the second as a false-clean `[]` — this test would FAIL then.
        let (client_conn, server_conn) = conn_pair();
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            let mut last_uri = String::new();
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    "textDocument/didClose" => {
                        if let Some(uri) = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                        {
                            clear_publish(&mut w, uri);
                        }
                    }
                    "textDocument/didOpen" => {
                        last_uri = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    "textDocument/rename" => reply_ok(&mut w, &id, ready_rename_edit()),
                    m if m == BARRIER_METHOD => {
                        reply_err(&mut w, &id, "method not found");
                        // Authoritative round lands POST-barrier.
                        write_frame(&mut w, &publish(&last_uri, 1, "boom")).unwrap();
                    }
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });
        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        // Cold: no prior didClose → no clear → post-barrier "boom".
        let cold = c
            .diagnostics("/work/main.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            cold.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(),
            vec!["boom"],
            "cold open returns the real error"
        );
        // Interleaved rename re-opens (its didClose emits clear #1).
        let _ = c.rename("/work/main.rs", "x\n", 0, 0, "renamed").unwrap();
        // Warm re-open: its didClose emits clear #2. TWO clears now precede
        // "boom". Order-based drain must return "boom", never a false-clean [].
        let warm = c
            .diagnostics("/work/main.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            warm.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(),
            vec!["boom"],
            "warm re-open past a rename must drain BOTH clears and return boom, not a false-clean []"
        );
        let _ = c.close();
    }

    #[test]
    fn diagnostics_probe_s1_no_clear_versionless_clean() {
        // PROBE S1 (DL-64 #1) — the shape the botched counter REGRESSED: a server
        // that sends NO didClose clear and publishes a genuinely clean file as a
        // version-LESS empty round. That clean publish arrives POST-barrier and
        // must return `Ok([])`. Pre-fix mis-drained the version-less empty publish
        // as if it were a "clear" on the warm path → false `DiagnosticsTimeout`.
        let (client_conn, server_conn) = conn_pair();
        let (reader, mut w) = server_conn;
        thread::spawn(move || {
            let mut r = BufReader::new(reader);
            let mut last_uri = String::new();
            while let Ok(Some(body)) = read_frame(&mut r) {
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => reply_ok(&mut w, &id, json!({"capabilities": {}})),
                    // No didClose clear: this server simply never emits one.
                    "textDocument/didOpen" => {
                        last_uri = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    m if m == BARRIER_METHOD => {
                        reply_err(&mut w, &id, "method not found");
                        // Genuinely clean file: version-LESS empty round, post-barrier.
                        clear_publish(&mut w, &last_uri);
                    }
                    "shutdown" => reply_ok(&mut w, &id, Value::Null),
                    "exit" => break,
                    _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                    _ => {}
                }
            }
        });
        let mut c = test_client(client_conn);
        c.initialize("file:///work").unwrap();
        // Cold: the version-less clean publish is post-barrier → Ok([]).
        let cold = c
            .diagnostics("/work/main.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        assert!(cold.is_empty(), "cold clean file returns Ok([]): {cold:?}");
        // Warm re-open: still Ok([]) — the version-less empty publish is the
        // authoritative post-barrier answer, never mis-drained into a timeout.
        let warm = c
            .diagnostics("/work/main.rs", "x\n", Duration::from_secs(2))
            .unwrap();
        assert!(
            warm.is_empty(),
            "warm clean file returns Ok([]), never a false DiagnosticsTimeout: {warm:?}"
        );
        let _ = c.close();
    }

    #[test]
    fn pool_reclaims_overshoot_on_next_new_key_acquire() {
        // DL-63 #1 (MIN-5 real eviction): after an all-leased overshoot (cap 1 →
        // len 2), the NEXT new-key acquire must LOOP-evict back under the cap.
        // The prior evict-one-insert-one was net-zero and pinned the pool at 2.
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let spawn = move || -> Result<Client, LspError> {
            counter.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            spawn_fake_server(server_conn, || Ok(ready_rename_edit()), Vec::new());
            Ok(test_client(client_conn))
        };
        // Long idle TTL so evict_idle is NOT what reclaims (isolate the
        // new-key-acquire reclaim path).
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_secs(60),
            1,
        ));
        let running = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let p = Arc::clone(&pool);
        let run = Arc::clone(&running);
        let h = thread::spawn(move || {
            p.with_client(Path::new("/a"), "rust", |_c| {
                run.store(true, Ordering::SeqCst);
                let _ = release_rx.recv(); // hold the lease
                Ok::<(), LspError>(())
            })
            .expect("/a");
        });
        while !running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        // /b at cap while /a leased → no victim → overshoot to 2.
        pool.with_client(Path::new("/b"), "rust", |_c| Ok::<(), LspError>(()))
            .expect("/b");
        assert_eq!(pool.len(), 2, "all-leased overshoot");
        // Free /a's lease; both /a and /b now idle+Ready+unleased.
        release_tx.send(()).unwrap();
        h.join().unwrap();
        assert_eq!(pool.len(), 2, "a freed lease alone does not reclaim");
        // A new-key acquire at cap must loop-evict BOTH stale servers → len 1.
        pool.with_client(Path::new("/c"), "rust", |_c| Ok::<(), LspError>(()))
            .expect("/c");
        assert_eq!(
            pool.len(),
            1,
            "new-key acquire loop-evicts the overshoot back under the cap"
        );
        assert!(pool.readiness(Path::new("/c"), "rust").is_some());
    }

    #[test]
    fn lease_counter_survives_same_key_handoff() {
        // DL-63 #4: the lease is a COUNTER, so one request's release cannot
        // un-pin another's in-flight lease on the SAME cell (the bool-stomp: T1
        // drop clearing T2's lease and letting a busy server be evicted).
        let (pool, _) = fake_pool();
        pool_rename(&pool, "/a", "/a/main.rs"); // establish the /a cell
        let key = PoolKey {
            root: PathBuf::from("/a"),
            language: "rust".to_string(),
        };
        let cell = lock(&pool.servers).get(&key).cloned().unwrap();
        // Two overlapping leases (a same-key handoff): T1 and T2.
        *lock(&cell.leases) += 1;
        *lock(&cell.leases) += 1;
        assert!(!evictable(&cell), "two leases pin the cell");
        // T1 releases via a guard drop — must NOT un-pin T2.
        drop(LeaseGuard(Arc::clone(&cell)));
        assert_eq!(
            *lock(&cell.leases),
            1,
            "T1's release leaves T2's lease intact (counter, not a stomped bool)"
        );
        assert!(
            !evictable(&cell),
            "still leased by T2 — never un-pinned by T1"
        );
        {
            let mut map = lock(&pool.servers);
            assert!(
                evict_lru(&mut map).is_none(),
                "a still-leased cell is never an LRU victim"
            );
        }
        // T2 releases — now fully idle and evictable.
        drop(LeaseGuard(Arc::clone(&cell)));
        assert_eq!(*lock(&cell.leases), 0);
        assert!(evictable(&cell), "both released → evictable");
    }

    #[test]
    fn pool_shutdown_during_init_surfaces_poolshutdown() {
        // DL-63 #5: shutdown racing in DURING a slow handshake must not resurrect
        // a live server into a terminal pool. ensure_ready re-checks `closed`
        // after init: it closes the freshly-spawned client and surfaces
        // PoolShutdown, never installing it `Ready`.
        let alive = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let av = Arc::clone(&alive);
        let st = Arc::clone(&started);
        let rl = Arc::clone(&release);
        let spawn = move || -> Result<Client, LspError> {
            let (client_conn, server_conn) = conn_pair();
            let (reader, mut w) = server_conn;
            let av = Arc::clone(&av);
            let st = Arc::clone(&st);
            let rl = Arc::clone(&rl);
            av.store(true, Ordering::SeqCst);
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                while let Ok(Some(body)) = read_frame(&mut r) {
                    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                        "initialize" => {
                            st.store(true, Ordering::SeqCst); // announce mid-handshake
                            while !rl.load(Ordering::SeqCst) {
                                thread::sleep(Duration::from_millis(1));
                            }
                            reply_ok(&mut w, &id, json!({"capabilities": {}}));
                        }
                        "shutdown" => reply_ok(&mut w, &id, Value::Null),
                        "exit" => break,
                        _ if !id.is_null() => reply_err(&mut w, &id, "method not found"),
                        _ => {}
                    }
                }
                av.store(false, Ordering::SeqCst); // torn down (client closed it)
            });
            Ok(test_client(client_conn))
        };
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_secs(60),
            8,
        ));
        let p = Arc::clone(&pool);
        let h = thread::spawn(move || {
            p.with_client(Path::new("/work"), "rust", |_c| Ok::<(), LspError>(()))
        });
        while !started.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        pool.shutdown(); // terminal WHILE init is stalled
        release.store(true, Ordering::SeqCst); // let init complete
        let res = h.join().unwrap();
        assert!(
            matches!(res, Err(LspError::PoolShutdown)),
            "install into a terminal pool must surface PoolShutdown, got {res:?}"
        );
        for _ in 0..400 {
            if !alive.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !alive.load(Ordering::SeqCst),
            "the mid-handshake client is closed, never resurrected into the terminal pool"
        );
        assert!(
            pool.readiness(Path::new("/work"), "rust").is_none(),
            "no Ready cell survives in the terminal pool"
        );
    }

    #[test]
    fn pool_cell_invalidated_on_live_pool_never_lies_poolshutdown() {
        // DL-64 #4: a poisoner keeps a `Shutdown` cell mapped so every acquire
        // sees an invalidated reservation; the pool is NEVER closed. On retry
        // exhaustion the honest outcome is the retry signal itself
        // (`CellInvalidated`) — there is no underlying fatal transport error to
        // surface here — and NEVER `PoolShutdown`, which would falsely claim a
        // LIVE pool is terminal (the lie the old code codified). `Ok` when a
        // window let the install land.
        let (pool, _) = fake_pool();
        let pool = Arc::new(pool);
        let key = PoolKey {
            root: PathBuf::from("/work"),
            language: "rust".to_string(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let p = Arc::clone(&pool);
        let k = key.clone();
        let sp = Arc::clone(&stop);
        let poisoner = thread::spawn(move || {
            while !sp.load(Ordering::SeqCst) {
                {
                    let mut map = lock(&p.servers);
                    let needs = match map.get(&k) {
                        Some(c) => *lock(&c.readiness) != Readiness::Shutdown,
                        None => true,
                    };
                    if needs {
                        let cell = Arc::new(PooledServer::reserved());
                        *lock(&cell.readiness) = Readiness::Shutdown;
                        map.insert(k.clone(), cell);
                    }
                }
                thread::yield_now();
            }
        });
        for _ in 0..200 {
            match pool.with_client(Path::new("/work"), "rust", |_c| Ok::<(), LspError>(())) {
                Ok(()) | Err(LspError::CellInvalidated) => {}
                Err(LspError::PoolShutdown) => {
                    panic!("live pool must never surface PoolShutdown (the lie DL-64 #4 fixes)")
                }
                Err(e) => panic!("unexpected error surfaced: {e}"),
            }
        }
        stop.store(true, Ordering::SeqCst);
        poisoner.join().unwrap();
    }

    #[test]
    fn call_after_reader_eof_never_burns_timeout() {
        // DL-64 #2 (dead-flag TOCTOU): if the read loop sets `dead` and DRAINS the
        // pending map between call's top-of-fn check and its pending-insert, the
        // freshly-inserted waiter is unreachable by that drain. Against a HALF-dead
        // transport (read side EOF, write side still accepting) the write then
        // succeeds and a bare `recv_timeout` would burn the FULL call timeout and
        // return a non-fatal `Timeout`. The post-insert re-check must fail FATAL
        // fast. Stressed to hit the narrow window.
        for _ in 0..300 {
            let (client_conn, server_conn) = conn_pair();
            let (reader, w) = server_conn;
            // Half-dead server: keep the READ side draining (client writes still
            // succeed) but drop the WRITE side so the client read loop hits EOF.
            let sr = thread::spawn(move || {
                let mut r = BufReader::new(reader);
                drop(w);
                while let Ok(Some(_)) = read_frame(&mut r) {}
            });
            let mut c = test_client(client_conn); // call_timeout = 2s
            // Bias toward the TOCTOU ordering: let the read loop process EOF
            // (set dead + drain) before the insert lands.
            thread::yield_now();
            let start = Instant::now();
            let err = c
                .call("ping", Value::Null, Duration::from_secs(2))
                .unwrap_err();
            assert!(
                matches!(err, LspError::Closed { .. } | LspError::Io(_)),
                "half-dead transport must fail FATAL, got {err}"
            );
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "must fail fast, never burn the call timeout: {:?}",
                start.elapsed()
            );
            drop(c); // closes the write side → server read loop EOFs → thread exits
            sr.join().unwrap();
        }
    }

    #[test]
    fn ensure_ready_rechecks_membership_before_install() {
        // DL-64 #3: a concurrent evict/remove that UNMAPS this reservation during
        // a slow handshake must make ensure_ready surface the retryable
        // `CellInvalidated` and orderly-close the fresh client — never install a
        // live client into a cell absent from the map (a stranded/orphaned server
        // a request would then run against). `with_client` heals via a respawn;
        // the second (uninterfered) handshake installs and serves. Pre-fix (bare
        // closed-check, no membership check) installed the orphan → ONE spawn and
        // no Ready cell left mapped.
        let spawns = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let sc = Arc::clone(&spawns);
        let st = Arc::clone(&started);
        let rl = Arc::clone(&release);
        let spawn = move || -> Result<Client, LspError> {
            let n = sc.fetch_add(1, Ordering::SeqCst);
            let (client_conn, server_conn) = conn_pair();
            spawn_fake_server(server_conn, || Ok(ready_rename_edit()), Vec::new());
            if n == 0 {
                // First handshake: announce, then stall so the test can unmap the
                // reservation before the install.
                st.store(true, Ordering::SeqCst);
                while !rl.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(test_client(client_conn))
        };
        let pool = Arc::new(LspPool::from_spawn(
            Box::new(spawn),
            Duration::from_secs(60),
            8,
        ));
        let key = PoolKey {
            root: PathBuf::from("/work"),
            language: "rust".to_string(),
        };
        let p = Arc::clone(&pool);
        let h = thread::spawn(move || {
            p.with_client(Path::new("/work"), "rust", |_c| Ok::<(), LspError>(()))
        });
        while !started.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        // Unmap the reservation mid-handshake (a concurrent evict/remove on a
        // still-LIVE pool).
        {
            let mut map = lock(&pool.servers);
            map.remove(&key);
        }
        release.store(true, Ordering::SeqCst);
        let res = h.join().unwrap();
        assert!(res.is_ok(), "with_client heals via respawn: {res:?}");
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            2,
            "the unmapped first handshake must NOT install; a respawn serves"
        );
        assert_eq!(
            pool.readiness(Path::new("/work"), "rust"),
            Some(Readiness::Ready),
            "the healed respawn is installed Ready and mapped"
        );
    }
}
