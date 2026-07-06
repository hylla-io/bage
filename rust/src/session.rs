//! Båge's FILE-LEG two-phase, region-anchored, concurrency-safe edit
//! protocol (SPEC §8, ADR-0003) plus the file-lifecycle ops (ADR-0004):
//! create, delete, move, and the all-or-nothing heterogeneous batch.
//!
//! `prepare` is OPTIMISTIC: it holds no lock, reads each live file, resolves
//! every region-anchored edit against those bytes (rejecting a
//! conflict/ambiguous resolve with a typed error), preview-splices, formats,
//! lints, and reparses to prove the result is valid, then durably records a
//! [`wal::Intent`]. Nothing is written to a source file during `prepare` —
//! its sole on-disk effect is the WAL record.
//!
//! `commit` is the ATOMIC, lossless point. Per file, UNDER A PER-FILE LOCK,
//! it RE-READS the live bytes and RE-RESOLVES every edit (resolve-under-lock,
//! so a concurrent commit that benignly shifted a region is picked up and the
//! edit lands at the current offset, never the stale one), splices,
//! atomic-writes, and computes an [`EditResult`]. A region whose region_hash
//! no longer matches any live node is a conflict and that file is not
//! written. Same-file commits serialize on one lock; cross-file commits take
//! different locks and run in parallel.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;

use crate::atomicwrite::{self, AtomicWriteError};
use crate::edit::{self, EditError, FileEdit};
use crate::format::{Formatter, Linter, ToolError};
use crate::hashing::{Hasher, norm_hash, raw_hash};
use crate::parser::{Lang, ParseError, ParserPort};
use crate::region::{self, EditResult, FileAnchor, LineIndex};
use crate::wal::{self, Intent, Move, WalError};

/// A stable, machine-readable classification of a session error, used by
/// hosts (CLI exit codes, Hylla) to react to a failure without inspecting
/// error chains or message text. Serializes to the same lowercase strings as
/// the Go implementation: `conflict|drift|exists|not-found|usage|io`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A region-anchored edit could not be resolved against the live file
    /// (concurrent change or ambiguous twins).
    Conflict,
    /// A raw_hash drift reject: the live bytes no longer match the expected
    /// anchor the caller saw.
    Drift,
    /// A create rejected because the target path already exists.
    Exists,
    /// An op rejected because the target path does not exist.
    NotFound,
    /// A caller/usage error (bad arguments or invalid request).
    Usage,
    /// The default I/O or otherwise unclassified failure.
    Io,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::Conflict => "conflict",
            Kind::Drift => "drift",
            Kind::Exists => "exists",
            Kind::NotFound => "not-found",
            Kind::Usage => "usage",
            Kind::Io => "io",
        })
    }
}

/// The single session error type, replacing the Go side's sentinel-error +
/// `errors.Is/As` chain: every failure mode is one enum variant, and
/// [`SessionError::kind`] replaces Go's `KindOf` classification.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A region-anchored edit could not be resolved against the live file:
    /// the target's region_hash no longer matches any live node, or matches
    /// more than one. Either case is a hard reject — Båge never guesses and
    /// never misapplies (SPEC §8.3, §8.4). The file is left untouched.
    #[error("session: edit conflict for {path:?}: {reason}")]
    Conflict {
        /// The file whose region could not be resolved.
        path: String,
        /// The resolve status that triggered the conflict ("conflict" or
        /// "ambiguous").
        reason: String,
    },
    /// A raw_hash drift reject: the live file no longer hashes to the value
    /// the caller anchored against, so a destructive op (delete/move) never
    /// discards or relocates bytes the caller did not see.
    #[error("session: edit conflict for {path:?}: raw_hash drift")]
    Drift {
        /// The file whose live bytes drifted from the expected anchor.
        path: String,
    },
    /// A create (or move destination) rejected because the target already
    /// exists: the non-existence anchor is absolute, Båge never clobbers.
    #[error("session: {path:?}: target already exists")]
    Exists {
        /// The path that already exists.
        path: String,
    },
    /// An op rejected because the target does not exist: nothing to
    /// delete/move/edit, distinct from a drift reject.
    #[error("session: {path:?}: target does not exist")]
    NotFound {
        /// The missing path.
        path: String,
    },
    /// A caller/usage error: bad arguments or an invalid request (empty
    /// batch, duplicate batch paths, missing move destination).
    #[error("session: usage: {0}")]
    Usage(String),
    /// The configured formatter failed; the staged content is rejected.
    #[error("session: format {path:?}: {source}")]
    Format {
        /// The file whose staged bytes were being formatted.
        path: String,
        /// The formatter failure.
        source: ToolError,
    },
    /// The configured linter rejected the staged content.
    #[error("session: lint {path:?}: {source}")]
    Lint {
        /// The file whose staged bytes were being linted.
        path: String,
        /// The lint failure.
        source: ToolError,
    },
    /// The staged bytes failed to parse AT ALL (the parse floor). A tree
    /// that parses WITH error/missing nodes is accepted — the floor is
    /// lenient; only a total parser failure rejects.
    #[error("session: reparse {path:?}: {source}")]
    Parse {
        /// The file whose staged bytes failed to parse.
        path: String,
        /// The parser failure.
        source: ParseError,
    },
    /// A byte-range splice failed (overlapping or out-of-bounds edits).
    #[error("session: splice {path:?}: {source}")]
    Splice {
        /// The file being spliced.
        path: String,
        /// The splice failure.
        source: EditError,
    },
    /// A write-ahead-log operation failed.
    #[error("session: WAL: {0}")]
    Wal(#[from] WalError),
    /// An atomic file write failed.
    #[error("session: write {path:?}: {source}")]
    Write {
        /// The file being written.
        path: String,
        /// The atomic-write failure.
        source: AtomicWriteError,
    },
    /// A recovery replayed a single-op move intent whose source bytes are
    /// missing from `originals` — the WAL record is unusable.
    #[error("session: recover move {from:?}->{to:?}: missing original bytes")]
    RecoverMissingOriginal {
        /// The move source.
        from: String,
        /// The move destination.
        to: String,
    },
    /// Any other I/O failure, classified [`Kind::Io`] unless the underlying
    /// error is not-found (mirroring Go's `os.ErrNotExist` → `KindNotFound`).
    #[error("session: {op} {path:?}: {source}")]
    Io {
        /// The operation that failed (e.g. "read", "mkdir").
        op: &'static str,
        /// The path the operation targeted.
        path: String,
        /// The underlying I/O error.
        source: io::Error,
    },
}

impl SessionError {
    /// Classifies the error into its stable [`Kind`] (Go's `KindOf`).
    pub fn kind(&self) -> Kind {
        match self {
            SessionError::Conflict { .. } => Kind::Conflict,
            SessionError::Drift { .. } => Kind::Drift,
            SessionError::Exists { .. } => Kind::Exists,
            SessionError::NotFound { .. } => Kind::NotFound,
            SessionError::Usage(_) => Kind::Usage,
            SessionError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                Kind::NotFound
            }
            _ => Kind::Io,
        }
    }

    /// The file the failure concerns, when the variant carries one.
    pub fn path(&self) -> Option<&str> {
        match self {
            SessionError::Conflict { path, .. }
            | SessionError::Drift { path }
            | SessionError::Exists { path }
            | SessionError::NotFound { path } => Some(path),
            _ => None,
        }
    }
}

/// The machine- and human-renderable projection of a session error: its
/// stable [`Kind`], the offending path (when known, omitted from JSON when
/// absent), and the error message. JSON shape matches the Go implementation:
/// `{"kind":"conflict","path":"...","message":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorEnvelope {
    /// The stable classification.
    pub kind: Kind,
    /// The file the failure concerns; omitted from JSON when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The underlying error's text.
    pub message: String,
}

impl ErrorEnvelope {
    /// Writes the envelope as a single `bage: <kind>: <message>` line.
    pub fn render_text(&self, w: &mut impl io::Write) -> io::Result<()> {
        writeln!(w, "bage: {}: {}", self.kind, self.message)
    }
}

/// Projects `err` into an [`ErrorEnvelope`] (Go's `Envelope`).
pub fn envelope(err: &SessionError) -> ErrorEnvelope {
    ErrorEnvelope {
        kind: err.kind(),
        path: err.path().map(str::to_string),
        message: err.to_string(),
    }
}

impl crate::render::TextRender for ErrorEnvelope {
    fn render_text(&self, w: &mut dyn io::Write) -> io::Result<()> {
        writeln!(w, "bage: {}: {}", self.kind, self.message)
    }
}

impl crate::render::TextRender for DeleteResult {
    fn render_text(&self, w: &mut dyn io::Write) -> io::Result<()> {
        writeln!(w, "deleted {} raw={}", self.path, self.raw_hash)
    }
}

impl crate::render::TextRender for MoveResult {
    fn render_text(&self, w: &mut dyn io::Write) -> io::Result<()> {
        writeln!(
            w,
            "moved {} -> {} raw={}",
            self.from, self.dest.path, self.dest.new_file_raw_hash
        )
    }
}

