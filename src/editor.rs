//! The public behavior facade: a configured [`Editor`] wrapping the
//! region-anchored session engine, the shared tree-sitter parser, and
//! (lazily, per rename) a language-server client. In Go this lived in
//! `pkg/bage` re-exporting `internal/*` types as aliases; in Rust the crate's
//! modules are public, so this module only adds the behavior layer.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::clipboard;
use crate::edit::FileEdit;
use crate::format::{Formatter, Linter};
use crate::hashing::{self, Hasher, XxHasher};
use crate::inspect::{self, ReadOptions, ReadResult};
use crate::lsp;
use crate::parser::{Adapter, Lang, ParserPort};
use crate::region::{Edit, EditResult, FileAnchor, LineIndex, Region};
use crate::session::{BatchResult, DeleteResult, MoveResult, Op, Plan, Session, SessionError};

/// Configures an [`Editor`]. `wal_dir` is required; the hasher defaults to
/// [`XxHasher`]. `formatter` and `linter` are optional pipeline steps run
/// over the staged bytes. `lsp_command` names the language-server command
/// (argv) used by rename; it may be empty when rename is not needed. `lang`
/// is optional: when `None` each file's language is auto-detected from its
/// path via [`Lang::for_path`], so an agent IDE can open a mixed-language
/// tree; when set it forces that language for every file.
#[derive(Default)]
pub struct Config {
    /// Optional per-editor language override.
    pub lang: Option<Lang>,
    /// Computes region/file digests; defaults to [`XxHasher`] when `None`.
    pub hasher: Option<Box<dyn Hasher>>,
    /// When set, rewrites staged bytes before linting/parsing.
    pub formatter: Option<Box<dyn Formatter>>,
    /// When set, blocks the edit on a lint failure.
    pub linter: Option<Box<dyn Linter>>,
    /// The directory holding the write-ahead log; required.
    pub wal_dir: PathBuf,
    /// The language-server command (argv) used by rename; optional.
    pub lsp_command: Vec<String>,
}

