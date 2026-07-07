# Båge — Specification

> Bidirectional code-graph round-trip file editor. Standalone IDE-style edit engine over
> files + LSP; in integrated mode, Hylla links Båge as a library so one agent-facing edit
> lands in both the graph and the files with no possible drift. This SPEC is the buildable
> contract; design rationale lives in `docs/adr/` and `CONTEXT.md`. Crate: `bage` (Rust,
> edition 2024). Status: v0.7.0 (shipped) — cut / copy / paste over a file clipboard,
> insertion + whole-file-replace primitives, data-format key outline (§12). The Go
> implementation is archived on `go-legacy` and remains the byte-contract reference for
> normalize/hash parity; every hash digest below is byte-identical across both. Date:
> 2026-07-06.

---

## 1. Invariants (hard)

- **§1.1 Files are the source of truth.** The graph is a reconcilable projection. Commit
  ordering is file-first; the projection may never lead the truth.
- **§1.2 No drift, ever.** After any settled edit, the file and the graph agree. Partial
  failure is detected and resolved (handled failure → restore file; crash → converge on
  restart). Never silently misapply.
- **§1.3 Stable identity ≠ volatile locator.** Node identity is Hylla's (path-based,
  versioned). Båge addresses regions by `(file_content_hash, byte_range)` and never trusts a
  byte range whose file hash no longer matches.
- **§1.4 Båge is locator-addressed / ID-blind.** Båge operates only on
  `(file_path, byte_range, hashes)` + LSP ops; it never constructs or parses Hylla node IDs.
- **§1.5 Hexagonal.** Trait ports define boundaries; adapters implement them. Trait-first,
  dependency inversion, smallest concrete design. The ports are `ParserPort`, `Hasher`,
  `Formatter`, and `Linter`.
