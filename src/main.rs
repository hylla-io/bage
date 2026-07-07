//! Command bage — the standalone entrypoint for the Båge round-trip file
//! editor (SPEC §6 standalone mode): files + LSP, no graph, sharing the same
//! region/session edit engine as integrated mode. This is the Rust CLI,
//! flag-compatible with the Go `cmd/bage`.

use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use bage::editor::{self, Config, Editor};
use bage::hashing::{self, XxHasher};
use bage::inspect::{self, Block, ParseDefect, ReadOptions, ReadResult};
use bage::lsp;
use bage::parser::Lang;
use bage::region::{EditResult, Region};
use bage::render::{Format, TextRender, emit};
use bage::session::{ErrorEnvelope, Session, envelope};

/// bage — round-trip file editor (standalone).
#[derive(Parser)]
#[command(name = "bage", version, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Replace a region of a file, anchored by line/byte range and an
    /// optional region_hash (benign shifts re-resolve; conflicts reject).
    Apply(ApplyArgs),
    /// Write a NEW file; the anchor is non-existence (never clobbers).
    Create(CreateArgs),
    /// Unlink an existing file, gated by its expected raw_hash.
    Delete(DeleteArgs),
    /// Relocate a file, preserving its bytes (source raw_hash-gated,
    /// destination non-existence-gated).
    Move(MoveArgs),
    /// LSP-driven symbol rename applied atomically across files.
    Rename(RenameArgs),
    /// Structured READ view: language, drift hashes, addressable blocks.
    Read(ReadArgs),
    /// The outline view: every addressable block with its region_hash.
    Show(ShowArgs),
    /// Surface parse-health defects and (optionally) LSP diagnostics.
    Diagnose(DiagnoseArgs),
    /// Extract a region READ-ONLY (content + locator bundle); text output
    /// is the bare content, so it pipes cleanly.
    Copy(CopyArgs),
    /// Atomically REMOVE a region (hash-gated, WAL-backed) and emit what
    /// was removed.
    Cut(CutArgs),
    /// Insert text at a byte offset, line point, or end-of-file — from
    /// --text, --text-file, or the clipboard (--clip).
    Paste(PasteArgs),
}