/// An editor failure: either a session-engine reject (carrying its machine
/// kind) or a rename-pipeline failure.
#[derive(Debug, thiserror::Error)]
pub enum EditorError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("bage: rename: {0}")]
    Lsp(#[from] lsp::LspError),
    #[error("{0}")]
    Usage(String),
    #[error("bage: {context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Inspect(#[from] inspect::InspectError),
    #[error(transparent)]
    Clipboard(#[from] clipboard::ClipboardError),
}

/// The configured FILE-LEG edit engine: the public handle consumers drive
/// (SPEC §6 standalone mode). Data types live in their own modules; this is
/// the behavior surface.
pub struct Editor {
    sess: Session,
    wal_dir: PathBuf,
    lsp_command: Vec<String>,
    /// Persistent server pool: renames reuse one warm server per (root,
    /// language) for the editor's lifetime; every child is reaped on drop.
    lsp_pool: lsp::LspPool,
}

impl Editor {
    /// Validates `cfg` and wires an editor: a tree-sitter parser as the
    /// parsing port and a [`Session`] over the configured WAL dir, hasher,
    /// language, formatter, and linter.
    pub fn open(cfg: Config) -> Result<Editor, EditorError> {
        if cfg.wal_dir.as_os_str().is_empty() {
            return Err(EditorError::Usage(
                "bage: Config.wal_dir is required".to_string(),
            ));
        }
        let mut sess = Session::new(
            Box::new(Adapter::new()),
            cfg.hasher.unwrap_or_else(|| Box::new(XxHasher)),
            cfg.wal_dir.clone(),
        );
        sess.formatter = cfg.formatter;
        sess.linter = cfg.linter;
        sess.lang = cfg.lang;
        Ok(Editor {
            sess,
            wal_dir: cfg.wal_dir,
            lsp_pool: lsp::LspPool::new(cfg.lsp_command.clone()),
            lsp_command: cfg.lsp_command,
        })
    }

    /// The editor's shared parsing port, so a host can reuse the exact
    /// parser Båge edits with, keeping graph ingest and file edits
    /// structurally consistent.
    pub fn parser(&self) -> &dyn ParserPort {
        &*self.sess.parser
    }

    /// The editor's content hasher.
    pub fn hasher(&self) -> &dyn Hasher {
        &*self.sess.hasher
    }

    /// Optimistically stages every region-anchored edit against the live
    /// files, drift-checks via the per-region region_hash, runs the optional
    /// formatter/linter, reparses to prove the result is valid, and durably
    /// records a WAL intent. The returned plan's staged edits are not yet on
    /// disk — prepare's sole on-disk effect is the WAL record.
    pub fn prepare(&self, edits: &[Edit], anchors: &[FileAnchor]) -> Result<Plan, EditorError> {
        Ok(self.sess.prepare(edits, anchors)?)
    }

    /// The atomic, lossless point: per file, under that file's lock, commit
    /// re-reads the live bytes and re-resolves every edit (a benign
    /// concurrent shift lands at the current offset; a same-region conflict
    /// rejects), atomic-writes, and returns one [`EditResult`] per edit.
    pub fn commit(&self, plan: &Plan) -> Result<Vec<EditResult>, EditorError> {
        Ok(self.sess.commit(plan)?)
    }

    /// Abandons a prepared plan, discarding the staged edits and clearing
    /// the WAL; the source files are left untouched.
    pub fn rollback(&self, plan: &mut Plan) -> Result<(), EditorError> {
        Ok(self.sess.rollback(plan)?)
    }

    /// The crash path: replays any WAL intent left in the editor's WAL dir,
    /// restoring affected files to their pre-prepare state, then clears the
    /// WAL. A clean commit leaves nothing to replay, so recover is then a
    /// no-op.
    pub fn recover(&self) -> Result<(), EditorError> {
        Ok(self.sess.recover(&self.wal_dir)?)
    }

    /// The standalone convenience for a one-shot edit: prepare then commit.
    pub fn apply(
        &self,
        edits: &[Edit],
        anchors: &[FileAnchor],
    ) -> Result<Vec<EditResult>, EditorError> {
        let plan = self.sess.prepare(edits, anchors)?;
        Ok(self.sess.commit(&plan)?)
    }

    /// Brings a new file into being. Its anchor is NON-EXISTENCE: an
    /// existing path hard-rejects and is never clobbered. The staged content
    /// clears the same format/lint/parse floor edits clear, and the create
    /// is WAL-logged so a crash unlinks the half-created file.
    pub fn create(
        &self,
        path: &str,
        content: &str,
        lang: Option<Lang>,
    ) -> Result<EditResult, EditorError> {
        Ok(self.sess.create_file(path, content, lang)?)
    }

    /// Unlinks a file, gated by the expected raw_hash drift anchor; the full
    /// prior bytes are WAL-captured before the unlink so a crash restores
    /// them.
    pub fn delete(&self, path: &str, expected_raw_hash: &str) -> Result<DeleteResult, EditorError> {
        Ok(self.sess.delete_file(path, expected_raw_hash)?)
    }

    /// Relocates a file, preserving the source bytes unchanged: the source
    /// is gated by `expected_raw_hash` and the destination by non-existence.
    pub fn move_file(
        &self,
        from: &str,
        to: &str,
        expected_raw_hash: &str,
    ) -> Result<MoveResult, EditorError> {
        Ok(self.sess.move_file(from, to, expected_raw_hash)?)
    }

    /// Applies a heterogeneous op list (edit + create + delete + move) as
    /// ONE all-or-nothing change, returning one result per op in input
    /// order. If ANY op fails, the entire batch is rejected and the
    /// filesystem is left exactly as before.
    pub fn apply_batch(&self, ops: &[Op]) -> Result<Vec<BatchResult>, EditorError> {
        Ok(self.sess.apply_batch(ops)?)
    }

    /// Opens `path` with the shared parser, lists its blocks, and returns a
    /// [`ReadResult`] carrying the detected language and the whole-file raw
    /// and normalized hashes.
    pub fn read(&self, path: &str, opts: &ReadOptions) -> Result<ReadResult, EditorError> {
        Ok(inspect::read_file(path, opts, self.hasher())?)
    }

    /// Performs an LSP-driven rename of the symbol at the zero-based
    /// `(line, col)` UTF-16 position in `file`, then stages the resulting
    /// cross-file edits as region-anchored edits and prepares them. It
    /// requires `Config.lsp_command`: it spawns the language server,
    /// requests the rename, converts the server's `WorkspaceEdit` into
    /// byte-range edits, grounds each as a region with a content
    /// region_hash, builds one file anchor per file, and prepares them. The
    /// caller commits (or rolls back) the returned plan. The language server
    /// is kept warm in the editor's [`lsp::LspPool`] (keyed by the file's
    /// root+language) for reuse by later renames, and reaped when the editor
    /// drops (`LspPool`'s `Drop` shuts every server down). There is NO
    /// background idle reaper: the pool exposes `evict_idle` for a caller-driven
    /// maintenance pass, but `Editor` never schedules it, so a warm server
    /// lives until the pool drops or its slot is LRU-evicted by a new key.
    pub fn rename(
        &self,
        file: &str,
        line: u32,
        col: u32,
        new_name: &str,
    ) -> Result<Plan, EditorError> {
        if self.lsp_command.is_empty() {
            return Err(EditorError::Usage(
                "bage: rename requires Config.lsp_command".to_string(),
            ));
        }

        let abs = std::fs::canonicalize(file).map_err(|e| EditorError::Io {
            context: format!("rename resolve {file:?}"),
            source: e,
        })?;
        let abs_str = abs.to_string_lossy().into_owned();
        let content = std::fs::read_to_string(&abs).map_err(|e| EditorError::Io {
            context: format!("rename read {abs_str:?}"),
            source: e,
        })?;

        let we = self.lsp_pool.with_client_for_file(&abs, |client| {
            client.rename(&abs_str, &content, line, col, new_name)
        })?;

        let file_edits = lsp::workspace_edit_to_file_edits(&we, |p| std::fs::read(p))?;
        if file_edits.is_empty() {
            return Err(EditorError::Usage(
                "bage: rename: server returned no edits".to_string(),
            ));
        }

        let (edits, anchors) = ground_edits(&file_edits, self.hasher())?;
        Ok(self.sess.prepare(&edits, &anchors)?)
    }

    /// Extracts a region's bytes READ-ONLY: resolves `target` against the
    /// live file (symbol / line / byte / region_hash addressing; a hash
    /// verifies-and-relocates so a stale offset never copies the wrong
    /// bytes) and returns the content plus its locator bundle. When `clip`
    /// is set the content is also written to the file clipboard.
    pub fn copy(
        &self,
        path: &str,
        target: &inspect::CopyTarget,
        clip: bool,
    ) -> Result<CopyResult, EditorError> {
        let (opened, result) = self.extract(path, target)?;
        drop(opened);
        if clip {
            clipboard::write(&clipboard::Clip {
                content: result.content.clone(),
                source_path: path.to_string(),
                region_hash: result.region_hash.clone(),
                cut: false,
            })?;
        }
        Ok(result)
    }

    /// Atomically REMOVES a region: resolves `target` like [`Editor::copy`],
    /// captures the content, then commits a hash-gated empty-text edit
    /// through the session engine (per-file lock, WAL, reject-never-corrupt
    /// — a concurrent change of the region rejects with a conflict). When
    /// `clip` is set the content is written to the file clipboard BEFORE the
    /// removal commits, so a crash can never lose the bytes to the clipboard
    /// AND the file at once.
    pub fn cut(
        &self,
        path: &str,
        target: &inspect::CopyTarget,
        clip: bool,
    ) -> Result<CutResult, EditorError> {
        let (opened, removed) = self.extract(path, target)?;
        let live = &opened.tree.source;

        if clip {
            clipboard::write(&clipboard::Clip {
                content: removed.content.clone(),
                source_path: path.to_string(),
                region_hash: removed.region_hash.clone(),
                cut: true,
            })?;
        }

        let li = LineIndex::new(live);
        let reg = li.fill_line_cols(Region {
            path: path.to_string(),
            start_byte: removed.start_byte as i64,
            end_byte: removed.end_byte as i64,
            region_hash: removed.region_hash.clone(),
            ..Default::default()
        });
        let anchors = [crate::region::file_anchor(self.hasher(), path, live)];
        drop(opened);
        let edits = [Edit {
            region: reg,
            new_text: String::new(),
        }];
        let plan = self.sess.prepare(&edits, &anchors)?;
        let mut results = self.sess.commit(&plan)?;
        let result = results.pop().ok_or_else(|| {
            EditorError::Usage("bage: cut: commit returned no result".to_string())
        })?;
        Ok(CutResult { removed, result })
    }

    /// INSERTS `text` at a point: `--at-byte` positions verbatim; the line
    /// and append modes normalize newlines exactly like `bage apply`'s
    /// insertion flags (append starts on a fresh line when the file lacks a
    /// trailing newline; line insertions gain a trailing newline if missing
    /// so line structure is preserved). The per-file anchor gates drift.
    pub fn paste(
        &self,
        path: &str,
        point: PastePoint,
        text: &str,
    ) -> Result<Vec<EditResult>, EditorError> {
        let live = std::fs::read(path).map_err(|e| EditorError::Io {
            context: format!("paste read {path:?}"),
            source: e,
        })?;

        let mut new_text = text.to_string();
        let mut reg = match point {
            PastePoint::AtByte(n) => {
                if n > live.len() {
                    return Err(EditorError::Usage(format!(
                        "bage: paste --at-byte {n} is past end of file (len {})",
                        live.len()
                    )));
                }
                let li = LineIndex::new(&live);
                li.fill_line_cols(Region {
                    start_byte: n as i64,
                    end_byte: n as i64,
                    ..Default::default()
                })
            }
            PastePoint::Point(p) => {
                match p {
                    inspect::InsertionPoint::Append => {
                        if !live.is_empty() && live.last() != Some(&b'\n') {
                            new_text.insert(0, '\n');
                        }
                    }
                    inspect::InsertionPoint::BeforeLine(_)
                    | inspect::InsertionPoint::AfterLine(_) => {
                        if !new_text.ends_with('\n') {
                            new_text.push('\n');
                        }
                    }
                }
                inspect::resolve_insertion(&live, p)
                    .map_err(|m| EditorError::Usage(m.replacen("resolve:", "bage paste:", 1)))?
            }
        };
        reg.path = path.to_string();

        let anchors = [crate::region::file_anchor(self.hasher(), path, &live)];
        let edits = [Edit {
            region: reg,
            new_text,
        }];
        let plan = self.sess.prepare(&edits, &anchors)?;
        Ok(self.sess.commit(&plan)?)
    }

    /// Shared copy/cut extraction: opens + parses the live file, resolves
    /// the target range, and builds the [`CopyResult`] locator bundle from
    /// the live bytes.
    fn extract(
        &self,
        path: &str,
        target: &inspect::CopyTarget,
    ) -> Result<(inspect::OpenedFile, CopyResult), EditorError> {
        let opened = inspect::open_file(path)?;
        let (s, e) = inspect::resolve_copy_range(self.parser(), &opened, target)?;
        let live = &opened.tree.source;
        let li = LineIndex::new(live);
        let (start_line, _) = li.position_for_byte(s);
        let (end_line, _) = li.position_for_byte(e);
        let result = CopyResult {
            path: path.to_string(),
            start_byte: s,
            end_byte: e,
            start_line,
            end_line,
            region_hash: crate::region::hash_region(live, s, e),
            content: String::from_utf8_lossy(&live[s..e]).into_owned(),
        };
        Ok((opened, result))
    }
}

/// Where [`Editor::paste`] inserts: a raw byte position (verbatim, no
/// newline normalization) or a first-class insertion point (append /
/// before-line / after-line, with `bage apply`-identical newline rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastePoint {
    /// Insert at this byte offset, verbatim.
    AtByte(usize),
    /// Insert at a line-oriented point with newline normalization.
    Point(inspect::InsertionPoint),
}