/// A tagged file-lifecycle operation (ADR-0004). Rust's sum type replaces
/// the Go side's `OpKind` tag + nil-able payload fields: each variant
/// carries exactly the data its op needs, so an edit op without an edit
/// payload is unrepresentable.
#[derive(Debug, Clone)]
pub enum Op {
    /// A region-anchored edit: replace the bytes of a content-anchored
    /// region (gated by its region_hash) with new text.
    Edit(region::Edit),
    /// Create a new file from non-existence: the target must not already
    /// exist (no clobber). `lang` optionally forces the parse-floor
    /// language; `None` auto-detects from the path.
    Create {
        /// The file to create.
        path: String,
        /// The full content of the new file.
        content: String,
        /// Optional parse-floor language override.
        lang: Option<Lang>,
    },
    /// Unlink an existing file, gated by the expected raw_hash drift anchor.
    Delete {
        /// The file to delete.
        path: String,
        /// The raw_hash the live bytes must still match.
        expected_raw_hash: String,
    },
    /// Relocate a file: anchored-delete(from) + anchored-create(to) as one
    /// atomic-on-recovery unit, preserving the bytes unchanged.
    Move {
        /// The source path (gated by `expected_raw_hash`).
        from: String,
        /// The destination path (gated by non-existence).
        to: String,
        /// The raw_hash the live source bytes must still match.
        expected_raw_hash: String,
    },
}

/// The success signal a host reads after a delete to close that file's node
/// versions: the deleted path and the confirmed raw_hash the live bytes
/// matched at unlink time. JSON is snake_case, matching Go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteResult {
    /// The file that was deleted.
    pub path: String,
    /// The raw content hash the live bytes matched (the satisfied drift
    /// anchor), confirming WHICH content was removed.
    pub raw_hash: String,
}

/// The signal a host reads after a move so it can re-identify the moved
/// file's nodes: the removed source path plus the destination's whole-file
/// [`EditResult`] over the relocated bytes. JSON is snake_case, matching Go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveResult {
    /// The source path removed by the move.
    pub from: String,
    /// The whole-file result for the destination (the relocated bytes),
    /// shaped like a create result.
    pub dest: EditResult,
}

/// The per-op outcome of a successful [`Session::apply_batch`], in input
/// order. Rust's sum type replaces Go's `BatchResult` struct whose fields
/// were populated "exactly one, selected by Kind".
#[derive(Debug, Clone, PartialEq)]
pub enum BatchResult {
    /// The per-edit results for an edit op.
    Edit(Vec<EditResult>),
    /// The whole-file result for a created file.
    Create(EditResult),
    /// The removed-path signal for a delete.
    Delete(DeleteResult),
    /// The relocation signal for a move.
    Move(MoveResult),
}

/// The result of a successful [`Session::prepare`]: the region edits and the
/// per-file anchors so `commit` can re-validate under lock — prepare is
/// optimistic, commit is the atomic point (SPEC §8).
#[derive(Debug, Clone)]
pub struct Plan {
    /// The WAL intent recorded by prepare (already persisted): originals for
    /// restore-on-failure plus the expected per-file hashes.
    pub intent: Intent,
    /// The region-anchored edits this plan will apply, in input order.
    pub edits: Vec<region::Edit>,
    /// Each file path's per-file drift gate as prepared (SPEC §8.1).
    pub anchors: HashMap<String, FileAnchor>,
}

/// The configured FILE-LEG edit engine. `formatter` and `linter` may be
/// `None` to skip the corresponding step. `lang`, when set, forces that
/// language for every file; when `None` each file's language is
/// auto-detected from its path via [`Lang::for_path`]. A single `Session` is
/// safe for concurrent `prepare`/`commit` calls: the per-file lock map
/// serializes writers to the same file while letting different files proceed
/// in parallel (SPEC §8.3).
pub struct Session {
    /// Reparses live and staged bytes (drift relocation + parse assertion).
    pub parser: Box<dyn ParserPort>,
    /// Computes the raw and normalized hashes recorded into the WAL intent
    /// and the post-edit hashes returned in each [`EditResult`].
    pub hasher: Box<dyn Hasher>,
    /// When set, rewrites staged bytes before linting/parsing.
    pub formatter: Option<Box<dyn Formatter>>,
    /// When set, validates staged bytes; a lint failure blocks prepare.
    pub linter: Option<Box<dyn Linter>>,
    /// Optional per-session language override; `None` = per-file auto.
    pub lang: Option<Lang>,
    /// The directory holding this session's write-ahead log.
    pub wal_dir: PathBuf,
    /// Maps a file path to its writer lock. The outer mutex is the
    /// META-lock: it is held only to fetch/create an entry, NEVER while
    /// acquiring a per-file lock, so lock acquisition can never deadlock
    /// through the map itself.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// The per-process counter feeding [`new_intent_id`]; incremented atomically
/// so concurrent `prepare` calls never observe the same value.
static INTENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a process-unique intent identifier from the PID and a monotonic
/// counter, so two intents prepared in one process never collide.
fn new_intent_id() -> String {
    let n = INTENT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    format!("intent-{}-{}", std::process::id(), n)
}

/// Buckets edits by their region's path. A `BTreeMap` keeps per-file
/// iteration deterministic (sorted paths) across prepare and commit; input
/// order is preserved within each bucket.
fn group_by_file(edits: &[region::Edit]) -> BTreeMap<String, Vec<region::Edit>> {
    let mut by_file: BTreeMap<String, Vec<region::Edit>> = BTreeMap::new();
    for e in edits {
        by_file
            .entry(e.region.path.clone())
            .or_default()
            .push(e.clone());
    }
    by_file
}

/// Reads a file whose live bytes an anchor will gate, mapping a missing path
/// to [`SessionError::NotFound`] (nothing to delete/move/edit), distinct
/// from a drift reject.
fn read_for_anchor(path: &str) -> Result<Vec<u8>, SessionError> {
    match fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(SessionError::NotFound {
            path: path.to_string(),
        }),
        Err(e) => Err(SessionError::Io {
            op: "read",
            path: path.to_string(),
            source: e,
        }),
    }
}

/// Creates the parent directories of `path` if any are missing.
fn mkdir_parents(path: &str) -> Result<(), SessionError> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| SessionError::Io {
            op: "mkdir",
            path: path.to_string(),
            source: e,
        })?;
    }
    Ok(())
}

/// Opens `path` with `O_CREATE|O_EXCL` semantics (`create_new`) so a
/// pre-existing path rejects with [`SessionError::Exists`] and is never
/// clobbered. The existence-check-and-create is one atomic kernel step, so
/// two concurrent creates cannot both win.
fn open_exclusive(path: &str) -> Result<fs::File, SessionError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                SessionError::Exists {
                    path: path.to_string(),
                }
            } else {
                SessionError::Io {
                    op: "create open",
                    path: path.to_string(),
                    source: e,
                }
            }
        })
}

/// Writes `data` into the already-open, exclusively-created `f` and fsyncs
/// so the content is durable. On failure the file is left for the caller to
/// remove.
fn write_and_sync(f: &mut fs::File, path: &str, data: &[u8]) -> Result<(), SessionError> {
    let io_err = |op: &'static str, e: io::Error| SessionError::Io {
        op,
        path: path.to_string(),
        source: e,
    };
    f.write_all(data).map_err(|e| io_err("create write", e))?;
    f.sync_all().map_err(|e| io_err("create fsync", e))?;
    Ok(())
}

/// Removes `path`, treating an already-missing file as success.
fn remove_if_exists(path: &str) -> Result<(), SessionError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SessionError::Io {
            op: "remove",
            path: path.to_string(),
            source: e,
        }),
    }
}

/// The validated, side-effect-free plan for one batch op, produced by the
/// VALIDATE phase and consumed by APPLY, so apply never re-derives an anchor
/// or re-reads a snapshot validate already proved.
enum Prepared {
    Create {
        path: String,
        staged: Vec<u8>,
    },
    Delete {
        path: String,
        live: Vec<u8>,
    },
    Move {
        from: String,
        to: String,
        live: Vec<u8>,
    },
    Edit {
        path: String,
        resolved: Vec<FileEdit>,
        spliced: Vec<u8>,
    },
}

/// Returns every path a batch touches — each op's path plus each move's
/// destination — for the union lock, rejecting the first duplicate: a
/// heterogeneous batch must touch each path AT MOST ONCE or the apply phase
/// would be undefined (ADR-0004 §10.1).
fn batch_paths(ops: &[Op]) -> Result<Vec<String>, SessionError> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut paths = Vec::new();
    for op in ops {
        let touched: [Option<&str>; 2] = match op {
            Op::Edit(e) => [Some(e.region.path.as_str()), None],
            Op::Create { path, .. } | Op::Delete { path, .. } => [Some(path.as_str()), None],
            Op::Move { from, to, .. } => [Some(from.as_str()), Some(to.as_str())],
        };
        for p in touched.into_iter().flatten() {
            if p.is_empty() {
                continue;
            }
            if !seen.insert(p) {
                return Err(SessionError::Usage(format!(
                    "batch op path {p:?} appears in more than one op; a batch must touch each path at most once"
                )));
            }
            paths.push(p.to_string());
        }
    }
    Ok(paths)
}