#[derive(Args)]
struct ApplyArgs {
    /// Path of the file to edit.
    #[arg(long)]
    file: String,
    /// 1-based line to replace (mutually exclusive with --lines / --start).
    #[arg(long, default_value_t = -1)]
    line: i64,
    /// 1-based inclusive line range L1-L2 to replace.
    #[arg(long, default_value = "")]
    lines: String,
    /// Inclusive start byte of the region to replace.
    #[arg(long, default_value_t = -1)]
    start: i64,
    /// Exclusive end byte of the region to replace.
    #[arg(long, default_value_t = -1)]
    end: i64,
    /// Replace the ENTIRE file [0, len). Mutually exclusive with
    /// --line/--lines/--start/--end; carries no region_hash (the per-file
    /// anchor still gates drift) and never strips a trailing newline.
    #[arg(long, default_value_t = false)]
    all: bool,
    /// Insert --text at end-of-file. If the file does not end with a
    /// newline, one is prepended to the inserted text so the append starts
    /// on a fresh line. Mutually exclusive with the other addressing modes;
    /// carries no region_hash (the per-file anchor gates drift) and never
    /// strips a trailing newline from --text.
    #[arg(long, default_value_t = false)]
    append: bool,
    /// Insert --text at the start of this 1-based line. The inserted text
    /// gets a trailing newline appended if missing (never stripped), so
    /// existing line structure is preserved. Mutually exclusive with the
    /// other addressing modes; carries no region_hash (the per-file anchor
    /// gates drift).
    #[arg(long, default_value_t = -1)]
    before_line: i64,
    /// Insert --text just after this 1-based line's newline; a line at or
    /// past EOF clamps to end-of-file. The inserted text gets a trailing
    /// newline appended if missing (never stripped), so existing line
    /// structure is preserved. Mutually exclusive with the other addressing
    /// modes; carries no region_hash (the per-file anchor gates drift).
    #[arg(long, default_value_t = -1)]
    after_line: i64,
    /// Replacement text for the region.
    #[arg(long, default_value = "")]
    text: String,
    /// Read replacement text from this file instead of --text.
    #[arg(long, default_value = "")]
    text_file: String,
    /// Optional region_hash anchoring the region by content.
    #[arg(long, default_value = "")]
    region_hash: String,
    /// Expected raw content hash of the LIVE FILE (file-level drift gate,
    /// like delete/move): when set, a mismatch rejects before anything is
    /// staged, so an edit grounded on stale bytes can never land on a file
    /// that changed underneath it. Empty = no file-level gate (the
    /// region_hash, when given, still gates the region).
    #[arg(long, default_value = "")]
    raw_hash: String,
    /// Source language by canonical name; empty = auto-detect from --file.
    #[arg(long, default_value = "")]
    lang: String,
    /// Optional formatter command run on the staged bytes.
    #[arg(long, default_value = "")]
    fmt: String,
    /// Optional linter command run on the staged bytes.
    #[arg(long, default_value = "")]
    lint: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct CreateArgs {
    /// Path of the file to create (must not already exist).
    #[arg(long)]
    file: String,
    /// Full content of the new file.
    #[arg(long, default_value = "")]
    text: String,
    /// Read the new file's content from this file instead of --text.
    #[arg(long, default_value = "")]
    text_file: String,
    /// Source language by canonical name; empty = auto-detect from --file.
    #[arg(long, default_value = "")]
    lang: String,
    /// Optional formatter command run on the staged bytes.
    #[arg(long, default_value = "")]
    fmt: String,
    /// Optional linter command run on the staged bytes.
    #[arg(long, default_value = "")]
    lint: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct DeleteArgs {
    /// Path of the file to delete (must exist).
    #[arg(long)]
    file: String,
    /// Expected raw content hash of the live file (drift gate); empty =
    /// compute from live bytes (delete-current, no drift protection).
    #[arg(long, default_value = "")]
    raw_hash: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct MoveArgs {
    /// Source path to move (must exist).
    #[arg(long)]
    from: String,
    /// Destination path (must not already exist).
    #[arg(long)]
    to: String,
    /// Expected raw content hash of the live source (drift gate); empty =
    /// compute from live bytes (relocate-current, no drift protection).
    #[arg(long, default_value = "")]
    raw_hash: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct RenameArgs {
    /// Path of the file containing the symbol.
    #[arg(long)]
    file: String,
    /// Zero-based line of the symbol.
    #[arg(long, default_value_t = -1)]
    line: i64,
    /// Zero-based UTF-16 column of the symbol.
    #[arg(long, default_value_t = -1)]
    col: i64,
    /// New name for the symbol.
    #[arg(long, default_value = "")]
    new: String,
    /// LSP server command to drive the rename.
    #[arg(long, default_value = "gopls")]
    lsp: String,
    /// Source language (currently only 'go').
    #[arg(long, default_value = "go")]
    lang: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct ReadArgs {
    /// Path of the file to read.
    #[arg(long)]
    file: String,
    /// 1-based line of a single-line sub-range read.
    #[arg(long, default_value_t = -1)]
    line: i64,
    /// 1-based inclusive line range L1-L2 sub-range read.
    #[arg(long, default_value = "")]
    lines: String,
    /// Inclusive start byte of a byte sub-range read.
    #[arg(long, default_value_t = -1)]
    start: i64,
    /// Exclusive end byte of a byte sub-range read.
    #[arg(long, default_value_t = -1)]
    end: i64,
    /// Keep only the block whose symbol name equals this.
    #[arg(long, default_value = "")]
    symbol: String,
    /// Include each block's raw source text.
    #[arg(long, default_value_t = false)]
    content: bool,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct ShowArgs {
    /// Path of the file to show.
    #[arg(long)]
    file: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct DiagnoseArgs {
    /// Path of the file to diagnose.
    #[arg(long)]
    file: String,
    /// Optional LSP server command; when given, also collect the server's
    /// published diagnostics.
    #[arg(long, default_value = "")]
    lsp: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let result = match cli.cmd {
        Cmd::Apply(a) => run_apply(a, &mut stdout, &mut stderr),
        Cmd::Create(a) => run_create(a, &mut stdout, &mut stderr),
        Cmd::Delete(a) => run_delete(a, &mut stdout, &mut stderr),
        Cmd::Move(a) => run_move(a, &mut stdout, &mut stderr),
        Cmd::Rename(a) => run_rename(a, &mut stdout, &mut stderr),
        Cmd::Read(a) => run_read(a, &mut stdout, &mut stderr),
        Cmd::Show(a) => run_show(a, &mut stdout, &mut stderr),
        Cmd::Diagnose(a) => run_diagnose(a, &mut stdout, &mut stderr),
        Cmd::Copy(a) => run_copy(a, &mut stdout, &mut stderr),
        Cmd::Cut(a) => run_cut(a, &mut stdout, &mut stderr),
        Cmd::Paste(a) => run_paste(a, &mut stdout, &mut stderr),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

/// Parses the --format flag, printing the parse error to stderr on failure.
fn parse_format(s: &str, stderr: &mut dyn Write) -> Result<Format, ()> {
    s.parse::<Format>().map_err(|e| {
        let _ = writeln!(stderr, "{e}");
    })
}

/// Maps the --lang flag to an optional language override: empty means
/// auto-detect per file; a non-empty value must name a known language.
fn parse_lang(s: &str, stderr: &mut dyn Write) -> Result<Option<Lang>, ()> {
    if s.is_empty() {
        return Ok(None);
    }
    match Lang::from_name(s) {
        Some(l) => Ok(Some(l)),
        None => {
            let _ = writeln!(stderr, "bage: unsupported --lang {s:?}");
            Err(())
        }
    }
}

/// Emits the machine-branchable error envelope to stderr in the chosen
/// format (mirroring the Go CLI's `render.Emit(stderr, fmt, Envelope(err))`).
fn emit_envelope(stderr: &mut dyn Write, f: Format, env: &ErrorEnvelope) -> Result<(), ()> {
    let _ = emit(stderr, f, env);
    Err(())
}

/// Builds a session mirroring the Go CLI wiring: tree-sitter parser, xxHash
/// hasher, optional exec formatter/linter, WAL in the OS temp dir.
fn cli_session(lang: Option<Lang>, fmt_cmd: &str, lint_cmd: &str) -> Session {
    let mut sess = Session::new(
        Box::new(bage::parser::Adapter::new()),
        Box::new(XxHasher),
        std::env::temp_dir(),
    );
    sess.lang = lang;
    sess.formatter = split_cmd(fmt_cmd).map(|(name, args)| {
        Box::new(bage::format::CmdFormatter { name, args }) as Box<dyn bage::format::Formatter>
    });
    sess.linter = split_cmd(lint_cmd).map(|(name, args)| {
        Box::new(bage::format::CmdLinter { name, args }) as Box<dyn bage::format::Linter>
    });
    sess
}

/// Splits a command string into its executable name and arguments on runs of
/// whitespace; `None` for an empty (or whitespace-only) string so callers can
/// skip the corresponding step. Sufficient for the simple commands the CLI
/// accepts (e.g. "gofmt", "cat").
fn split_cmd(cmd: &str) -> Option<(String, Vec<String>)> {
    let mut fields = cmd.split_whitespace().map(str::to_string);
    let name = fields.next()?;
    Some((name, fields.collect()))
}

/// The renderable list of apply/create write-back results: one "applied …"
/// line per result, sorted by path then changed start offset — byte-identical
/// to the Go CLI's text output.
#[derive(Serialize)]
#[serde(transparent)]
struct EditResults(Vec<EditResult>);

impl TextRender for EditResults {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut sorted: Vec<&EditResult> = self.0.iter().collect();
        sorted.sort_by(|a, b| (&a.path, a.changed_start).cmp(&(&b.path, b.changed_start)));
        for r in sorted {
            writeln!(
                w,
                "applied {} bytes [{}:{}] lines [{}:{}] region={} raw={} norm={}",
                r.path,
                r.changed_start,
                r.changed_end,
                r.new_start_line,
                r.new_end_line,
                r.new_region_hash,
                r.new_file_raw_hash,
                r.new_file_norm_hash
            )?;
        }
        Ok(())
    }
}

fn run_apply(a: ApplyArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    let lang = parse_lang(&a.lang, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage apply: --file is required");
        return Err(());
    }

    let live = match std::fs::read(&a.file) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(stderr, "bage apply: read {:?}: {e}", a.file);
            return Err(());
        }
    };
    if !a.raw_hash.is_empty() {
        let live_hash = hashing::raw_hash(&XxHasher, &live);
        if live_hash != a.raw_hash {
            let env = ErrorEnvelope {
                kind: bage::session::Kind::Drift,
                path: Some(a.file.clone()),
                message: format!(
                    "bage apply: live file raw_hash {live_hash} does not match expected {} (file changed underneath the edit)",
                    a.raw_hash
                ),
            };
            return emit_envelope(stderr, fmt, &env);
        }
    }

    let reg = match apply_region(&a, &live) {
        Ok(r) => r,
        Err(msg) => {
            let _ = writeln!(stderr, "{msg}");
            return Err(());
        }
    };

    let mut new_text = a.text.clone();
    if !a.text_file.is_empty() {
        match std::fs::read_to_string(&a.text_file) {
            Ok(s) => new_text = s,
            Err(e) => {
                let _ = writeln!(
                    stderr,
                    "bage apply: read --text-file {:?}: {e}",
                    a.text_file
                );
                return Err(());
            }
        }
    }
    if a.line >= 0 || !a.lines.is_empty() {
        // Line addressing replaces line CONTENT — the trailing newline is
        // structural and preserved by apply_region — so a trailing newline
        // in --text would double it. Strip one so `--text "x"` and
        // `--text "x\n"` behave identically and never merge or split lines.
        if let Some(stripped) = new_text.strip_suffix('\n') {
            new_text = stripped.to_string();
        }
    }
    if a.append && !live.is_empty() && live.last() != Some(&b'\n') {
        // The append must start on a fresh line: when the file does not end
        // with a newline, prepend one to the inserted text.
        new_text.insert(0, '\n');
    }
    if (a.before_line >= 0 || a.after_line >= 0) && !new_text.ends_with('\n') {
        // Line insertion preserves line structure: ensure the inserted text
        // ends with a newline so the line it lands before stays intact.
        new_text.push('\n');
    }

    let sess = cli_session(lang, &a.fmt, &a.lint);
    let edits = [bage::region::Edit {
        region: reg,
        new_text,
    }];
    let anchors = [bage::region::file_anchor(&XxHasher, &a.file, &live)];

    let outcome = sess
        .prepare(&edits, &anchors)
        .and_then(|plan| sess.commit(&plan));
    match outcome {
        Ok(results) => {
            let _ = emit(stdout, fmt, &EditResults(results));
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &envelope(&e)),
    }
}

/// Builds the region-anchored target from the apply flags. Exactly one
/// addressing mode must be supplied: the whole file (--all), a single line
/// (--line), a 1-based inclusive line range (--lines), a raw byte range
/// (--start/--end), or an insertion point (--append / --before-line /
/// --after-line). --all spans [0, len); the insertion modes resolve to a
/// zero-width region via the shared [`inspect::resolve_insertion`]. Neither
/// carries a region_hash — the per-file anchor still gates drift. Line
/// addressing resolves to bytes via the shared [`inspect::resolve_range`]
/// (which excludes the structural trailing newline); the optional
/// region_hash is attached unchanged so the resolver can verify content and
/// relocate a benign shift.
fn apply_region(a: &ApplyArgs, live: &[u8]) -> Result<Region, String> {
    let range_mode = a.line >= 0 || !a.lines.is_empty() || a.start >= 0 || a.end >= 0;
    let insert_flags =
        usize::from(a.append) + usize::from(a.before_line >= 0) + usize::from(a.after_line >= 0);
    if insert_flags > 1 {
        return Err(
            "bage apply: choose one of --append, --before-line, or --after-line".to_string(),
        );
    }
    if insert_flags == 1 {
        if a.all || range_mode {
            return Err(
                "bage apply: --append/--before-line/--after-line are mutually \
                 exclusive with --all/--line/--lines/--start/--end"
                    .to_string(),
            );
        }
        let point = if a.append {
            inspect::InsertionPoint::Append
        } else if a.before_line >= 0 {
            inspect::InsertionPoint::BeforeLine(a.before_line)
        } else {
            inspect::InsertionPoint::AfterLine(a.after_line)
        };
        let mut reg = inspect::resolve_insertion(live, point)
            .map_err(|m| m.replacen("resolve:", "bage apply:", 1))?;
        reg.path = a.file.clone();
        return Ok(reg);
    }
    if a.all {
        if range_mode {
            return Err(
                "bage apply: --all is mutually exclusive with --line/--lines/--start/--end"
                    .to_string(),
            );
        }
        let mut reg = inspect::resolve_range(live, -1, "", 0, live.len() as i64)
            .map_err(|m| m.replacen("resolve:", "bage apply:", 1))?;
        reg.path = a.file.clone();
        return Ok(reg);
    }
    let mut reg = inspect::resolve_range(live, a.line, &a.lines, a.start, a.end)
        .map_err(|m| m.replacen("resolve:", "bage apply:", 1))?;
    reg.path = a.file.clone();
    reg.region_hash = a.region_hash.clone();
    Ok(reg)
}

fn run_create(a: CreateArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    let lang = parse_lang(&a.lang, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage create: --file is required");
        return Err(());
    }
    if !a.text.is_empty() && !a.text_file.is_empty() {
        let _ = writeln!(
            stderr,
            "bage create: choose one of --text or --text-file, not both"
        );
        return Err(());
    }

    let mut content = a.text.clone();
    if !a.text_file.is_empty() {
        match std::fs::read_to_string(&a.text_file) {
            Ok(s) => content = s,
            Err(e) => {
                let _ = writeln!(
                    stderr,
                    "bage create: read --text-file {:?}: {e}",
                    a.text_file
                );
                return Err(());
            }
        }
    }

    let sess = cli_session(lang, &a.fmt, &a.lint);
    match sess.create_file(&a.file, &content, None) {
        Ok(res) => {
            let _ = emit(stdout, fmt, &EditResults(vec![res]));
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &envelope(&e)),
    }
}

/// Resolves the expected raw_hash for delete/move: an explicit flag value is
/// the caller's drift anchor; empty means anchor-to-current, so the live
/// bytes are read and hashed (no drift protection — documented). A read
/// failure rejects before anything is touched.
fn expected_raw_hash(
    verb: &str,
    path: &str,
    flag: &str,
    stderr: &mut dyn Write,
) -> Result<String, ()> {
    if !flag.is_empty() {
        return Ok(flag.to_string());
    }
    match std::fs::read(path) {
        Ok(live) => Ok(hashing::raw_hash(&XxHasher, &live)),
        Err(e) => {
            let _ = writeln!(stderr, "bage {verb}: read {path:?}: {e}");
            Err(())
        }
    }
}

fn run_delete(a: DeleteArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage delete: --file is required");
        return Err(());
    }
    let expected = expected_raw_hash("delete", &a.file, &a.raw_hash, stderr)?;
    let sess = cli_session(None, "", "");
    match sess.delete_file(&a.file, &expected) {
        Ok(res) => {
            let _ = emit(stdout, fmt, &res);
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &envelope(&e)),
    }
}

fn run_move(a: MoveArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.from.is_empty() {
        let _ = writeln!(stderr, "bage move: --from is required");
        return Err(());
    }
    if a.to.is_empty() {
        let _ = writeln!(stderr, "bage move: --to is required");
        return Err(());
    }
    let expected = expected_raw_hash("move", &a.from, &a.raw_hash, stderr)?;
    let sess = cli_session(None, "", "");
    match sess.move_file(&a.from, &a.to, &expected) {
        Ok(res) => {
            let _ = emit(stdout, fmt, &res);
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &envelope(&e)),
    }
}

fn run_rename(a: RenameArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    let _lang = parse_lang(&a.lang, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage rename: --file is required");
        return Err(());
    }
    if a.line < 0 || a.col < 0 {
        let _ = writeln!(
            stderr,
            "bage rename: --line and --col are required and must be >= 0"
        );
        return Err(());
    }
    if a.new.is_empty() {
        let _ = writeln!(stderr, "bage rename: --new is required");
        return Err(());
    }
    let command: Vec<String> = a.lsp.split_whitespace().map(str::to_string).collect();
    if command.is_empty() {
        let _ = writeln!(stderr, "bage rename: --lsp must name a server command");
        return Err(());
    }

    let ed = match Editor::open(Config {
        lang: parse_lang(&a.lang, stderr)?,
        wal_dir: std::env::temp_dir(),
        lsp_command: command,
        ..Default::default()
    }) {
        Ok(ed) => ed,
        Err(e) => {
            let _ = writeln!(stderr, "bage rename: {e}");
            return Err(());
        }
    };

    let outcome = ed
        .rename(&a.file, a.line as u32, a.col as u32, &a.new)
        .and_then(|plan| ed.commit(&plan));
    match outcome {
        Ok(results) => {
            let _ = emit(stdout, fmt, &EditResults(results));
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &editor::envelope(&e)),
    }
}

fn run_read(a: ReadArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage read: --file is required");
        return Err(());
    }
    let opts = match read_options(&a) {
        Ok(o) => o,
        Err(msg) => {
            let _ = writeln!(stderr, "{msg}");
            return Err(());
        }
    };
    match inspect::read_file(&a.file, &opts, &XxHasher) {
        Ok(res) => {
            let _ = emit(stdout, fmt, &ReadView(res));
            Ok(())
        }
        Err(e) => {
            let env = editor::envelope(&editor::EditorError::Inspect(e));
            emit_envelope(stderr, fmt, &env)
        }
    }
}

/// Builds [`ReadOptions`] from the read flags: --line maps to `line`,
/// --lines "L1-L2" maps to `line`/`end_line`, --start/--end map to the byte
/// range, --symbol filters, --content includes raw text. A malformed --lines
/// is a usage error.
fn read_options(a: &ReadArgs) -> Result<ReadOptions, String> {
    let mut opts = ReadOptions {
        include_content: a.content,
        symbol: a.symbol.clone(),
        ..Default::default()
    };
    if a.start >= 0 {
        opts.start_byte = a.start as usize;
    }
    if a.end >= 0 {
        opts.end_byte = a.end as usize;
    }
    if a.line >= 0 {
        opts.line = a.line as usize;
    }
    if !a.lines.is_empty() {
        let (lo, hi) = a
            .lines
            .split_once('-')
            .ok_or_else(|| format!("bage read: --lines {:?} must be L1-L2", a.lines))?;
        let start: usize = lo
            .trim()
            .parse()
            .ok()
            .filter(|&n| n >= 1)
            .ok_or_else(|| format!("bage read: --lines start {lo:?} must be >= 1"))?;
        let end: usize = hi
            .trim()
            .parse()
            .ok()
            .filter(|&n| n >= 1)
            .ok_or_else(|| format!("bage read: --lines end {hi:?} must be >= 1"))?;
        opts.line = start;
        opts.end_line = end;
    }
    Ok(opts)
}

/// Newtype giving [`ReadResult`] the CLI's text rendering: a header line
/// "<path> lang=<lang> raw=<raw> norm=<norm> blocks=<N>" then one line per
/// block — byte-identical to the Go CLI.
#[derive(Serialize)]
#[serde(transparent)]
struct ReadView(ReadResult);

impl TextRender for ReadView {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        writeln!(
            w,
            "{} lang={} raw={} norm={} blocks={}",
            r.path,
            r.lang,
            r.raw_hash,
            r.norm_hash,
            r.blocks.len()
        )?;
        for b in &r.blocks {
            render_block_line(w, b)?;
            for line in b.content.lines() {
                writeln!(w, "    |{line}")?;
            }
        }
        Ok(())
    }
}

/// One outline line: "  <kind> <name> lines [sl:el] bytes [sb:eb] region=<H>"
/// with an empty name rendered as "-".
fn render_block_line(w: &mut dyn Write, b: &Block) -> io::Result<()> {
    let name = if b.name.is_empty() { "-" } else { &b.name };
    writeln!(
        w,
        "  {} {} lines [{}:{}] bytes [{}:{}] region={}",
        b.kind, name, b.start_line, b.end_line, b.start_byte, b.end_byte, b.region_hash
    )
}

/// The structured read view emitted by show: the resolved language, the
/// file-level raw/norm hashes (the per-file drift anchor), and the outline
/// of addressable blocks (no content field, matching Go's showView).
#[derive(Serialize)]
struct ShowView {
    path: String,
    lang: String,
    raw_hash: String,
    norm_hash: String,
    outline: Vec<ShowBlock>,
}

/// One addressable block in the show outline (Go's showBlock: no content).
#[derive(Serialize)]
struct ShowBlock {
    kind: String,
    name: String,
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
    region_hash: String,
}

impl TextRender for ShowView {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "{} lang={} raw={} norm={} blocks={}",
            self.path,
            self.lang,
            self.raw_hash,
            self.norm_hash,
            self.outline.len()
        )?;
        for b in &self.outline {
            let name = if b.name.is_empty() { "-" } else { &b.name };
            writeln!(
                w,
                "  {} {} lines [{}:{}] bytes [{}:{}] region={}",
                b.kind, name, b.start_line, b.end_line, b.start_byte, b.end_byte, b.region_hash
            )?;
        }
        Ok(())
    }
}

fn run_show(a: ShowArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage show: --file is required");
        return Err(());
    }
    let opened = match inspect::open_file(&a.file) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(stderr, "bage show: {e}");
            return Err(());
        }
    };
    let src = &opened.tree.source;
    let view = ShowView {
        path: a.file.clone(),
        lang: opened.lang.name().to_string(),
        raw_hash: hashing::raw_hash(&XxHasher, src),
        norm_hash: hashing::norm_hash(&XxHasher, src),
        outline: inspect::read_blocks(&opened, false)
            .into_iter()
            .map(|b| ShowBlock {
                kind: b.kind,
                name: b.name,
                start_line: b.start_line,
                end_line: b.end_line,
                start_byte: b.start_byte,
                end_byte: b.end_byte,
                region_hash: b.region_hash,
            })
            .collect(),
    };
    let _ = emit(stdout, fmt, &view);
    Ok(())
}