- **§1.6 Gates.** Every module is TDD-built and green under the three-command cargo gate —
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` (unit +
  property + concurrency + TOON goldens). CI runs exactly these three under the required
  status job `check`; the release workflow runs the same gate before building binaries.

## 2. Scope

- **In:** byte-addressable Tier-1 text formats (code; markdown, toml, yaml, json, html, rst,
  …) — parsed via tree-sitter, round-trip editable by byte-range splice + LSP `WorkspaceEdit`.
- **Out:** non-byte-addressable formats (EPUB/ebooks, docx/OOXML, pdf, ipynb) — Hylla's
  read-only doc adapters own these; their nodes are not editable (`Position == None`).

## 3. Architecture & module layout

The crate is a flat `src/` of hexagonal modules; `lib.rs` re-exports the public API and
`main.rs` is the standalone CLI. Trait ports (`ParserPort`, `Hasher`, `Formatter`, `Linter`)
keep every engine a swappable adapter.

```
src/main.rs         # standalone CLI entrypoint (clap derive)
src/lib.rs          # crate root; re-exports the public surface
src/atomicwrite.rs  # atomic temp+rename+fsync file writer
src/normalize.rs    # shared LF/BOM/trailing-ws normalization (MUST match Hylla)
src/hashing.rs      # Hasher port + XxHasher (xxHash64) / FnvHasher adapters; raw+norm digests
src/region.rs       # Region / FileAnchor / Edit / EditResult, hash_region, resolve (drift), LineIndex
src/parser.rs       # ParserPort port + Node/Tree/Point DTOs + tree-sitter Adapter; Lang enum
src/wal.rs          # edit-intent log (durable, file-based JSON; NO SQLite)
src/format.rs       # Formatter / Linter ports + command-exec adapters + fakes
src/edit.rs         # single-region round-trip: drift-check (two hashes) → splice → reparse
src/session.rs      # two-phase Prepare/Commit/Rollback over a multi-op batch + WAL; Kind taxonomy
src/lsp.rs          # LSP client + lifecycle; rename → WorkspaceEdit → FileEdits; workspace priming
src/editor.rs       # the public facade (Editor): apply/create/delete/move/read/rename + copy/cut/paste
src/inspect.rs      # open_file / outline / read_blocks / read_file; ReadOptions; resolve_* ; parse_health
src/render.rs       # Format{text|json|toon} + emit + the TextRender trait
src/clipboard.rs    # single-slot file clipboard (Clip; read/write) — v0.7 (§12)
```

Dependency direction: Hylla → Båge only. Båge imports nothing from Hylla.

## 4. Foundation ports & types

### §4.1 `atomicwrite`
- `pub fn write(path: &Path, data: &[u8]) -> Result<(), AtomicWriteError>` — write to a temp
  file in the same dir, `fsync`, then rename over the target. Clean up temp on error.
  POSIX-atomic.

### §4.2 `normalize`
- `pub fn normalize(b: &[u8]) -> Vec<u8>` — the shared rule, byte-identical with Hylla:
  1. normalize line endings to LF (drop `\r`);
  2. strip trailing horizontal whitespace per line: remove `[ \t\r]+` before each `\n` and at EOF;
  3. **LAST**, on the CR-free output, strip ALL consecutive leading UTF-8 BOMs (`EF BB BF`).
     Order matters: BOM-strip runs after `\r` removal and strips all leading BOMs, so neither a
     double BOM nor a `\r`-split BOM (`EF BB \r BF`) breaks idempotency (property-enforced; Hylla MUST match).
- This is the input to the **normalized** hash only; raw byte ranges index the **raw** file.

### §4.3 `hashing`
- `pub trait Hasher: Send + Sync { fn sum(&self, b: &[u8]) -> String }` — lowercase hex digest.
- `pub fn raw_hash(h: &dyn Hasher, raw: &[u8]) -> String` — gates byte-offset validity.
- `pub fn norm_hash(h: &dyn Hasher, raw: &[u8]) -> String` — `h.sum(&normalize(raw))`; the drift
  classifier.
- `XxHasher` is the canonical adapter: **xxHash64** formatted as `{:016x}` (16-char, zero-padded,
  lowercase hex). This fixed width+encoding IS the cross-system contract — byte-identical with
  Hylla and with the archived Go implementation. `FnvHasher` is a dependency-free test double.

### §4.4 `region`
- `pub struct Region { path, start_byte, end_byte, start_line, end_line, start_col, end_col, region_hash }`
  — mirrors Hylla's per-node locator bundle (`HYLLA_NODE_CONTRACT.md` §1) **minus graph identity**
  (no `parent_id`/`tail_symbol` — Båge is ID-blind). `start_byte == LINE_SENTINEL` (`-1`) marks a
  line-addressed region resolved via `start_line`/`end_line`. `region_hash` is the `{:016x}` of the
  region's NORMALIZED bytes, or `""` when the byte range is authoritative (single-model file mode).
- `pub struct FileAnchor { path, raw_hash, norm_hash }` — the per-file gate (§8.1). Built by
  `pub fn file_anchor(h: &dyn Hasher, path: &str, raw: &[u8]) -> FileAnchor`.
- `pub struct Edit { region, new_text }`. The model **echoes** a shown `region_hash` (omp-style) —
  it never computes a hash or resends old text. Graph-mode resolves a `node_id` Hylla-side to a
  `Region` before Båge sees it.
- `pub struct EditResult { path, changed_start, changed_end, new_region_hash, new_file_raw_hash,
  new_file_norm_hash, new_start_line, new_end_line }` — the write-back contract (§8.2).
- `pub fn hash_region(src, start, end) -> String` — the single source of a region_hash.
  `pub fn resolve(...) -> Result<Region, ResolveError>` relocates a region under drift, returning a
  `ResolveStatus` (exact / benign shift / conflict). `LineIndex` maps lines ↔ bytes for addressing.

### §4.5 `parser`
- `pub trait ParserPort: Send + Sync` — the engine-agnostic contract (`parse`,
  `parse_incremental`, `changed_ranges`). `Adapter` is the tree-sitter implementation.
- DTOs: `Node { kind, start_byte, end_byte, start_point, end_point, … }`, `Point { row, col }`,
  `ByteRange { start, end }`, `InputEdit { … }`, `Tree` (with `has_native()` false for the text
  fallback), and the `Lang` enum with `Lang::for_path`, `Lang::name`, `Lang::from_name`.

### §4.6 `wal`
- `pub struct Intent { id, edits: Vec<FileEdit>, originals, expected_raw_hash, expected_norm_hash, … }`
- `pub fn append(dir: &Path, intent: &Intent)` / `pub fn replay(dir: &Path) -> Result<Vec<Intent>, …>`
  / `pub fn clear(dir: &Path)` — durable, file-based (one fsynced JSON record per intent). NO SQLite.

## 5. Drift discipline (edit-time)

Before applying any region: hash the live file (raw). `raw` match → byte range valid → apply.
`raw` mismatch + `norm` match → whitespace-only drift → re-ground (re-resolve the region by
`region_hash`) then apply. `norm` mismatch → real drift → re-ground from Hylla or **reject**
(never slide). This is `region::resolve`, run fresh under the per-file lock on every relocation.

## 6. Modes

- **Standalone:** files + LSP; no graph; same locator/edit engine, driven through the `Editor`
  facade (or the CLI).
- **Integrated:** Hylla's coordinator drives the two-phase saga (WAL → `Editor::prepare`
  [stage temp, fmt/lint, parse CST] → `graph.prepare` → `Editor::commit` → `graph.commit`),
  restore-on-handled-failure / converge-on-crash.

## 7. Non-goals (parked, named)

External-edit file watcher; shadow-graph interaction (Hylla's); MCP edit-tool naming;
OOXML/pandoc editing.

## 8. Region-anchored edit model + concurrency (ADR-0003)

The edit unit is **region-anchored**, not byte-only, so a model sends the fewest tokens,
mistakes reject instead of corrupting, and concurrent edits are lossless. This matches omp
(`can1357/oh-my-pi`) and improves on it by editing CST regions/"blocks", not whole files.

### §8.1 Edit input
- `Region` (§4.4) is the content-anchored target; `FileAnchor` is the per-file gate. `Edit`
  pairs a `Region` with its `new_text`. Addressing is line-facing (model) / byte-internal.

### §8.2 Write contract (back to Hylla)
- `EditResult` (§4.4). `Editor::commit` returns `Vec<EditResult>` so Hylla incrementally
  re-ingests **only** the changed region.

### §8.3 Concurrency (per ADR-0003)
- **Per-file serialization** (one writer per file); cross-file edits run in parallel.
- **Resolve the region under the file lock, immediately before applying**, so every edit sees
  prior concurrent commits (no lost update).
- `region_hash` matches at a shifted offset = **benign shift** → re-resolve → apply.
  `region_hash` no longer matches = **conflict** → hard reject.
- The file-leg session reparses the live file fresh under the per-file lock on every relocation
  (no stale tree is ever trusted), which is correctness-equivalent to the incremental
  tree-reuse + `didChange` path that belongs to the integrated coordinator/LSP layer. The
  per-region `region_hash` is the live gate; the per-file `FileAnchor` hashes are informational
  (carried for Hylla's whole-file fast-path).

### §8.4 Hard errors (never silent)
tree-sitter parse failure → reject (always); configured lint failure → reject; fmt → applied;
`region_hash` unresolvable → reject.

### §8.5 omp parity proof
Snapshot = live file + region_hash; replay = reparse-and-match-by-hash; reject =
conflict/ambiguity. Falsifiable tests include: concurrent same-file edits (benign-shift
re-resolves, conflict rejects, no lost update), cross-file parallel, and reject-not-corrupt on
every drift class — exercised with real threads.

### §8.6 Property-enforced invariants
Two properties are enforced by property tests (not merely table-tested):
1. **Normalize idempotency**: `normalize(normalize(b)) == normalize(b)` for arbitrary bytes.
   Holds **only because** BOM-stripping runs LAST on the CR-free output and strips ALL leading
   BOMs — a `\r`-split BOM (`EF BB \r BF`) collapses to a BOM under `\r` removal and is then
   stripped (§4.2). Hylla MUST reproduce this exact order so `norm_hash` agrees cross-system.
2. **Text-fallback losslessness**: parsing under `Lang::Text` preserves the source byte-for-byte
   and spans `[0, len)` for any bytes (binary, multibyte UTF-8, lone CRs, BOMs) — the property
   that lets an agent IDE open ANY file without corruption.

## 9. Agent IDE surface & file-type coverage

Båge presents a uniform, file-type-agnostic editing surface so an agent can open, inspect, and
edit **any** file.

### §9.1 `Lang::for_path` — total language selection
`Lang::for_path(path: &str) -> Lang` maps a path to a `Lang` by extension (case-insensitive)
plus a few basenames (`Makefile`/`GNUmakefile`/`.mk` → `Make`; `Dockerfile`/`Containerfile` →
`Text`). It **never returns an unknown** — unknown extensions, dotfiles (`.env`, `.gitignore`),
extensionless and empty paths resolve to `Lang::Text`. Every file is therefore at least
text-editable (table- + property-enforced). `Config::lang` is an **optional override**: `None`
means auto-detect per file via `Lang::for_path`; `Some(lang)` forces that language for every
file. `Editor::open` does not require a `Lang`.

### §9.2 `Lang::Text` text-fallback contract
Under `Lang::Text` the adapter builds a document root spanning the whole file with **no native
tree** (`Tree::has_native()` is false) and one named line child per source line (each line keeps
its terminating `\n`, so concatenating children reproduces the source EXACTLY). The tree's source
is the input bytes verbatim — a byte-range splice + reparse is lossless for any bytes. Because
there is no native tree, `changed_ranges` degrades to a full reparse. This is the contract that
makes the grammar-less open→edit→write path corruption-proof.

### §9.3 Grammar + LSP coverage matrix
- **20 tree-sitter grammars** (real parse + round-trip fixtures): Go, TypeScript, TSX,
  JavaScript, Python, Rust, Java, C, C++, C#, Ruby, JSON, HTML, CSS, YAML, TOML, XML, Makefile,
  Bash, **Markdown**.
- **Text fallback** (lossless, no grammar): MDX, SCSS, Dockerfile, `.txt`, dotfiles, and any
  unknown type.
- **LSP rename availability VARIES by design** — it is an accelerator, not a precondition (the
  graph/LLM path covers what an LSP can't). gopls and rust-analyzer do full cross-file rename
  natively; clangd is carried across translation units by a generated `compile_commands.json`
  and pyright across files by workspace priming (§12.5); single-file rename is available wherever
  a server exposes it. No LSP rename by design for data/markup/build/script types and `Lang::Text`.

### §9.4 Inspect surface (`inspect`)
- `open_file(path: &str) -> Result<OpenedFile, InspectError>` — read + `Lang::for_path` + parse;
  `OpenedFile { path, lang, tree }`.
- `outline(tree: &Tree, lang: Lang) -> Vec<Symbol>` — documentSymbol-like listing: named
  declaration nodes (grammar-agnostic, by node kind) with byte + 1-based line ranges; the
  text fallback returns one line `Symbol` per source line, and data grammars list their named
  keys (§12.4).

## 10. File-lifecycle ops: create / delete / move / batch (ADR-0004)

Båge edits existing files (§8) **and** manages their lifecycle. All lifecycle ops ride the
same anchored two-phase engine — there is no second, weaker write path.

### §10.1 The `Op` batch
- The transaction unit is `Vec<Op>`, a tagged enum `Op::{Edit | Create | Delete | Move}`. One
  `Session::prepare`/`commit`/`rollback` stages and applies a **heterogeneous** batch as one
  logical change; the `Editor::apply`/`rename` verbs are the edit-only / rename-only cases, and
  `Editor::apply_batch` runs a mixed batch.
- Per-file locks are acquired in deterministic sorted order (deadlock-free); every op's anchor is
  validated and every write staged as a sibling temp before any flip.

### §10.2 Per-op anchors (the content-hash promise, extended)
- **Create** (`Op::Create { path, content, lang }`) — anchored by **non-existence**. Existing
  path with content → **hard reject** (never clobber).
- **Delete** (`Op::Delete { path, expected_raw_hash }`) / move-**source** — anchored by the
  expected **`raw_hash`**. Drift → **hard reject**. Prior bytes are WAL-captured for rollback
  before unlinking.
- **Move** (`Op::Move { from, to, expected_raw_hash }`) — `= anchored-delete(source) +
  anchored-create(dest)`, atomic-on-recovery, preserving the bytes unchanged.

### §10.3 "Atomic" defined honestly
POSIX has no multi-file atomic flip. Cross-file all-or-nothing is **WAL-backed, on recovery**:
the WAL records batch intent + undo bytes; `Session::recover` drives a crashed mid-flip batch to
fully-before or fully-after, never half. **File-first ordering** keeps the graph leg from leading
durable file state.

### §10.4 Gate boundary (Båge vs caller)
Båge's gate is the **mechanical per-file parse floor** (staged bytes must still parse → else hard
reject) plus **caller-configured `Formatter`/`Linter` hooks Båge executes on the staged bytes**.
The floor is **lenient by design**: tree-sitter is error-tolerant, so a tree with `ERROR`/`MISSING`
nodes is accepted (agents may write broken intermediate states; `diagnose` surfaces the defects,
the caller decides) — only bytes that produce no tree are rejected. **Project-level correctness —
whole-module compile/run, tests, and commit *timing* — is the caller's (Hylla's); Båge never runs
the build or tests.** The `prepare`/`commit` split hands the caller the commit-timing lever.

### §10.5 Read + diagnostics + scope edges
- **`show`** — emits a file's region + `region_hash` map (the addressable-block read view).
- **Diagnostics** — after an edit/move, LSP `publishDiagnostics` + the parse result ride the
  result envelope. Båge surfaces; the caller fixes.
- **Out of scope by design**: text search (ripgrep/harness), directory pruning (`create` makes
  parent dirs; `delete` leaves an empty parent for the host/VCS), an undo stack (git = history,
  WAL = crash-recovery), and a full LSP nav server. LSP scope is **write-adjacent only**: rename,
  `willRenameFiles`, diagnostics.

### §10.6 Graph-agnostic + open
Ops are locator-addressed primitives. Hylla originates them from a graph mutation in integrated
mode; an MCP wrapper originates them in standalone mode.

## 11. Read primitive + serialization + error taxonomy

Standalone callers need a read Hylla doesn't (Hylla holds content in its node), so Båge ships a
first-class read plus an output-encoding seam and a machine-branchable error taxonomy.
Library-first: the Rust types are the product; the CLI is a thin serialization edge.

### §11.1 Read API
- `inspect::read_blocks(opened: &OpenedFile, include_content: bool) -> Vec<Block>` — the
  OpenedFile-level primitive (a host that already parsed reuses its tree). `Block { kind, name,
  start_line, end_line, start_byte, end_byte, region_hash, content }` is **flat** so JSON stays
  snake_case and a block slice is a uniform array TOON renders tabular.
- `Editor::read(path, opts: &ReadOptions) -> Result<ReadResult, EditorError>` — the facade.
  `ReadOptions { include_content, symbol, line, end_line, start_byte, end_byte }` addresses
  whole-file (defaults), by `symbol` (name match), by line, or by byte range; `include_content`
  adds raw bytes. The CLI `bage read` mirrors it.
- `region_hash` is computed once by `region::hash_region`; `show` and `read` share it.

### §11.2 Serialization (`render`)
- `Format::{Text | Json | Toon}` + `emit<T: Serialize + TextRender>(w, format, &v)`. `Json` is
  serde `to_writer_pretty`; `Text` dispatches to a `TextRender` the result type owns (domain
  types stay render-free, no cycle); `Toon` is compact tabular for uniform arrays (~30–60% fewer
  tokens than JSON). Every verb takes `--format`, default `text`.

### §11.3 Error taxonomy
- `session::Kind::{Conflict | Drift | Exists | NotFound | Usage | Io}`, `SessionError::kind() ->
  Kind`, and `session::envelope(err: &SessionError) -> ErrorEnvelope { kind, path, message }`
  (serde-serializable to JSON/TOON) — so an external MCP wrapper branches on `kind` without
  parsing English. `EditorError` wraps `SessionError` via `#[from]` and `editor::envelope`
  produces the same envelope. Conflict (region) and drift (raw_hash) carry **distinct** kinds.