/// The outcome of a [`Editor::copy`] (and the `removed` half of a cut): the
/// extracted content plus the locator bundle identifying exactly which
/// bytes were taken. Text rendering is the bare content, so
/// `bage copy ... --format text` pipes cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyResult {
    /// The file the content came from.
    pub path: String,
    /// Inclusive start byte of the copied region.
    pub start_byte: usize,
    /// Exclusive end byte of the copied region.
    pub end_byte: usize,
    /// 1-based start line of the copied region.
    pub start_line: usize,
    /// 1-based end line of the copied region.
    pub end_line: usize,
    /// The region_hash of the copied bytes at capture time.
    pub region_hash: String,
    /// The copied bytes (lossy UTF-8, matching block content semantics).
    pub content: String,
}

impl crate::render::TextRender for CopyResult {
    fn render_text(&self, w: &mut dyn std::io::Write) -> std::io::Result<()> {
        w.write_all(self.content.as_bytes())
    }
}

/// The outcome of a [`Editor::cut`]: what was removed plus the standard
/// write-back [`EditResult`] for the mutation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CutResult {
    /// The removed region's locator bundle and content.
    pub removed: CopyResult,
    /// The write-back contract for the removal edit.
    pub result: EditResult,
}

impl crate::render::TextRender for CutResult {
    fn render_text(&self, w: &mut dyn std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "cut {} bytes [{}:{}] lines [{}:{}] region={} raw={} norm={}",
            self.result.path,
            self.removed.start_byte,
            self.removed.end_byte,
            self.removed.start_line,
            self.removed.end_line,
            self.removed.region_hash,
            self.result.new_file_raw_hash,
            self.result.new_file_norm_hash
        )?;
        w.write_all(self.removed.content.as_bytes())
    }
}