/// The structured read view emitted by diagnose: the file, its resolved
/// language, the always-present parse-health defects, and (only when --lsp
/// is given) the language server's diagnostics.
#[derive(Serialize)]
struct DiagnoseView {
    path: String,
    lang: String,
    parse_health: Vec<ParseDefect>,
    lsp: Vec<LspDiagnosticView>,
}

/// One LSP-reported diagnostic in the diagnose view.
#[derive(Serialize)]
struct LspDiagnosticView {
    severity: String,
    source: String,
    message: String,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
}

impl TextRender for DiagnoseView {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "{} lang={} parse_health={} lsp={}",
            self.path,
            self.lang,
            self.parse_health.len(),
            self.lsp.len()
        )?;
        for d in &self.parse_health {
            writeln!(
                w,
                "  parse {} line {} col {} bytes [{}:{}]",
                d.kind, d.line, d.col, d.start_byte, d.end_byte
            )?;
        }
        for d in &self.lsp {
            let source = if d.source.is_empty() { "-" } else { &d.source };
            writeln!(
                w,
                "  lsp {} [{}] line {} col {}: {}",
                d.severity, source, d.start_line, d.start_col, d.message
            )?;
        }
        Ok(())
    }
}

fn run_diagnose(a: DiagnoseArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage diagnose: --file is required");
        return Err(());
    }
    let opened = match inspect::open_file(&a.file) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(stderr, "bage diagnose: {e}");
            return Err(());
        }
    };
    let mut view = DiagnoseView {
        path: a.file.clone(),
        lang: opened.lang.name().to_string(),
        parse_health: inspect::parse_health(&opened),
        lsp: Vec::new(),
    };

    // The LSP tier is opt-in: an LSP-start/connect failure is a real
    // (non-zero) error since the caller explicitly asked for it, but finding
    // diagnostics is success.
    if !a.lsp.trim().is_empty() {
        match collect_lsp_diagnostics(&a.file, &a.lsp) {
            Ok(diags) => {
                view.lsp = diags
                    .into_iter()
                    .map(|d| LspDiagnosticView {
                        severity: d.severity,
                        source: d.source,
                        message: d.message,
                        start_line: d.start_line,
                        start_col: d.start_col,
                        end_line: d.end_line,
                        end_col: d.end_col,
                    })
                    .collect();
            }
            Err(msg) => {
                let _ = writeln!(stderr, "bage diagnose: {msg}");
                return Err(());
            }
        }
    }

    let _ = emit(stdout, fmt, &view);
    Ok(())
}