## 12. Clipboard verbs + insertion primitives (v0.7)

Region **move/duplicate** and **insertion** are first-class, riding the same anchored
two-phase engine — no second write path.

### §12.1 Insertion primitive (#20)
`inspect::resolve_insertion(src, InsertionPoint)` resolves a **zero-width** region
(`start == end`) for `Append` (EOF), `BeforeLine(n)`, or `AfterLine(n)`. It carries **no
`region_hash`** — there is no content to hash — so the per-file anchor is the only drift gate.
Shared by `bage apply --append/--before-line/--after-line` and `bage paste`. `bage apply --all`
resolves the whole-file span `[0, len)` (also hash-free) for a lossless whole-file replace,
fixing the stale-tail hazard of a too-short `--lines` range (dogfood finding #12). All four are
mutually exclusive with each other and with `--line/--lines/--start/--end`.

### §12.2 Clipboard verbs (cut / copy / paste)
- **`copy`** — `Editor::copy` extracts a region READ-ONLY by `--symbol`, `--line`/`--lines`, or
  `--start`/`--end` (optional `--region-hash` verifies + benignly relocates). `text` output is
  the **bare content** so it pipes.
- **`cut`** — `Editor::cut` extracts **and removes** the region: WAL-backed, `region_hash`-gated;
  a hash mismatch rejects and nothing is removed.
- **`paste`** — `Editor::paste` inserts at a `PastePoint` (`AtByte(n)` verbatim, or an
  `InsertionPoint`) from `--text`, `--text-file`, or `--clip`. Exactly one point and exactly one
  source are required.

### §12.3 File clipboard (`--clip`)
A single-slot JSON record at `$BAGE_CLIPBOARD` (default `~/.bage/clipboard.json`, OS temp
fallback when `HOME` is unset), written atomically. `Clip{content, source_path, region_hash,
cut}` carries the bytes plus provenance; `cut --clip` writes the slot **before** the removal
commits. This makes a region move **cross-file and cross-process**; `paste --clip` on an empty
slot is the distinct `Empty` error. Båge never touches the OS/GUI clipboard.

### §12.4 Data-format key outline (#21)
The outline declaration-kind set is extended per data grammar so named-key addressing works:
JSON `pair`, YAML `block_mapping_pair`, TOML top-level `pair` + `table`, XML/HTML `element`,
each with name extraction. Code grammars keep the substring `is_decl_kind` path.

### §12.5 LSP cross-file rename completeness (#23)
`Client::rename` primes the workspace — `didOpen`ing same-language siblings under the root
(capped, `BAGE_LSP_NO_PRIME=1` to disable) — so servers that only see open files (pyright) rename
across files. For clangd, a minimal `compile_commands.json` is generated when absent (and removed
on close) so a rename crosses translation units. Container-verified for gopls, pyright, and clangd
(`BAGE_DOCKER_LSP=1`).