/// Converts a flat slice of byte-range [`FileEdit`]s (from an LSP
/// `WorkspaceEdit`) into region-anchored edits plus one [`FileAnchor`] per
/// file. Each file edit becomes a region whose byte range carries a
/// region_hash computed from that file's live bytes, so resolve verifies
/// content (exact in place, benign shift re-resolves, real conflict
/// rejects). Files are read once each and the per-file anchor is built from
/// those live bytes. Edits are returned in a deterministic (path, then
/// start-byte) order.
pub fn ground_edits(
    file_edits: &[FileEdit],
    hasher: &dyn Hasher,
) -> Result<(Vec<Edit>, Vec<FileAnchor>), EditorError> {
    let mut by_path: BTreeMap<&str, Vec<&FileEdit>> = BTreeMap::new();
    for fe in file_edits {
        by_path.entry(&fe.path).or_default().push(fe);
    }

    let mut edits = Vec::new();
    let mut anchors = Vec::with_capacity(by_path.len());
    for (path, mut group) in by_path {
        let live = std::fs::read(path).map_err(|e| EditorError::Io {
            context: format!("read {path:?}"),
            source: e,
        })?;
        let li = LineIndex::new(&live);

        group.sort_by_key(|fe| fe.start_byte);
        for fe in group {
            if fe.end_byte < fe.start_byte || fe.end_byte > live.len() {
                return Err(EditorError::Usage(format!(
                    "bage: edit byte range [{}:{}] out of bounds for {path:?} (len {})",
                    fe.start_byte,
                    fe.end_byte,
                    live.len()
                )));
            }
            let reg = li.fill_line_cols(Region {
                path: path.to_string(),
                start_byte: fe.start_byte as i64,
                end_byte: fe.end_byte as i64,
                region_hash: crate::region::hash_region(&live, fe.start_byte, fe.end_byte),
                ..Default::default()
            });
            edits.push(Edit {
                region: reg,
                new_text: fe.new_text.clone(),
            });
        }
        anchors.push(crate::region::file_anchor(hasher, path, &live));
    }
    Ok((edits, anchors))
}