/// Starts the named LSP server, initializes it rooted at the file's
/// directory, opens the file, and collects the server's published
/// diagnostics. Any LSP-stage failure is returned — diagnose treats an
/// opted-in LSP that cannot start as a hard error, distinct from a server
/// that simply reports problems.
fn collect_lsp_diagnostics(file: &str, lsp_cmd: &str) -> Result<Vec<lsp::Diagnostic>, String> {
    let command: Vec<String> = lsp_cmd.split_whitespace().map(str::to_string).collect();
    if command.is_empty() {
        return Err("--lsp must name a server command".to_string());
    }
    let abs = std::fs::canonicalize(file).map_err(|e| format!("resolve {file:?}: {e}"))?;
    let abs_str = abs.to_string_lossy().into_owned();
    let content = std::fs::read_to_string(&abs).map_err(|e| format!("read {abs_str:?}: {e}"))?;

    let mut client =
        lsp::Client::new_stdio(&command).map_err(|e| format!("start lsp {:?}: {e}", command[0]))?;
    let root = abs
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let outcome = (|| {
        client
            .initialize(&lsp::file_uri(&root).to_string())
            .map_err(|e| format!("initialize: {e}"))?;
        client
            .diagnostics(&abs_str, &content, std::time::Duration::from_secs(10))
            .map_err(|e| format!("collect diagnostics: {e}"))
    })();
    let _ = client.close();
    outcome
}