impl Session {
    /// Constructs a session with no formatter/linter and per-file language
    /// auto-detection.
    pub fn new(parser: Box<dyn ParserPort>, hasher: Box<dyn Hasher>, wal_dir: PathBuf) -> Session {
        Session {
            parser,
            hasher,
            formatter: None,
            linter: None,
            lang: None,
            wal_dir,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// The language for `path`: the per-session override when set, otherwise
    /// auto-detected from the path ([`Lang::for_path`] is total).
    fn lang_for(&self, path: &str) -> Lang {
        self.lang.unwrap_or_else(|| Lang::for_path(path))
    }

    /// Returns the writer lock for a path, creating it on first use under
    /// the meta-lock. The returned `Arc` is stable for the path's lifetime,
    /// so repeated commits of one file always serialize on the same lock.
    fn file_lock(&self, path: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().expect("lock map poisoned");
        locks.entry(path.to_string()).or_default().clone()
    }

    /// Returns the per-file locks for the given paths in DETERMINISTIC
    /// SORTED, DEDUPLICATED order. Sorted acquisition is the deadlock-free
    /// invariant: two concurrent ops touching the same pair of files (a move
    /// A→B racing a move B→A) always take the locks in the same global
    /// order. Deduplication means a degenerate same-path pair takes a single
    /// lock once (a self-lock would deadlock). The caller acquires the
    /// guards in the returned order.
    fn lock_arcs(&self, paths: &[&str]) -> Vec<Arc<Mutex<()>>> {
        let mut uniq: Vec<&str> = paths.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        uniq.into_iter().map(|p| self.file_lock(p)).collect()
    }

    /// Optimistically stages every region-anchored edit and records ONE WAL
    /// intent. It holds no lock. For each file it reads the live bytes,
    /// resolves every edit (a conflict/ambiguous resolve rejects the whole
    /// prepare), preview-splices, runs the formatter then the linter (a lint
    /// failure rejects), and reparses to assert the result still parses (a
    /// total parse failure rejects). On success the only on-disk effect is
    /// the WAL record — no source file is written.
    pub fn prepare(
        &self,
        edits: &[region::Edit],
        anchors: &[FileAnchor],
    ) -> Result<Plan, SessionError> {
        let by_file = group_by_file(edits);

        let mut intent = Intent {
            id: new_intent_id(),
            ..Default::default()
        };

        for (path, file_edits) in &by_file {
            let live = fs::read(path).map_err(|e| SessionError::Io {
                op: "read live file",
                path: path.clone(),
                source: e,
            })?;

            // Resolve every edit against the optimistic live snapshot. A
            // conflict or ambiguous here rejects the whole prepare before
            // any WAL is written.
            let resolved = self.resolve_edits(path, &live, file_edits)?;

            let spliced =
                edit::splice_edits(&live, &resolved).map_err(|e| SessionError::Splice {
                    path: path.clone(),
                    source: e,
                })?;

            self.format_lint_parse(path, self.lang_for(path), &spliced)?;

            intent.edits.extend(resolved);
            intent
                .expected_raw_hash
                .insert(path.clone(), raw_hash(self.hasher.as_ref(), &live));
            intent
                .expected_norm_hash
                .insert(path.clone(), norm_hash(self.hasher.as_ref(), &live));
            intent.originals.insert(path.clone(), live);
        }

        wal::append(&self.wal_dir, &intent)?;

        Ok(Plan {
            intent,
            edits: edits.to_vec(),
            anchors: anchors
                .iter()
                .map(|a| (a.path.clone(), a.clone()))
                .collect(),
        })
    }

    /// The atomic, lossless point. Per file, under that file's lock, it
    /// RE-READS the live bytes, RE-RESOLVES every edit against the current
    /// content (resolve-under-lock — a benignly shifted region lands at its
    /// current offset and a same-region conflict is rejected), splices,
    /// atomic-writes, and computes an [`EditResult`] per edit. A conflict on
    /// any file aborts the commit: every file already written in this commit
    /// is restored from its original, and the WAL is preserved for
    /// [`Session::recover`]. On full success the WAL is cleared.
    pub fn commit(&self, plan: &Plan) -> Result<Vec<EditResult>, SessionError> {
        let by_file = group_by_file(&plan.edits);

        let mut results = Vec::new();
        let mut written: Vec<&String> = Vec::with_capacity(by_file.len());

        for (path, file_edits) in &by_file {
            match self.commit_file(path, file_edits) {
                Ok(res) => {
                    written.push(path);
                    results.extend(res);
                }
                Err(e) => {
                    // Handled failure: restore every file this commit
                    // already wrote so the source is left untouched (SPEC
                    // §1.2, §8.4). The WAL is preserved so recover remains a
                    // backstop if a restore itself fails.
                    self.restore(&written, &plan.intent.originals);
                    return Err(e);
                }
            }
        }

        wal::clear(&self.wal_dir)?;
        Ok(results)
    }

    /// Applies one file's edits under that file's lock — the
    /// resolve-under-lock unit: it re-reads the live bytes inside the lock
    /// so it sees every prior concurrent commit.
    fn commit_file(
        &self,
        path: &str,
        edits: &[region::Edit],
    ) -> Result<Vec<EditResult>, SessionError> {
        let lock = self.file_lock(path);
        let _guard = lock.lock().expect("file lock poisoned");

        let live = fs::read(path).map_err(|e| SessionError::Io {
            op: "commit read",
            path: path.to_string(),
            source: e,
        })?;

        let resolved = self.resolve_edits(path, &live, edits)?;

        let out = edit::splice_edits(&live, &resolved).map_err(|e| SessionError::Splice {
            path: path.to_string(),
            source: e,
        })?;

        atomicwrite::write(Path::new(path), &out).map_err(|e| SessionError::Write {
            path: path.to_string(),
            source: e,
        })?;

        Ok(self.edit_results(path, &out, &resolved))
    }

    /// Resolves every edit against `live` via [`region::resolve`], returning
    /// the byte-range [`FileEdit`]s to splice. A conflict or ambiguous
    /// resolve becomes [`SessionError::Conflict`] so the file is rejected,
    /// never misapplied (SPEC §8.3, §8.4). Benign shifts are silently
    /// re-grounded to the resolved offset.
    fn resolve_edits(
        &self,
        path: &str,
        live: &[u8],
        edits: &[region::Edit],
    ) -> Result<Vec<FileEdit>, SessionError> {
        let lang = self.lang_for(path);
        edits
            .iter()
            .map(|e| {
                let (start, end, _status) =
                    region::resolve(self.parser.as_ref(), lang, live, &e.region).map_err(|re| {
                        SessionError::Conflict {
                            path: path.to_string(),
                            reason: re.status().to_string(),
                        }
                    })?;
                Ok(FileEdit {
                    path: path.to_string(),
                    start_byte: start,
                    end_byte: end,
                    new_text: e.new_text.clone(),
                })
            })
            .collect()
    }

    /// Runs the formatter (if set), the linter (if set; a failure rejects),
    /// and a reparse over `spliced` to assert it still parses (a TOTAL parse
    /// failure rejects; a tree with error/missing nodes is accepted — the
    /// floor is lenient). Mutates nothing on disk.
    fn format_lint_parse(
        &self,
        path: &str,
        lang: Lang,
        spliced: &[u8],
    ) -> Result<(), SessionError> {
        let mut staged = std::borrow::Cow::Borrowed(spliced);
        if let Some(f) = &self.formatter {
            staged =
                std::borrow::Cow::Owned(f.format(&staged).map_err(|e| SessionError::Format {
                    path: path.to_string(),
                    source: e,
                })?);
        }
        if let Some(l) = &self.linter {
            l.lint(&staged).map_err(|e| SessionError::Lint {
                path: path.to_string(),
                source: e,
            })?;
        }
        self.parser
            .parse(lang, &staged)
            .map_err(|e| SessionError::Parse {
                path: path.to_string(),
                source: e,
            })?;
        Ok(())
    }

    /// Builds one [`EditResult`] per applied edit over the post-write bytes
    /// `out`: the changed range, the recomputed region/file hashes, and the
    /// new 1-based line range (SPEC §8.2). Splicing is reverse-sorted by
    /// offset, so a given edit's changed range shifts only by the net length
    /// delta of every edit at a LOWER start offset; walk ascending
    /// accumulating that delta.
    fn edit_results(&self, path: &str, out: &[u8], resolved: &[FileEdit]) -> Vec<EditResult> {
        let li = LineIndex::new(out);
        let raw = raw_hash(self.hasher.as_ref(), out);
        let norm = norm_hash(self.hasher.as_ref(), out);

        let mut asc: Vec<&FileEdit> = resolved.iter().collect();
        asc.sort_by_key(|e| e.start_byte); // stable sort

        let mut results = Vec::with_capacity(asc.len());
        let mut delta: i64 = 0;
        for e in asc {
            let new_start = (e.start_byte as i64 + delta) as usize;
            let new_end = new_start + e.new_text.len();
            let (start_line, _) = li.position_for_byte(new_start);
            let (end_line, _) = li.position_for_byte(new_end);
            results.push(EditResult {
                path: path.to_string(),
                changed_start: new_start,
                changed_end: new_end,
                new_region_hash: region::hash_region(out, new_start, new_end),
                new_file_raw_hash: raw.clone(),
                new_file_norm_hash: norm.clone(),
                new_start_line: start_line,
                new_end_line: end_line,
            });
            delta += e.new_text.len() as i64 - (e.end_byte as i64 - e.start_byte as i64);
        }
        results
    }

    /// Writes the originals of every already-written path back to disk on a
    /// handled commit failure, leaving the source untouched (SPEC §1.2). A
    /// restore error is swallowed because the WAL is preserved as the
    /// recovery backstop.
    fn restore(&self, written: &[&String], originals: &HashMap<String, Vec<u8>>) {
        for p in written {
            if let Some(orig) = originals.get(*p) {
                let _ = atomicwrite::write(Path::new(p), orig);
            }
        }
    }

    /// Abandons a prepared plan. Because prepare writes nothing live (only
    /// the WAL record), rollback discards the staged edits and clears the
    /// WAL; the source files are left untouched.
    pub fn rollback(&self, plan: &mut Plan) -> Result<(), SessionError> {
        plan.edits.clear();
        wal::clear(&self.wal_dir)?;
        Ok(())
    }

    /// The restart path: replays the WAL in `dir` and converges every
    /// intent, then clears the WAL.
    ///
    /// - Every path in `originals` is restored (this is both the edit undo
    ///   and the delete undo — a delete captures the FULL prior bytes before
    ///   the unlink).
    /// - A SINGLE-OP move intent (`batch == false`) converges FORWARD to
    ///   fully-moved: the destination is (re)written from
    ///   `originals[from]` and the source unlinked; its source is SKIPPED by
    ///   the generic originals restore.
    /// - A UNIFIED BATCH intent (`batch == true`, ADR-0004 §10.1) converges
    ///   the WHOLE batch BACKWARD to fully-before: move sources ARE restored
    ///   in place and move destinations removed, so a crashed batch can
    ///   never land half-applied.
    /// - Every `creates` path is unlinked (create's undo is unlink).
    pub fn recover(&self, dir: &Path) -> Result<(), SessionError> {
        let intents = wal::replay(dir)?;
        for intent in &intents {
            // Collect move sources to skip in the restore loop ONLY for a
            // single-op forward-converging move.
            let move_src: HashSet<&str> = if intent.batch {
                HashSet::new()
            } else {
                intent.moves.iter().map(|m| m.from.as_str()).collect()
            };

            for (path, original) in &intent.originals {
                if move_src.contains(path.as_str()) {
                    continue;
                }
                atomicwrite::write(Path::new(path), original).map_err(|e| SessionError::Write {
                    path: path.clone(),
                    source: e,
                })?;
            }

            if intent.batch {
                // BATCH: converge every move BACKWARD (undo). The source
                // bytes were restored in place above; only remove the
                // destination the crashed apply may have written.
                for mv in &intent.moves {
                    remove_if_exists(&mv.to)?;
                }
            } else {
                // SINGLE-OP move converges to FULLY-MOVED (ADR-0004): the
                // destination must hold the source bytes and the source must
                // be gone, whichever point the crash stopped at.
                for mv in &intent.moves {
                    let original = intent.originals.get(&mv.from).ok_or_else(|| {
                        SessionError::RecoverMissingOriginal {
                            from: mv.from.clone(),
                            to: mv.to.clone(),
                        }
                    })?;
                    atomicwrite::write(Path::new(&mv.to), original).map_err(|e| {
                        SessionError::Write {
                            path: mv.to.clone(),
                            source: e,
                        }
                    })?;
                    remove_if_exists(&mv.from)?;
                }
            }

            // A create intent's undo is UNLINK: converge back to
            // non-existence; a path already gone is not an error.
            for path in &intent.creates {
                remove_if_exists(path)?;
            }
        }
        wal::clear(dir)?;
        Ok(())
    }

    /// Creates a new file, returning a whole-file [`EditResult`].
    ///
    /// Under the target's per-file lock it: (1) runs the format/lint/parse
    /// floor over the staged bytes (a failure rejects, NOTHING is written);
    /// (2) creates parent directories; (3) claims the path with `O_EXCL`
    /// semantics so a pre-existing path (or a concurrent create that won the
    /// race) HARD-REJECTS with [`SessionError::Exists`] — the claim happens
    /// BEFORE the WAL append, so a `creates` record can only ever name a
    /// file THIS op brought into being and recover's unlink can never delete
    /// pre-existing content; (4) WAL-logs the create; (5) writes + fsyncs;
    /// (6) clears the WAL. On any post-claim failure the partial file is
    /// removed.
    pub fn create_file(
        &self,
        path: &str,
        content: &str,
        lang: Option<Lang>,
    ) -> Result<EditResult, SessionError> {
        let lock = self.file_lock(path);
        let _guard = lock.lock().expect("file lock poisoned");

        let staged = content.as_bytes();
        let lang = lang.unwrap_or_else(|| self.lang_for(path));
        self.format_lint_parse(path, lang, staged)?;

        mkdir_parents(path)?;

        // Claim the path FIRST: the O_EXCL create atomically proves
        // non-existence (the non-existence anchor) before anything durable
        // names the path.
        let mut f = open_exclusive(path)?;

        let intent = Intent {
            id: new_intent_id(),
            creates: vec![path.to_string()],
            ..Default::default()
        };
        if let Err(e) = wal::append(&self.wal_dir, &intent) {
            drop(f);
            let _ = fs::remove_file(path);
            return Err(e.into());
        }

        if let Err(e) = write_and_sync(&mut f, path, staged) {
            drop(f);
            let _ = fs::remove_file(path);
            let _ = wal::clear(&self.wal_dir);
            return Err(e);
        }
        drop(f);

        wal::clear(&self.wal_dir)?;
        Ok(self.create_result(path, staged))
    }

    /// Deletes a file, returning a [`DeleteResult`].
    ///
    /// Under the target's per-file lock it: (1) reads the live bytes — a
    /// missing path rejects with [`SessionError::NotFound`]; (2) gates on
    /// the raw_hash drift anchor — a mismatch rejects with
    /// [`SessionError::Drift`] and NOTHING is unlinked (Båge never discards
    /// bytes the caller did not see); (3) WAL-logs the delete with the FULL
    /// prior bytes in `originals` BEFORE the unlink, so a crash in the
    /// window is recoverable; (4) unlinks; (5) clears the WAL.
    pub fn delete_file(
        &self,
        path: &str,
        expected_raw_hash: &str,
    ) -> Result<DeleteResult, SessionError> {
        let lock = self.file_lock(path);
        let _guard = lock.lock().expect("file lock poisoned");

        let live = read_for_anchor(path)?;

        let live_raw = raw_hash(self.hasher.as_ref(), &live);
        if live_raw != expected_raw_hash {
            return Err(SessionError::Drift {
                path: path.to_string(),
            });
        }

        // Durable undo record BEFORE the destructive unlink.
        let intent = Intent {
            id: new_intent_id(),
            deletes: vec![path.to_string()],
            originals: HashMap::from([(path.to_string(), live)]),
            ..Default::default()
        };
        wal::append(&self.wal_dir, &intent)?;

        // If the unlink fails the WAL record is preserved as the recovery
        // backstop, so it is NOT cleared.
        fs::remove_file(path).map_err(|e| SessionError::Io {
            op: "delete unlink",
            path: path.to_string(),
            source: e,
        })?;

        wal::clear(&self.wal_dir)?;
        Ok(DeleteResult {
            path: path.to_string(),
            raw_hash: live_raw,
        })
    }

    /// Relocates `from` to `to`, preserving the bytes unchanged, and returns
    /// a [`MoveResult`].
    ///
    /// Under BOTH per-file locks (sorted order — deadlock-free even for
    /// crossing moves) it: (1) reads the live source (missing rejects
    /// not-found); (2) gates the source raw_hash (mismatch rejects drift);
    /// (3) O_EXCL-claims + writes + fsyncs the destination — NEVER
    /// `fs::rename`, which would clobber an existing destination; (4)
    /// WAL-logs the move with the source bytes in `originals` BEFORE the
    /// source unlink; (5) unlinks the source; (6) clears the WAL. On any
    /// reject NOTHING moves.
    pub fn move_file(
        &self,
        from: &str,
        to: &str,
        expected_raw_hash: &str,
    ) -> Result<MoveResult, SessionError> {
        if to.is_empty() {
            return Err(SessionError::Usage(format!(
                "move {from:?} requires a destination"
            )));
        }

        let arcs = self.lock_arcs(&[from, to]);
        let _guards: Vec<MutexGuard<'_, ()>> = arcs
            .iter()
            .map(|m| m.lock().expect("file lock poisoned"))
            .collect();

        let live = read_for_anchor(from)?;

        let live_raw = raw_hash(self.hasher.as_ref(), &live);
        if live_raw != expected_raw_hash {
            return Err(SessionError::Drift {
                path: from.to_string(),
            });
        }

        mkdir_parents(to)?;

        // Claim + write the destination BEFORE the WAL append, so a rejected
        // destination never logs a moves record recover would act on.
        let mut dst = open_exclusive(to)?;
        if let Err(e) = write_and_sync(&mut dst, to, &live) {
            drop(dst);
            let _ = fs::remove_file(to);
            return Err(e);
        }
        drop(dst);

        // Durable record BEFORE the source unlink: the recoverable window
        // always has the source bytes safe.
        let intent = Intent {
            id: new_intent_id(),
            moves: vec![Move {
                from: from.to_string(),
                to: to.to_string(),
            }],
            originals: HashMap::from([(from.to_string(), live.clone())]),
            ..Default::default()
        };
        if let Err(e) = wal::append(&self.wal_dir, &intent) {
            let _ = fs::remove_file(to);
            return Err(e.into());
        }

        fs::remove_file(from).map_err(|e| SessionError::Io {
            op: "move unlink src",
            path: from.to_string(),
            source: e,
        })?;

        wal::clear(&self.wal_dir)?;
        Ok(MoveResult {
            from: from.to_string(),
            dest: self.create_result(to, &live),
        })
    }

    /// Applies a heterogeneous op list as ONE all-or-nothing change
    /// (ADR-0004 §10.1). It locks the UNION of every op's path (plus each
    /// move's destination) in sorted order, then runs four phases under that
    /// lock: (A) VALIDATE every anchor, writing nothing — any failure
    /// rejects the whole batch having written NOTHING; (B) append ONE
    /// unified WAL intent (`batch = true`) capturing every op's undo; (C)
    /// APPLY every op — on any apply failure ROLL BACK every already-applied
    /// op from that intent (the WAL is preserved as the recover backstop);
    /// (D) clear the WAL on full success. Returns one [`BatchResult`] per op
    /// in input order. An empty batch or a duplicate touched path is a
    /// usage reject.
    pub fn apply_batch(&self, ops: &[Op]) -> Result<Vec<BatchResult>, SessionError> {
        if ops.is_empty() {
            return Err(SessionError::Usage(
                "apply_batch requires at least one op".to_string(),
            ));
        }

        let paths = batch_paths(ops)?;
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let arcs = self.lock_arcs(&path_refs);
        let _guards: Vec<MutexGuard<'_, ()>> = arcs
            .iter()
            .map(|m| m.lock().expect("file lock poisoned"))
            .collect();

        // Phase A — VALIDATE every op's anchor up front, writing nothing.
        let (prepared, intent) = self.validate_batch(ops)?;

        // Phase B — ONE unified intent so a crash anywhere in APPLY
        // converges via recover.
        wal::append(&self.wal_dir, &intent)?;

        // Phase C — APPLY every op; roll back from the unified intent on any
        // failure (WAL preserved as the recover backstop).
        match self.apply_ops(&prepared) {
            Ok(results) => {
                // Phase D — full success: clear the WAL.
                wal::clear(&self.wal_dir)?;
                Ok(results)
            }
            Err(e) => {
                self.rollback_batch(&intent);
                Err(e)
            }
        }
    }

    /// The VALIDATE phase: checks every op's anchor against the live
    /// filesystem WITHOUT writing anything and folds every op's undo into
    /// one unified intent.
    fn validate_batch(&self, ops: &[Op]) -> Result<(Vec<Prepared>, Intent), SessionError> {
        let mut intent = Intent {
            id: new_intent_id(),
            batch: true,
            ..Default::default()
        };
        let mut prepared = Vec::with_capacity(ops.len());
        for op in ops {
            prepared.push(self.validate_op(op, &mut intent)?);
        }
        Ok((prepared, intent))
    }

    /// Validates ONE op's anchor and folds its undo into `intent`, returning
    /// the side-effect-free apply plan.
    fn validate_op(&self, op: &Op, intent: &mut Intent) -> Result<Prepared, SessionError> {
        match op {
            Op::Create {
                path,
                content,
                lang,
            } => {
                if fs::symlink_metadata(path).is_ok() {
                    return Err(SessionError::Exists { path: path.clone() });
                }
                let staged = content.as_bytes().to_vec();
                self.format_lint_parse(path, lang.unwrap_or_else(|| self.lang_for(path)), &staged)?;
                intent.creates.push(path.clone());
                Ok(Prepared::Create {
                    path: path.clone(),
                    staged,
                })
            }
            Op::Delete {
                path,
                expected_raw_hash,
            } => {
                let live = read_for_anchor(path)?;
                if raw_hash(self.hasher.as_ref(), &live) != *expected_raw_hash {
                    return Err(SessionError::Drift { path: path.clone() });
                }
                intent.deletes.push(path.clone());
                intent.originals.insert(path.clone(), live.clone());
                Ok(Prepared::Delete {
                    path: path.clone(),
                    live,
                })
            }
            Op::Move {
                from,
                to,
                expected_raw_hash,
            } => {
                if to.is_empty() {
                    return Err(SessionError::Usage(format!(
                        "move {from:?} requires a destination"
                    )));
                }
                let live = read_for_anchor(from)?;
                if raw_hash(self.hasher.as_ref(), &live) != *expected_raw_hash {
                    return Err(SessionError::Drift { path: from.clone() });
                }
                if fs::symlink_metadata(to).is_ok() {
                    return Err(SessionError::Exists { path: to.clone() });
                }
                intent.moves.push(Move {
                    from: from.clone(),
                    to: to.clone(),
                });
                intent.originals.insert(from.clone(), live.clone());
                Ok(Prepared::Move {
                    from: from.clone(),
                    to: to.clone(),
                    live,
                })
            }
            Op::Edit(e) => {
                let path = e.region.path.clone();
                let live = read_for_anchor(&path)?;
                let resolved = self.resolve_edits(&path, &live, std::slice::from_ref(e))?;
                let spliced =
                    edit::splice_edits(&live, &resolved).map_err(|err| SessionError::Splice {
                        path: path.clone(),
                        source: err,
                    })?;
                self.format_lint_parse(&path, self.lang_for(&path), &spliced)?;
                intent.originals.insert(path.clone(), live);
                Ok(Prepared::Edit {
                    path,
                    resolved,
                    spliced,
                })
            }
        }
    }

    /// The APPLY phase: applies each prepared op in input order. On the
    /// first failure it returns the error WITHOUT rolling back — the caller
    /// rolls back from the unified intent, so the rollback uses the full
    /// undo record.
    fn apply_ops(&self, prepared: &[Prepared]) -> Result<Vec<BatchResult>, SessionError> {
        prepared.iter().map(|p| self.apply_op(p)).collect()
    }

    /// Performs ONE prepared op's on-disk effect (no anchor re-check —
    /// VALIDATE already proved it under the same held lock).
    fn apply_op(&self, p: &Prepared) -> Result<BatchResult, SessionError> {
        match p {
            Prepared::Create { path, staged } => {
                Ok(BatchResult::Create(self.apply_create(path, staged)?))
            }
            Prepared::Delete { path, live } => {
                fs::remove_file(path).map_err(|e| SessionError::Io {
                    op: "batch delete unlink",
                    path: path.clone(),
                    source: e,
                })?;
                Ok(BatchResult::Delete(DeleteResult {
                    path: path.clone(),
                    raw_hash: raw_hash(self.hasher.as_ref(), live),
                }))
            }
            Prepared::Move { from, to, live } => {
                Ok(BatchResult::Move(self.apply_move(from, to, live)?))
            }
            Prepared::Edit {
                path,
                resolved,
                spliced,
            } => {
                atomicwrite::write(Path::new(path), spliced).map_err(|e| SessionError::Write {
                    path: path.clone(),
                    source: e,
                })?;
                Ok(BatchResult::Edit(
                    self.edit_results(path, spliced, resolved),
                ))
            }
        }
    }

    /// Brings a batch create's file into being with the O_EXCL claim +
    /// fsynced write. The non-existence anchor was proven in VALIDATE, but
    /// O_EXCL re-proves it so nothing can be clobbered.
    fn apply_create(&self, path: &str, staged: &[u8]) -> Result<EditResult, SessionError> {
        mkdir_parents(path)?;
        let mut f = open_exclusive(path)?;
        if let Err(e) = write_and_sync(&mut f, path, staged) {
            drop(f);
            let _ = fs::remove_file(path);
            return Err(e);
        }
        Ok(self.create_result(path, staged))
    }

    /// Relocates the validated source bytes to the destination with the
    /// O_EXCL claim + fsynced write, then unlinks the source.
    fn apply_move(&self, from: &str, to: &str, live: &[u8]) -> Result<MoveResult, SessionError> {
        mkdir_parents(to)?;
        let mut dst = open_exclusive(to)?;
        if let Err(e) = write_and_sync(&mut dst, to, live) {
            drop(dst);
            let _ = fs::remove_file(to);
            return Err(e);
        }
        drop(dst);
        if let Err(e) = fs::remove_file(from) {
            let _ = fs::remove_file(to);
            return Err(SessionError::Io {
                op: "batch move unlink src",
                path: from.to_string(),
                source: e,
            });
        }
        Ok(MoveResult {
            from: from.to_string(),
            dest: self.create_result(to, live),
        })
    }

    /// Undoes every op recorded in the unified intent on an APPLY-phase
    /// failure, returning the filesystem to its pre-batch state: restore
    /// edited/deleted originals in place (move sources skipped — their bytes
    /// belong to the move reversal), reverse every move, unlink every
    /// created file. Errors are swallowed because the WAL is preserved as
    /// the recover backstop.
    fn rollback_batch(&self, intent: &Intent) {
        let move_src: HashSet<&str> = intent.moves.iter().map(|m| m.from.as_str()).collect();

        for (path, original) in &intent.originals {
            if move_src.contains(path.as_str()) {
                continue;
            }
            let _ = atomicwrite::write(Path::new(path), original);
        }

        for mv in &intent.moves {
            if let Some(original) = intent.originals.get(&mv.from) {
                let _ = atomicwrite::write(Path::new(&mv.from), original);
            }
            let _ = fs::remove_file(&mv.to);
        }

        for path in &intent.creates {
            let _ = fs::remove_file(path);
        }
    }

    /// The whole-file [`EditResult`] for a created (or move-destination)
    /// file: the changed range spans the entire content and the hashes/line
    /// range are computed over the new bytes (SPEC §8.2 shape reused).
    fn create_result(&self, path: &str, data: &[u8]) -> EditResult {
        let li = LineIndex::new(data);
        let (start_line, _) = li.position_for_byte(0);
        let (end_line, _) = li.position_for_byte(data.len());
        EditResult {
            path: path.to_string(),
            changed_start: 0,
            changed_end: data.len(),
            new_region_hash: region::hash_region(data, 0, data.len()),
            new_file_raw_hash: raw_hash(self.hasher.as_ref(), data),
            new_file_norm_hash: norm_hash(self.hasher.as_ref(), data),
            new_start_line: start_line,
            new_end_line: end_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::FakeLinter;
    use crate::hashing::XxHasher;
    use crate::parser::{Adapter, ByteRange, InputEdit, Tree};
    use crate::region::Region;

    /// A small valid Go file with three sibling funcs the tests edit.
    const GO_SRC: &str = "package main\n\nfunc a() {}\n\nfunc b() {}\n\nfunc c() {}\n";

    fn new_session(wal_dir: &Path) -> Session {
        let mut s = Session::new(
            Box::new(Adapter::new()),
            Box::new(XxHasher),
            wal_dir.to_path_buf(),
        );
        s.lang = Some(Lang::Go);
        s
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> String {
        let p = dir.join(name);
        fs::write(&p, contents).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn read_str(path: &str) -> String {
        String::from_utf8(fs::read(path).unwrap()).unwrap()
    }

    fn raw_of(s: &str) -> String {
        raw_hash(&XxHasher, s.as_bytes())
    }

    /// Anchors the byte region of `from` within `src` and replaces it with
    /// `to`.
    fn region_edit(path: &str, src: &str, from: &str, to: &str) -> region::Edit {
        let start = src.find(from).unwrap();
        let end = start + from.len();
        region::Edit {
            region: Region {
                path: path.to_string(),
                start_byte: start as i64,
                end_byte: end as i64,
                region_hash: region::hash_region(src.as_bytes(), start, end),
                ..Default::default()
            },
            new_text: to.to_string(),
        }
    }

    #[test]
    fn prepare_commit_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let e = region_edit(&path, GO_SRC, "func a() {}", "func a() { return }");
        let anchor = region::file_anchor(&XxHasher, &path, GO_SRC.as_bytes());

        let plan = s.prepare(&[e], &[anchor]).unwrap();
        // Source untouched until commit.
        assert_eq!(read_str(&path), GO_SRC);

        let results = s.commit(&plan).unwrap();
        let want = GO_SRC.replace("func a() {}", "func a() { return }");
        assert_eq!(read_str(&path), want);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.path, path);
        assert_eq!(&want[r.changed_start..r.changed_end], "func a() { return }");
        assert_eq!(
            r.new_region_hash,
            region::hash_region(want.as_bytes(), r.changed_start, r.changed_end)
        );
        assert_eq!(r.new_file_raw_hash, raw_of(&want));
        assert_eq!((r.new_start_line, r.new_end_line), (3, 3));
    }

    #[test]
    fn prepare_conflict_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        // The live file's a() body differs from what the edit was anchored
        // against, so the region_hash matches nothing — a hard conflict.
        let live = GO_SRC.replace("func a() {}", "func a() { x := 1; _ = x }");
        let path = write_file(dir.path(), "main.go", &live);
        let s = new_session(wal_dir.path());

        let e = region_edit(&path, GO_SRC, "func a() {}", "func a() { return }");
        let err = s.prepare(&[e], &[]).unwrap_err();
        assert_eq!(err.kind(), Kind::Conflict);
        assert_eq!(err.path(), Some(path.as_str()));
        assert_eq!(read_str(&path), live);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn prepare_linter_failure_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let mut s = new_session(wal_dir.path());
        s.linter = Some(Box::new(FakeLinter {
            err: Some(ToolError {
                tool: "fake".into(),
                message: "lint boom".into(),
            }),
            ..Default::default()
        }));

        let e = region_edit(&path, GO_SRC, "func a() {}", "func a() { return }");
        let err = s.prepare(&[e], &[]).unwrap_err();
        assert!(matches!(err, SessionError::Lint { .. }), "{err}");
        assert_eq!(err.kind(), Kind::Io);
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    /// Wraps the real adapter, failing the parse when the source contains a
    /// sentinel — tree-sitter is error-tolerant, so the total-parse-failure
    /// floor (SPEC §8.4) is exercised by injection, not by bad bytes.
    struct ErrParser {
        inner: Adapter,
        fail_on: &'static [u8],
    }

    impl ParserPort for ErrParser {
        fn parse(&self, lang: Lang, src: &[u8]) -> Result<Tree, ParseError> {
            if src.windows(self.fail_on.len()).any(|w| w == self.fail_on) {
                return Err(ParseError::NoTree { lang });
            }
            self.inner.parse(lang, src)
        }

        fn parse_incremental(
            &self,
            lang: Lang,
            src: &[u8],
            old: &mut Tree,
            edit: InputEdit,
        ) -> Result<Tree, ParseError> {
            self.inner.parse_incremental(lang, src, old, edit)
        }

        fn changed_ranges(&self, old: &Tree, new: &Tree) -> Vec<ByteRange> {
            self.inner.changed_ranges(old, new)
        }
    }

    #[test]
    fn prepare_parse_failure_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let mut s = new_session(wal_dir.path());
        s.parser = Box::new(ErrParser {
            inner: Adapter::new(),
            fail_on: b"SENTINEL_BODY",
        });

        let e = region_edit(
            &path,
            GO_SRC,
            "func a() {}",
            "func a() { SENTINEL_BODY := 1; _ = SENTINEL_BODY }",
        );
        let err = s.prepare(&[e], &[]).unwrap_err();
        assert!(matches!(err, SessionError::Parse { .. }), "{err}");
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn lenient_parse_floor_accepts_error_nodes() {
        // A tree WITH error nodes is accepted — only total parse failure
        // rejects (the lenient floor).
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let path = dir.path().join("broken.go").to_string_lossy().into_owned();
        let res = s.create_file(&path, "func broken(( {\n", None).unwrap();
        assert_eq!(res.changed_end, "func broken(( {\n".len());
        assert_eq!(read_str(&path), "func broken(( {\n");
    }

    #[test]
    fn prepare_writes_and_commit_clears_wal() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let e = region_edit(&path, GO_SRC, "func b() {}", "func b() { return }");
        let plan = s.prepare(&[e], &[]).unwrap();

        let intents = wal::replay(wal_dir.path()).unwrap();
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].id, plan.intent.id);
        assert_eq!(intents[0].originals[&path], GO_SRC.as_bytes());

        s.commit(&plan).unwrap();
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn rollback_leaves_source_untouched_and_clears_wal() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let e = region_edit(&path, GO_SRC, "func a() {}", "func a() { return }");
        let mut plan = s.prepare(&[e], &[]).unwrap();
        s.rollback(&mut plan).unwrap();
        assert_eq!(read_str(&path), GO_SRC);
        assert!(plan.edits.is_empty());
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn recover_restores_originals() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let e = region_edit(&path, GO_SRC, "func a() {}", "func a() { return }");
        s.prepare(&[e], &[]).unwrap();
        // Simulate a crash: live file corrupted, commit never ran.
        fs::write(&path, "GARBAGE\n").unwrap();
        s.recover(wal_dir.path()).unwrap();
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn multi_file_partial_failure_restores() {
        // Two files prepared in ONE plan; between prepare and commit file
        // B's target region is mutated so B can no longer resolve under
        // lock. Commit must reject with conflict, file A must be restored to
        // its exact original, file B must hold its out-of-band mutation, and
        // the WAL must be PRESERVED so recover stays a backstop.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path_a = write_file(dir.path(), "a.go", GO_SRC);
        let path_b = write_file(dir.path(), "b.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let edits = vec![
            region_edit(&path_a, GO_SRC, "func a() {}", "func a() { return }"),
            region_edit(&path_b, GO_SRC, "func a() {}", "func a() { panic(1) }"),
        ];
        let plan = s.prepare(&edits, &[]).unwrap();

        let mutated_b = GO_SRC.replace("func a() {}", "func a() { x := 9; _ = x }");
        fs::write(&path_b, &mutated_b).unwrap();

        let err = s.commit(&plan).unwrap_err();
        assert_eq!(err.kind(), Kind::Conflict);
        assert_eq!(read_str(&path_a), GO_SRC);
        assert_eq!(read_str(&path_b), mutated_b);
        assert!(!wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn prepare_whitespace_shift_re_resolves() {
        // The live file prepended a header, shifting every func without
        // changing any func's own bytes: the stale in-place hash misses but
        // the CST relocation finds the node, so the edit applies.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let live = format!("// header\n{GO_SRC}");
        let path = write_file(dir.path(), "main.go", &live);
        let s = new_session(wal_dir.path());

        let e = region_edit(&path, GO_SRC, "func c() {}", "func c() { _ = 0 }");
        let plan = s.prepare(&[e], &[]).unwrap();
        s.commit(&plan).unwrap();
        let final_src = read_str(&path);
        assert!(final_src.contains("func c() { _ = 0 }"), "{final_src}");
        assert!(final_src.starts_with("// header\n"), "{final_src}");
    }

    #[test]
    fn no_lost_update_stale_snapshot_re_resolves() {
        // B prepares against an old snapshot, A commits first (shifting
        // offsets), then B commits: B must land at the SHIFTED location so
        // A's edit is preserved (SPEC §8.3 no lost update).
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let plan_b = s
            .prepare(
                &[region_edit(
                    &path,
                    GO_SRC,
                    "func c() {}",
                    "func c() { _ = 0 }",
                )],
                &[],
            )
            .unwrap();
        let plan_a = s
            .prepare(
                &[region_edit(
                    &path,
                    GO_SRC,
                    "func a() {}",
                    "func a() { x := 1; _ = x; return }",
                )],
                &[],
            )
            .unwrap();
        s.commit(&plan_a).unwrap();
        s.commit(&plan_b).unwrap();

        let final_src = read_str(&path);
        assert!(final_src.contains("func a() { x := 1; _ = x; return }"));
        assert!(final_src.contains("func c() { _ = 0 }"));
        assert!(final_src.contains("func b() {}"));
    }

    #[test]
    fn create_file_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let path = dir
            .path()
            .join("nested/pkg/new.go")
            .to_string_lossy()
            .into_owned();
        let content = "package pkg\n\nfunc New() {}\n";

        let res = s.create_file(&path, content, None).unwrap();
        assert_eq!(read_str(&path), content);
        assert_eq!(res.path, path);
        assert_eq!((res.changed_start, res.changed_end), (0, content.len()));
        assert_eq!(
            res.new_region_hash,
            region::hash_region(content.as_bytes(), 0, content.len())
        );
        assert_eq!(res.new_file_raw_hash, raw_of(content));
        assert_eq!((res.new_start_line, res.new_end_line), (1, 4));
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn create_file_rejects_existing() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let err = s.create_file(&path, "package other\n", None).unwrap_err();
        assert_eq!(err.kind(), Kind::Exists);
        // Never clobbered, and the reject left no WAL record whose recovery
        // could unlink the pre-existing file.
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn recover_unlinks_half_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "half.go", "package half\n");
        let s = new_session(wal_dir.path());

        // Simulate a crash after the O_EXCL claim + WAL record but before
        // the WAL clear: the intent names the created path.
        let intent = Intent {
            id: "crashed-create".into(),
            creates: vec![path.clone()],
            ..Default::default()
        };
        wal::append(wal_dir.path(), &intent).unwrap();

        s.recover(wal_dir.path()).unwrap();
        assert!(!Path::new(&path).exists());
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn delete_file_removes_matching_and_clears_wal() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let res = s.delete_file(&path, &raw_of(GO_SRC)).unwrap();
        assert!(!Path::new(&path).exists());
        assert_eq!(res.path, path);
        assert_eq!(res.raw_hash, raw_of(GO_SRC));
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn delete_file_rejects_drift() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let err = s.delete_file(&path, "0000000000000000").unwrap_err();
        assert_eq!(err.kind(), Kind::Drift);
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn delete_file_rejects_missing() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let path = dir.path().join("gone.go").to_string_lossy().into_owned();

        let err = s.delete_file(&path, "whatever").unwrap_err();
        assert_eq!(err.kind(), Kind::NotFound);
    }

    #[test]
    fn recover_restores_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let path = dir.path().join("del.go").to_string_lossy().into_owned();

        // Simulate a crash between the durable delete record and the WAL
        // clear: the file is already unlinked, its bytes are in originals.
        let intent = Intent {
            id: "crashed-delete".into(),
            deletes: vec![path.clone()],
            originals: HashMap::from([(path.clone(), GO_SRC.as_bytes().to_vec())]),
            ..Default::default()
        };
        wal::append(wal_dir.path(), &intent).unwrap();

        s.recover(wal_dir.path()).unwrap();
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn move_file_relocates_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let from = write_file(dir.path(), "old.go", GO_SRC);
        let to = dir.path().join("sub/new.go").to_string_lossy().into_owned();
        let s = new_session(wal_dir.path());

        let res = s.move_file(&from, &to, &raw_of(GO_SRC)).unwrap();
        assert!(!Path::new(&from).exists());
        assert_eq!(read_str(&to), GO_SRC);
        assert_eq!(res.from, from);
        assert_eq!(res.dest.path, to);
        assert_eq!(res.dest.new_file_raw_hash, raw_of(GO_SRC));
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn move_file_rejects_source_drift() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let from = write_file(dir.path(), "old.go", GO_SRC);
        let to = dir.path().join("new.go").to_string_lossy().into_owned();
        let s = new_session(wal_dir.path());

        let err = s.move_file(&from, &to, "0000000000000000").unwrap_err();
        assert_eq!(err.kind(), Kind::Drift);
        assert_eq!(read_str(&from), GO_SRC);
        assert!(!Path::new(&to).exists());
    }

    #[test]
    fn move_file_refuses_to_clobber_dest() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let from = write_file(dir.path(), "old.go", GO_SRC);
        let to = write_file(dir.path(), "dest.go", "package dest\n");
        let s = new_session(wal_dir.path());

        let err = s.move_file(&from, &to, &raw_of(GO_SRC)).unwrap_err();
        assert_eq!(err.kind(), Kind::Exists);
        // Source intact, destination unchanged, no WAL record left.
        assert_eq!(read_str(&from), GO_SRC);
        assert_eq!(read_str(&to), "package dest\n");
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn recover_converges_single_move_forward() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let from = write_file(dir.path(), "old.go", GO_SRC);
        let to = dir.path().join("new.go").to_string_lossy().into_owned();
        let s = new_session(wal_dir.path());

        // Crash after the move's WAL record but before the source unlink:
        // source present, destination not yet visible. A single-op move
        // converges FORWARD to fully-moved.
        let intent = Intent {
            id: "crashed-move".into(),
            moves: vec![Move {
                from: from.clone(),
                to: to.clone(),
            }],
            originals: HashMap::from([(from.clone(), GO_SRC.as_bytes().to_vec())]),
            ..Default::default()
        };
        wal::append(wal_dir.path(), &intent).unwrap();

        s.recover(wal_dir.path()).unwrap();
        assert!(!Path::new(&from).exists());
        assert_eq!(read_str(&to), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn recover_converges_crashed_batch_backward() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());

        // Batch state mid-crash: the move fully applied (src gone, dest
        // written), the create landed, the edited file was overwritten.
        let move_from = dir.path().join("src.go").to_string_lossy().into_owned();
        let move_to = write_file(dir.path(), "dst.go", GO_SRC);
        let created = write_file(dir.path(), "created.go", "package c\n");
        let edited = write_file(dir.path(), "edited.go", "MUTATED\n");

        let intent = Intent {
            id: "crashed-batch".into(),
            batch: true,
            moves: vec![Move {
                from: move_from.clone(),
                to: move_to.clone(),
            }],
            creates: vec![created.clone()],
            originals: HashMap::from([
                (move_from.clone(), GO_SRC.as_bytes().to_vec()),
                (edited.clone(), b"package e\n".to_vec()),
            ]),
            ..Default::default()
        };
        wal::append(wal_dir.path(), &intent).unwrap();

        s.recover(wal_dir.path()).unwrap();
        // The WHOLE batch converged BACKWARD to fully-before: the move
        // source restored in place, the destination removed, the created
        // file unlinked, the edit undone.
        assert_eq!(read_str(&move_from), GO_SRC);
        assert!(!Path::new(&move_to).exists());
        assert!(!Path::new(&created).exists());
        assert_eq!(read_str(&edited), "package e\n");
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn apply_batch_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());