/// Maps an editor failure to the machine-branchable error envelope shared
/// with the Go CLI: session errors carry their own kind; LSP and rename
/// pipeline failures classify as io/usage.
pub fn envelope(err: &EditorError) -> crate::session::ErrorEnvelope {
    use crate::session::{ErrorEnvelope, Kind};
    match err {
        EditorError::Session(e) => crate::session::envelope(e),
        EditorError::Lsp(e) => ErrorEnvelope {
            kind: Kind::Io,
            path: None,
            message: e.to_string(),
        },
        EditorError::Usage(m) => ErrorEnvelope {
            kind: Kind::Usage,
            path: None,
            message: m.clone(),
        },
        EditorError::Io { .. } => ErrorEnvelope {
            kind: Kind::Io,
            path: None,
            message: err.to_string(),
        },
        EditorError::Clipboard(e) => ErrorEnvelope {
            kind: match e {
                clipboard::ClipboardError::Empty => Kind::Usage,
                clipboard::ClipboardError::Io { .. } => Kind::Io,
            },
            path: None,
            message: e.to_string(),
        },
        EditorError::Inspect(e) => ErrorEnvelope {
            kind: match e {
                inspect::InspectError::Usage(_) => Kind::Usage,
                inspect::InspectError::Resolve(_) => Kind::Conflict,
                inspect::InspectError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    Kind::NotFound
                }
                _ => Kind::Io,
            },
            path: None,
            message: e.to_string(),
        },
    }
}