#[derive(Args)]
struct CopyArgs {
    /// Path of the file to copy from.
    #[arg(long)]
    file: String,
    /// 1-based single line to copy.
    #[arg(long, default_value_t = -1)]
    line: i64,
    /// 1-based inclusive line range L1-L2 to copy.
    #[arg(long, default_value = "")]
    lines: String,
    /// Inclusive start byte of the region to copy.
    #[arg(long, default_value_t = -1)]
    start: i64,
    /// Exclusive end byte of the region to copy.
    #[arg(long, default_value_t = -1)]
    end: i64,
    /// Copy the block whose symbol name equals this (errors when zero or
    /// several blocks match — never guesses).
    #[arg(long, default_value = "")]
    symbol: String,
    /// Content anchor: alone it locates the region purely by content; with
    /// a range/symbol it verifies (and benignly relocates) the target.
    #[arg(long, default_value = "")]
    region_hash: String,
    /// Also write the content to the file clipboard
    /// ($BAGE_CLIPBOARD, default ~/.bage/clipboard.json).
    #[arg(long, default_value_t = false)]
    clip: bool,
    /// Output format: text|json|toon. Text is the BARE content (pipeable).
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct CutArgs {
    /// Path of the file to cut from.
    #[arg(long)]
    file: String,
    /// 1-based single line to cut.
    #[arg(long, default_value_t = -1)]
    line: i64,
    /// 1-based inclusive line range L1-L2 to cut.
    #[arg(long, default_value = "")]
    lines: String,
    /// Inclusive start byte of the region to cut.
    #[arg(long, default_value_t = -1)]
    start: i64,
    /// Exclusive end byte of the region to cut.
    #[arg(long, default_value_t = -1)]
    end: i64,
    /// Cut the block whose symbol name equals this (errors when zero or
    /// several blocks match — never guesses).
    #[arg(long, default_value = "")]
    symbol: String,
    /// Content anchor: alone it locates the region purely by content; with
    /// a range/symbol it verifies (and benignly relocates) the target. A
    /// mismatch rejects the cut — nothing is removed.
    #[arg(long, default_value = "")]
    region_hash: String,
    /// Also write the removed content to the file clipboard BEFORE the
    /// removal commits ($BAGE_CLIPBOARD, default ~/.bage/clipboard.json).
    #[arg(long, default_value_t = false)]
    clip: bool,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Args)]