        let edit_path = write_file(dir.path(), "edit.go", GO_SRC);
        let del_path = write_file(dir.path(), "del.go", GO_SRC);
        let move_from = write_file(dir.path(), "from.go", GO_SRC);
        let move_to = dir.path().join("to.go").to_string_lossy().into_owned();
        let create_path = dir.path().join("new.go").to_string_lossy().into_owned();

        let ops = vec![
            Op::Edit(region_edit(
                &edit_path,
                GO_SRC,
                "func a() {}",
                "func a() { return }",
            )),
            Op::Create {
                path: create_path.clone(),
                content: "package created\n".into(),
                lang: None,
            },
            Op::Delete {
                path: del_path.clone(),
                expected_raw_hash: raw_of(GO_SRC),
            },
            Op::Move {
                from: move_from.clone(),
                to: move_to.clone(),
                expected_raw_hash: raw_of(GO_SRC),
            },
        ];

        let results = s.apply_batch(&ops).unwrap();
        assert_eq!(results.len(), 4);
        // Result variants match the op kinds in input order.
        assert!(matches!(&results[0], BatchResult::Edit(rs) if rs.len() == 1));
        assert!(matches!(&results[1], BatchResult::Create(r) if r.path == create_path));
        assert!(matches!(&results[2], BatchResult::Delete(r) if r.path == del_path));
        assert!(
            matches!(&results[3], BatchResult::Move(r) if r.from == move_from && r.dest.path == move_to)
        );