/// The digest of the raw bytes exactly as given (gates byte-offset
/// validity), re-exported at the facade for hosts.
pub fn raw_hash(h: &dyn Hasher, raw: &[u8]) -> String {
    hashing::raw_hash(h, raw)
}

/// The digest of the normalized bytes (the content anchor), re-exported at
/// the facade for hosts.
pub fn norm_hash(h: &dyn Hasher, raw: &[u8]) -> String {
    hashing::norm_hash(h, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::hash_region;

    fn editor(dir: &tempfile::TempDir) -> Editor {
        Editor::open(Config {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn open_requires_wal_dir() {
        assert!(matches!(
            Editor::open(Config::default()),
            Err(EditorError::Usage(_))
        ));
    }

    #[test]
    fn apply_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ed = editor(&dir);
        let path = dir.path().join("m.go");
        let path_str = path.to_string_lossy().into_owned();
        let src = b"package main\n\nfunc f() {}\n";
        std::fs::write(&path, src).unwrap();

        let li = LineIndex::new(src);
        let reg = li.fill_line_cols(Region {
            path: path_str.clone(),
            start_byte: 14,
            end_byte: 25,
            region_hash: hash_region(src, 14, 25),
            ..Default::default()
        });
        let edits = [Edit {
            region: reg,
            new_text: "func g() {}".to_string(),
        }];
        let anchors = [crate::region::file_anchor(ed.hasher(), &path_str, src)];

        let results = ed.apply(&edits, &anchors).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"package main\n\nfunc g() {}\n"
        );
    }

    #[test]
    fn create_read_delete_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let ed = editor(&dir);
        let path = dir.path().join("t.txt");
        let path_str = path.to_string_lossy().into_owned();

        let created = ed.create(&path_str, "one\ntwo\n", None).unwrap();
        assert_eq!(created.path, path_str);

        let read = ed.read(&path_str, &ReadOptions::default()).unwrap();
        assert_eq!(read.lang, "text");
        assert_eq!(read.blocks.len(), 2);
        assert_eq!(read.raw_hash, created.new_file_raw_hash);

        let deleted = ed.delete(&path_str, &created.new_file_raw_hash).unwrap();
        assert_eq!(deleted.raw_hash, created.new_file_raw_hash);
        assert!(!path.exists());
    }

    #[test]
    fn copy_is_read_only_and_matches_read_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let ed = editor(&dir);
        let p = dir.path().join("m.go");
        let p = p.to_string_lossy().into_owned();
        let src = b"package main\n\nfunc f() {}\n\nfunc g() {}\n";
        std::fs::write(&p, src).unwrap();

        let by_symbol = ed
            .copy(
                &p,
                &crate::inspect::CopyTarget {
                    line: -1,
                    start: -1,
                    end: -1,
                    symbol: "g".into(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(by_symbol.content, "func g() {}");
        assert_eq!(
            by_symbol.region_hash,
            crate::region::hash_region(src, by_symbol.start_byte, by_symbol.end_byte)
        );
        // File untouched.
        assert_eq!(std::fs::read(&p).unwrap(), src);

        // Bare region_hash addressing relocates purely by content.
        let by_hash = ed
            .copy(
                &p,
                &crate::inspect::CopyTarget {
                    line: -1,
                    start: -1,
                    end: -1,
                    region_hash: by_symbol.region_hash.clone(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(by_hash.content, by_symbol.content);

        // A wrong hash combined with a range is a conflict, not a misread.
        let err = ed
            .copy(
                &p,
                &crate::inspect::CopyTarget {
                    line: -1,
                    start: 0,
                    end: 5,
                    region_hash: "0".repeat(16),
                    ..Default::default()
                },
                false,
            )
            .unwrap_err();
        assert_eq!(envelope(&err).kind, crate::session::Kind::Conflict);
    }

    #[test]
    fn cut_removes_exactly_the_region_and_rejects_on_drift() {
        let dir = tempfile::tempdir().unwrap();
        let ed = editor(&dir);
        let p = dir.path().join("t.txt");
        let p = p.to_string_lossy().into_owned();
        std::fs::write(&p, "one\ntwo\nthree\n").unwrap();

        let cut = ed
            .cut(
                &p,
                &crate::inspect::CopyTarget {
                    line: 2,
                    start: -1,
                    end: -1,
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(cut.removed.content, "two");
        assert_eq!(std::fs::read(&p).unwrap(), b"one\n\nthree\n");

        // Symbol ambiguity (duplicate lines in text mode) rejects as usage.
        std::fs::write(&p, "dup\nmid\ndup\n").unwrap();
        let dup_hash = crate::region::hash_region(b"dup\nmid\ndup\n", 0, 3);
        let err = ed
            .cut(
                &p,
                &crate::inspect::CopyTarget {
                    line: -1,
                    start: -1,
                    end: -1,
                    region_hash: dup_hash,
                    ..Default::default()
                },
                false,
            )
            .unwrap_err();
        assert_eq!(envelope(&err).kind, crate::session::Kind::Conflict);
        assert_eq!(std::fs::read(&p).unwrap(), b"dup\nmid\ndup\n");
    }

    #[test]
    fn cut_then_paste_reproduces_bytes_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let ed = editor(&dir);
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        let a = a.to_string_lossy().into_owned();
        let b = b.to_string_lossy().into_owned();
        std::fs::write(&a, "fn keep() {}\n\nfn moved() { let x = 1; }\n").unwrap();
        std::fs::write(&b, "fn existing() {}\n").unwrap();

        let cut = ed
            .cut(
                &a,
                &crate::inspect::CopyTarget {
                    line: -1,
                    start: -1,
                    end: -1,
                    symbol: "moved".into(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        let results = ed
            .paste(
                &b,
                PastePoint::Point(crate::inspect::InsertionPoint::Append),
                &cut.removed.content,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&b).unwrap(),
            "fn existing() {}\nfn moved() { let x = 1; }"
        );
        assert!(!std::fs::read_to_string(&a).unwrap().contains("moved"));
    }

    #[test]
    fn paste_points_normalize_newlines_like_apply() {
        let dir = tempfile::tempdir().unwrap();
        let ed = editor(&dir);
        let p = dir.path().join("t.txt");
        let p = p.to_string_lossy().into_owned();

        // Append onto a newline-less file starts on a fresh line.
        std::fs::write(&p, "tail-no-newline").unwrap();
        ed.paste(
            &p,
            PastePoint::Point(crate::inspect::InsertionPoint::Append),
            "added",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "tail-no-newline\nadded"
        );

        // before-line gains a trailing newline so the target line survives.
        std::fs::write(&p, "one\ntwo\n").unwrap();
        ed.paste(
            &p,
            PastePoint::Point(crate::inspect::InsertionPoint::BeforeLine(2)),
            "mid",
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\nmid\ntwo\n");

        // at-byte is verbatim; past-EOF rejects as usage.
        std::fs::write(&p, "ab").unwrap();
        ed.paste(&p, PastePoint::AtByte(1), "X").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "aXb");
        let err = ed.paste(&p, PastePoint::AtByte(99), "X").unwrap_err();
        assert_eq!(envelope(&err).kind, crate::session::Kind::Usage);
    }
    #[test]
    fn ground_edits_sorts_and_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"hello world\n").unwrap();
        let p = p.to_string_lossy().into_owned();
        let fes = [
            FileEdit {
                path: p.clone(),
                start_byte: 6,
                end_byte: 11,
                new_text: "there".into(),
            },
            FileEdit {
                path: p.clone(),
                start_byte: 0,
                end_byte: 5,
                new_text: "howdy".into(),
            },
        ];
        let (edits, anchors) = ground_edits(&fes, &XxHasher).unwrap();
        assert_eq!(edits.len(), 2);
        assert!(edits[0].region.start_byte < edits[1].region.start_byte);
        assert_eq!(anchors.len(), 1);
        assert_eq!(edits[0].region.region_hash.len(), 16);
        // Out-of-bounds rejects.
        let bad = [FileEdit {
            path: p,
            start_byte: 0,
            end_byte: 999,
            new_text: String::new(),
        }];
        assert!(matches!(
            ground_edits(&bad, &XxHasher),
            Err(EditorError::Usage(_))
        ));
    }
}