struct PasteArgs {
    /// Path of the file to paste into.
    #[arg(long)]
    file: String,
    /// Insert at this exact byte offset, verbatim (no newline
    /// normalization).
    #[arg(long, default_value_t = -1)]
    at_byte: i64,
    /// Insert at end-of-file (fresh-line rule: a newline is prepended when
    /// the file does not end with one).
    #[arg(long, default_value_t = false)]
    append: bool,
    /// Insert at the start of this 1-based line (a trailing newline is
    /// appended to the text if missing).
    #[arg(long, default_value_t = -1)]
    before_line: i64,
    /// Insert just after this 1-based line's newline (a trailing newline is
    /// appended to the text if missing); past-EOF clamps to end-of-file.
    #[arg(long, default_value_t = -1)]
    after_line: i64,
    /// The text to paste.
    #[arg(long, default_value = "")]
    text: String,
    /// Read the text to paste from this file.
    #[arg(long, default_value = "")]
    text_file: String,
    /// Paste from the file clipboard (written by cut/copy --clip).
    #[arg(long, default_value_t = false)]
    clip: bool,
    /// Source language by canonical name; empty = auto-detect from --file.
    #[arg(long, default_value = "")]
    lang: String,
    /// Output format: text|json|toon.
    #[arg(long, default_value = "text")]
    format: String,
}

/// Shared copy/cut flag → target mapping.
fn copy_target(
    line: i64,
    lines: &str,
    start: i64,
    end: i64,
    symbol: &str,
    region_hash: &str,
) -> inspect::CopyTarget {
    inspect::CopyTarget {
        line,
        lines: lines.to_string(),
        start,
        end,
        symbol: symbol.to_string(),
        region_hash: region_hash.to_string(),
    }
}