        assert!(read_str(&edit_path).contains("func a() { return }"));
        assert_eq!(read_str(&create_path), "package created\n");
        assert!(!Path::new(&del_path).exists());
        assert!(!Path::new(&move_from).exists());
        assert_eq!(read_str(&move_to), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn apply_batch_rejects_on_stale_anchor_all_or_nothing() {
        // A stale anchor on op 2 rejects the WHOLE batch at VALIDATE, before
        // any durable record: op 1's file is untouched, nothing half-applied.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let edit_path = write_file(dir.path(), "edit.go", GO_SRC);
        let del_path = write_file(dir.path(), "del.go", GO_SRC);

        let ops = vec![
            Op::Edit(region_edit(
                &edit_path,
                GO_SRC,
                "func a() {}",
                "func a() { return }",
            )),
            Op::Delete {
                path: del_path.clone(),
                expected_raw_hash: "0000000000000000".into(),
            },
        ];
        let err = s.apply_batch(&ops).unwrap_err();
        assert_eq!(err.kind(), Kind::Drift);
        assert_eq!(read_str(&edit_path), GO_SRC);
        assert_eq!(read_str(&del_path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn apply_batch_rolls_back_on_apply_failure() {
        // Op 2 passes VALIDATE (its path does not exist) but fails APPLY
        // (its parent "directory" is a regular file, so mkdir fails) after
        // op 1's edit already landed. The batch must roll back: op 1's file
        // restored, and the WAL preserved as the recover backstop.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let edit_path = write_file(dir.path(), "edit.go", GO_SRC);
        let blocker = write_file(dir.path(), "blocker", "not a dir\n");
        let create_path = format!("{blocker}/new.go");

        let ops = vec![
            Op::Edit(region_edit(
                &edit_path,
                GO_SRC,
                "func a() {}",
                "func a() { return }",
            )),
            Op::Create {
                path: create_path.clone(),
                content: "package blocked\n".into(),
                lang: None,
            },
        ];
        let err = s.apply_batch(&ops).unwrap_err();
        assert_eq!(err.kind(), Kind::Io);
        assert_eq!(read_str(&edit_path), GO_SRC);
        assert!(!Path::new(&create_path).exists());
        assert!(!wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn apply_batch_rejects_duplicate_paths() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        let path = write_file(dir.path(), "dup.go", GO_SRC);

        let ops = vec![
            Op::Delete {
                path: path.clone(),
                expected_raw_hash: raw_of(GO_SRC),
            },
            Op::Edit(region_edit(
                &path,
                GO_SRC,
                "func a() {}",
                "func a() { return }",
            )),
        ];
        let err = s.apply_batch(&ops).unwrap_err();
        assert_eq!(err.kind(), Kind::Usage);
        assert_eq!(read_str(&path), GO_SRC);
        assert!(wal::replay(wal_dir.path()).unwrap().is_empty());
    }

    #[test]
    fn apply_batch_rejects_empty() {
        let wal_dir = tempfile::tempdir().unwrap();
        let s = new_session(wal_dir.path());
        assert_eq!(s.apply_batch(&[]).unwrap_err().kind(), Kind::Usage);
    }

    #[test]
    fn errkind_envelope_json_matches_go() {
        // Kind strings match Go's exactly.
        for (k, want) in [
            (Kind::Conflict, "conflict"),
            (Kind::Drift, "drift"),
            (Kind::Exists, "exists"),
            (Kind::NotFound, "not-found"),
            (Kind::Usage, "usage"),
            (Kind::Io, "io"),
        ] {
            assert_eq!(k.to_string(), want);
            assert_eq!(serde_json::to_value(k).unwrap(), want);
        }

        // Conflict envelope carries kind + path + message.
        let ce = SessionError::Conflict {
            path: "f".into(),
            reason: "conflict".into(),
        };
        let j = serde_json::to_value(envelope(&ce)).unwrap();
        assert_eq!(j["kind"], "conflict");
        assert_eq!(j["path"], "f");
        assert!(j["message"].as_str().unwrap().contains("conflict"));

        // A pathless error omits the "path" key entirely (Go omitempty).
        let usage = SessionError::Usage("bad".into());
        let j = serde_json::to_value(envelope(&usage)).unwrap();
        assert_eq!(j["kind"], "usage");
        assert!(j.get("path").is_none());

        // A wrapped OS not-found classifies as not-found, not io (Go's
        // os.ErrNotExist mapping).
        let nf = SessionError::Io {
            op: "open",
            path: "x.go".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert_eq!(nf.kind(), Kind::NotFound);
    }

    #[test]
    fn envelope_render_text_single_line() {
        let env = envelope(&SessionError::Conflict {
            path: "f".into(),
            reason: "conflict".into(),
        });
        let mut buf = Vec::new();
        env.render_text(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        assert!(out.starts_with("bage: conflict: "), "{out}");
        assert!(out.contains(&env.message), "{out}");
    }

    #[test]
    fn results_marshal_snake_case_json() {
        let d = DeleteResult {
            path: "p.go".into(),
            raw_hash: "h".into(),
        };
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["path"], "p.go");
        assert_eq!(j["raw_hash"], "h");

        let m = MoveResult {
            from: "a.go".into(),
            dest: EditResult {
                path: "b.go".into(),
                ..Default::default()
            },
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["from"], "a.go");
        assert_eq!(j["dest"]["path"], "b.go");
        assert!(j["dest"].get("new_file_raw_hash").is_some());
    }

    // ---- concurrency ----

    #[test]
    fn concurrent_prepare_shared_wal_integrity() {
        // N threads each prepare a DISJOINT edit (one per file) into the
        // SAME wal dir at once. The shared log must end up with exactly N
        // cleanly-decoded records — no torn or interleaved line — each with
        // a distinct, non-empty intent ID.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        const N: usize = 16;
        let s = new_session(wal_dir.path());

        std::thread::scope(|scope| {
            for i in 0..N {
                let s = &s;
                let dir = dir.path();
                scope.spawn(move || {
                    let name = format!("f{i}.go");
                    let p = write_file(dir, &name, GO_SRC);
                    let e = region_edit(&p, GO_SRC, "func a() {}", "func a() { return }");
                    s.prepare(&[e], &[]).unwrap();
                });
            }
        });

        let intents = wal::replay(wal_dir.path()).unwrap();
        assert_eq!(intents.len(), N, "torn/interleaved append");
        let mut seen = HashSet::new();
        for intent in &intents {
            assert!(!intent.id.is_empty(), "torn write: empty ID");
            assert!(seen.insert(intent.id.clone()), "duplicate ID {}", intent.id);
            assert_eq!(intent.originals.len(), 1, "interleaved record");
            for orig in intent.originals.values() {
                assert_eq!(orig, GO_SRC.as_bytes(), "torn payload");
            }
        }
    }

    #[test]
    fn concurrent_same_file_disjoint_all_apply() {
        // N threads each prepare+commit a DISJOINT region edit to one shared
        // file. Every edit must land, none lost: resolve-under-lock
        // re-grounds each benignly-shifted region against the current bytes
        // so concurrent commits compose losslessly (SPEC §8.3, ADR-0003).
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let specs = [
            ("func a() {}", "func a() { return }"),
            ("func b() {}", "func b() { panic(1) }"),
            ("func c() {}", "func c() { _ = 0 }"),
        ];

        std::thread::scope(|scope| {
            for (from, to) in specs {
                let s = &s;
                let path = &path;
                scope.spawn(move || {
                    let e = region_edit(path, GO_SRC, from, to);
                    let plan = s.prepare(&[e], &[]).unwrap();
                    s.commit(&plan).unwrap();
                });
            }
        });

        let final_src = read_str(&path);
        for (_, to) in specs {
            assert!(final_src.contains(to), "edit {to:?} lost:\n{final_src}");
        }
    }

    #[test]
    fn concurrent_same_region_conflict_rejects() {
        // Two threads target the SAME region. Whichever commits first wins;
        // the loser must get a conflict when it re-resolves under the lock,
        // and the file holds exactly one of the two edits — never a blend.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "main.go", GO_SRC);
        let s = new_session(wal_dir.path());

        let p1 = s
            .prepare(
                &[region_edit(
                    &path,
                    GO_SRC,
                    "func a() {}",
                    "func a() { return }",
                )],
                &[],
            )
            .unwrap();
        let p2 = s
            .prepare(
                &[region_edit(
                    &path,
                    GO_SRC,
                    "func a() {}",
                    "func a() { panic(1) }",
                )],
                &[],
            )
            .unwrap();

        let (r1, r2) = std::thread::scope(|scope| {
            let h1 = scope.spawn(|| s.commit(&p1));
            let h2 = scope.spawn(|| s.commit(&p2));
            (h1.join().unwrap(), h2.join().unwrap())
        });

        let mut ok = 0;
        let mut conflict = 0;
        for r in [&r1, &r2] {
            match r {
                Ok(_) => ok += 1,
                Err(e) if e.kind() == Kind::Conflict => conflict += 1,
                Err(e) => panic!("unexpected commit error: {e}"),
            }
        }
        assert_eq!((ok, conflict), (1, 1));

        let final_src = read_str(&path);
        let has_return = final_src.contains("func a() { return }");
        let has_panic = final_src.contains("func a() { panic(1) }");
        assert_ne!(has_return, has_panic, "corrupted blend:\n{final_src}");
    }

    #[test]
    fn cross_file_parallel_commit() {
        // DIFFERENT files edited concurrently; all must commit with no false
        // serialization or deadlock (different per-file locks, SPEC §8.3).
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        const N: usize = 6;
        let paths: Vec<String> = (0..N)
            .map(|i| write_file(dir.path(), &format!("f{i}.go"), GO_SRC))
            .collect();
        let s = new_session(wal_dir.path());

        std::thread::scope(|scope| {
            for p in &paths {
                let s = &s;
                scope.spawn(move || {
                    let e = region_edit(p, GO_SRC, "func a() {}", "func a() { return }");
                    let plan = s.prepare(&[e], &[]).unwrap();
                    s.commit(&plan).unwrap();
                });
            }
        });

        for p in &paths {
            assert!(
                read_str(p).contains("func a() { return }"),
                "{p} not committed"
            );
        }
    }

    #[test]
    fn concurrent_crossing_moves_deadlock_free() {
        // A move A→B racing a move B→A on ONE session takes the same pair
        // of per-file locks; sorted acquisition (lock_arcs) is the
        // deadlock-free invariant. Both destinations exist, so both moves
        // hard-reject with Exists — the assertion is that BOTH calls RETURN
        // (no deadlock) and no bytes are lost. Repeat to shake interleavings.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.go", GO_SRC);
        let b = write_file(dir.path(), "b.go", "package b\n");
        let s = new_session(wal_dir.path());

        for _ in 0..8 {
            let (r1, r2) = std::thread::scope(|scope| {
                let h1 = scope.spawn(|| s.move_file(&a, &b, &raw_of(GO_SRC)));
                let h2 = scope.spawn(|| s.move_file(&b, &a, &raw_of("package b\n")));
                (h1.join().unwrap(), h2.join().unwrap())
            });
            assert_eq!(r1.unwrap_err().kind(), Kind::Exists);
            assert_eq!(r2.unwrap_err().kind(), Kind::Exists);
            assert_eq!(read_str(&a), GO_SRC);
            assert_eq!(read_str(&b), "package b\n");
        }
    }
}