/// Opens a throwaway editor for the clipboard verbs (WAL in the OS temp
/// dir, per-file auto language).
fn cli_editor(lang: Option<Lang>, stderr: &mut dyn Write) -> Result<Editor, ()> {
    Editor::open(Config {
        lang,
        wal_dir: std::env::temp_dir(),
        ..Default::default()
    })
    .map_err(|e| {
        let _ = writeln!(stderr, "bage: {e}");
    })
}

fn run_copy(a: CopyArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage copy: --file is required");
        return Err(());
    }
    let ed = cli_editor(None, stderr)?;
    let target = copy_target(a.line, &a.lines, a.start, a.end, &a.symbol, &a.region_hash);
    match ed.copy(&a.file, &target, a.clip) {
        Ok(res) => {
            let _ = emit(stdout, fmt, &res);
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &editor::envelope(&e)),
    }
}

fn run_cut(a: CutArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage cut: --file is required");
        return Err(());
    }
    let ed = cli_editor(None, stderr)?;
    let target = copy_target(a.line, &a.lines, a.start, a.end, &a.symbol, &a.region_hash);
    match ed.cut(&a.file, &target, a.clip) {
        Ok(res) => {
            let _ = emit(stdout, fmt, &res);
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &editor::envelope(&e)),
    }
}

fn run_paste(a: PasteArgs, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<(), ()> {
    let fmt = parse_format(&a.format, stderr)?;
    let lang = parse_lang(&a.lang, stderr)?;
    if a.file.is_empty() {
        let _ = writeln!(stderr, "bage paste: --file is required");
        return Err(());
    }

    let point_flags = usize::from(a.at_byte >= 0)
        + usize::from(a.append)
        + usize::from(a.before_line >= 0)
        + usize::from(a.after_line >= 0);
    if point_flags != 1 {
        let _ = writeln!(
            stderr,
            "bage paste: exactly one of --at-byte, --append, --before-line, or --after-line is required"
        );
        return Err(());
    }
    let point = if a.at_byte >= 0 {
        editor::PastePoint::AtByte(a.at_byte as usize)
    } else if a.append {
        editor::PastePoint::Point(inspect::InsertionPoint::Append)
    } else if a.before_line >= 0 {
        editor::PastePoint::Point(inspect::InsertionPoint::BeforeLine(a.before_line))
    } else {
        editor::PastePoint::Point(inspect::InsertionPoint::AfterLine(a.after_line))
    };

    let source_flags = usize::from(!a.text.is_empty())
        + usize::from(!a.text_file.is_empty())
        + usize::from(a.clip);
    if source_flags != 1 {
        let _ = writeln!(
            stderr,
            "bage paste: exactly one of --text, --text-file, or --clip is required"
        );
        return Err(());
    }
    let text = if a.clip {
        match bage::clipboard::read() {
            Ok(clip) => clip.content,
            Err(e) => {
                let env = editor::envelope(&editor::EditorError::Clipboard(e));
                return emit_envelope(stderr, fmt, &env);
            }
        }
    } else if !a.text_file.is_empty() {
        match std::fs::read_to_string(&a.text_file) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(
                    stderr,
                    "bage paste: read --text-file {:?}: {e}",
                    a.text_file
                );
                return Err(());
            }
        }
    } else {
        a.text.clone()
    };

    let ed = cli_editor(lang, stderr)?;
    match ed.paste(&a.file, point, &text) {
        Ok(results) => {
            let _ = emit(stdout, fmt, &EditResults(results));
            Ok(())
        }
        Err(e) => emit_envelope(stderr, fmt, &editor::envelope(&e)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_results_text_matches_go_shape() {
        let r = EditResult {
            path: "a.txt".into(),
            changed_start: 0,
            changed_end: 3,
            new_region_hash: "r".repeat(16),
            new_file_raw_hash: "x".repeat(16),
            new_file_norm_hash: "n".repeat(16),
            new_start_line: 1,
            new_end_line: 2,
        };
        let mut buf = Vec::new();
        EditResults(vec![r]).render_text(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s,
            format!(
                "applied a.txt bytes [0:3] lines [1:2] region={} raw={} norm={}\n",
                "r".repeat(16),
                "x".repeat(16),
                "n".repeat(16)
            )
        );
    }

    #[test]
    fn read_options_parses_lines() {
        let a = ReadArgs {
            file: "f".into(),
            line: -1,
            lines: "2-4".into(),
            start: -1,
            end: -1,
            symbol: String::new(),
            content: false,
            format: "text".into(),
        };
        let opts = read_options(&a).unwrap();
        assert_eq!((opts.line, opts.end_line), (2, 4));
        let bad = ReadArgs {
            lines: "x".into(),
            ..a
        };
        assert!(read_options(&bad).is_err());
    }

    /// Renders a [`ReadView`] over the given blocks for the text-format tests.
    fn render_read(blocks: Vec<Block>) -> String {
        let view = ReadView(ReadResult {
            path: "f.md".into(),
            lang: "markdown".into(),
            raw_hash: "x".repeat(16),
            norm_hash: "n".repeat(16),
            blocks,
        });
        let mut buf = Vec::new();
        view.render_text(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// A range block carrying `content`, for the --content rendering tests.
    fn block(content: &str) -> Block {
        Block {
            kind: "range".into(),
            name: String::new(),
            start_line: 1,
            end_line: 2,
            start_byte: 0,
            end_byte: content.len(),
            region_hash: "r".repeat(16),
            content: content.into(),
        }
    }

    #[test]
    fn read_view_text_without_content_prints_metadata_only() {
        let s = render_read(vec![block("")]);
        assert!(!s.contains('|'), "no content lines expected, got: {s}");
        assert_eq!(s.lines().count(), 2, "header + block line only, got: {s}");
    }

    #[test]
    fn read_view_text_prints_content_under_block_line() {
        let s = render_read(vec![block("hello\n")]);
        assert!(s.ends_with("    |hello\n"), "content line missing: {s}");
    }

    #[test]
    fn read_view_text_prints_multiline_content() {
        let s = render_read(vec![block("a\nb\nc\n")]);
        assert!(s.contains("    |a\n    |b\n    |c\n"), "got: {s}");
    }

    #[test]
    fn read_view_text_prints_content_with_no_trailing_newline() {
        let s = render_read(vec![block("a\nb")]);
        assert!(s.ends_with("    |a\n    |b\n"), "got: {s}");
    }

    /// [`ApplyArgs`] with defaults matching the CLI flag defaults.
    fn apply_args(file: &str) -> ApplyArgs {
        ApplyArgs {
            file: file.into(),
            raw_hash: String::new(),
            line: -1,
            lines: String::new(),
            start: -1,
            end: -1,
            all: false,
            append: false,
            before_line: -1,
            after_line: -1,
            text: String::new(),
            text_file: String::new(),
            region_hash: String::new(),
            lang: String::new(),
            fmt: String::new(),
            lint: String::new(),
            format: "text".into(),
        }
    }

    #[test]
    fn apply_region_all_spans_whole_file_without_region_hash() {
        let live = b"line1\nline2\n";
        let mut a = apply_args("f.md");
        a.all = true;
        let reg = apply_region(&a, live).unwrap();
        assert_eq!((reg.start_byte, reg.end_byte), (0, live.len() as i64));
        assert!(
            reg.region_hash.is_empty(),
            "--all must carry no region_hash"
        );
    }

    #[test]
    fn apply_region_all_rejects_other_addressing_modes() {
        let live = b"x\n";
        let cases: [fn(&mut ApplyArgs); 4] = [
            |a| a.line = 1,
            |a| a.lines = "1-2".into(),
            |a| a.start = 0,
            |a| a.end = 1,
        ];
        for set in cases {
            let mut a = apply_args("f.md");
            a.all = true;
            set(&mut a);
            let err = apply_region(&a, live).unwrap_err();
            assert!(err.contains("--all is mutually exclusive"), "got: {err}");
        }
    }

    #[test]
    fn apply_all_replaces_longer_file_with_shorter_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.md");
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        let mut a = apply_args(path.to_str().unwrap());
        a.all = true;
        a.text = "short\n".into();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run_apply(a, &mut out, &mut err).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short\n");
    }

    #[test]
    fn apply_all_replaces_shorter_file_with_longer_text_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.md");
        std::fs::write(&path, "tiny\n").unwrap();
        let mut a = apply_args(path.to_str().unwrap());
        a.all = true;
        // No trailing newline: --all must NOT strip or append one.
        a.text = "a\nb\nc".into();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run_apply(a, &mut out, &mut err).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc");
    }

    /// Runs `run_apply` over a temp file seeded with `initial`, returning
    /// the file content after the edit.
    fn apply_to_file(initial: &str, set: impl FnOnce(&mut ApplyArgs)) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.md");
        std::fs::write(&path, initial).unwrap();
        let mut a = apply_args(path.to_str().unwrap());
        set(&mut a);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run_apply(a, &mut out, &mut err).unwrap();
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn apply_append_to_file_with_trailing_newline() {
        let got = apply_to_file("a\nb\n", |a| {
            a.append = true;
            a.text = "c\n".into();
        });
        assert_eq!(got, "a\nb\nc\n");
    }

    #[test]
    fn apply_append_prepends_newline_when_file_lacks_one() {
        let got = apply_to_file("a\nb", |a| {
            a.append = true;
            a.text = "c\n".into();
        });
        assert_eq!(got, "a\nb\nc\n");
    }

    #[test]
    fn apply_append_to_empty_file_inserts_verbatim() {
        let got = apply_to_file("", |a| {
            a.append = true;
            a.text = "x\n".into();
        });
        assert_eq!(got, "x\n");
    }

    #[test]
    fn apply_before_line_one_inserts_at_start_and_ensures_newline() {
        // No trailing newline on --text: one is appended so line structure
        // is preserved.
        let got = apply_to_file("one\ntwo\n", |a| {
            a.before_line = 1;
            a.text = "zero".into();
        });
        assert_eq!(got, "zero\none\ntwo\n");
    }

    #[test]
    fn apply_after_line_last_appends_line() {
        let got = apply_to_file("one\ntwo\n", |a| {
            a.after_line = 2;
            a.text = "three".into();
        });
        assert_eq!(got, "one\ntwo\nthree\n");
    }

    #[test]
    fn apply_after_line_past_eof_clamps_to_end() {
        let got = apply_to_file("one\ntwo\n", |a| {
            a.after_line = 99;
            a.text = "three\n".into();
        });
        assert_eq!(got, "one\ntwo\nthree\n");
    }

    #[test]
    fn apply_region_insertion_modes_are_mutually_exclusive() {
        let live = b"x\n";
        let cases: [fn(&mut ApplyArgs); 6] = [
            |a| {
                a.append = true;
                a.before_line = 1;
            },
            |a| {
                a.append = true;
                a.after_line = 1;
            },
            |a| {
                a.before_line = 1;
                a.after_line = 1;
            },
            |a| {
                a.append = true;
                a.all = true;
            },
            |a| {
                a.append = true;
                a.line = 1;
            },
            |a| {
                a.before_line = 1;
                a.start = 0;
                a.end = 1;
            },
        ];
        for set in cases {
            let mut a = apply_args("f.md");
            set(&mut a);
            let err = apply_region(&a, live).unwrap_err();
            assert!(err.contains("bage apply:"), "got: {err}");
        }
    }

    #[test]
    fn apply_region_insertion_is_zero_width_without_region_hash() {
        let live = b"one\ntwo\n";
        let mut a = apply_args("f.md");
        a.append = true;
        let reg = apply_region(&a, live).unwrap();
        assert_eq!((reg.start_byte, reg.end_byte), (8, 8));
        assert!(
            reg.region_hash.is_empty(),
            "insertion must carry no region_hash"
        );
    }
}
