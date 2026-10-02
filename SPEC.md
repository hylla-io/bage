# Båge — Specification

> Bidirectional code-graph round-trip file editor. Standalone IDE-style edit engine over
> files + LSP; in integrated mode, Hylla links Båge as a library so one agent-facing edit
> lands in both the graph and the files with no possible drift. This SPEC is the buildable
> contract; design rationale lives in `docs/adr/` and `CONTEXT.md`. Crate: `bage` (Rust,
> edition 2024). Status: v0.8.6 (shipped) — cut / copy / paste over a file clipboard,
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
- `pub fn append(dir, &Intent)` / `record_applying(dir, id, &Applying)` / `mark_landed(dir, id)` /
  `replay(dir) -> Result<Vec<Replayed>, …>` / `clear(dir, id)` / `clear_legacy(dir)` — durable,
  file-based, fsynced. NO SQLite.
- **One file per intent**: `<dir>/bage-intent-<hex id>.wal`. `clear(dir, id)` unlinks that file only,
  so finishing one op never drops another op's in-flight intent. `append` creates the file
  exclusively (a repeated id is `WalError::DuplicateId`).
- Line 1 is the intent (no `v` key; Go-compatible fields, plus `after` — the raw hash each lifecycle
  op leaves at each path, `null` = absent). Later lines are `{"v":2,"applying":{path,after,before?}}`
  (written before a commit writes `path`; `before` = replaced bytes when they differ from
  `originals[path]`) and `{"v":2,"landed":true}` (all bytes durable, written before `clear`). A record
  whose write or fsync fails is truncated back off the file, so a failed marker never reads as
  `Landed`; if that truncation also fails, the record may survive.
- `Replayed { intent, applied, status: Torn | Pending | Landed | Legacy }`. A torn LAST line reads as
  absent (a torn intent line is `Torn`). Loud `WalError`: an unknown `v`, a misshapen record, a record
  after `landed`, an unparsable line before the end, a file name that disagrees with its intent.
- `<dir>/wal.log`, the shared log older versions wrote, is replayed as `Legacy` and recovered by the
  old unconditional rules.

### §4.7 What is on disk after each outcome
- **Success**: the op's bytes; its intent cleared.
- **Clear fails after `landed`**: `SessionError::LandedWalNotCleared`. The bytes DID land; the intent
  stays marked landed and `recover` keeps the bytes and removes it.
- **Marker fails** (commit, create, delete, batch): the op is undone now, as for any other failure
  below, with the WAL error as the cause: `SessionError::Wal` when fully undone, else `LeftBehind`.
  A single-op move converges forward, so there it is `LandedWalNotCleared`.
- **Any other failure**: the op is undone now — every path put back only while it still holds this
  op's bytes. Fully undone: the intent is cleared and the error is the cause. Otherwise the error is
  `SessionError::LeftBehind { cause, residue }`, one entry per path or intent:
  - `Residue::NotUndone { path }` — the put-back failed; the path still holds the op's bytes. The
    intent is kept and `recover` finishes the undo.
  - `Residue::ChangedSince { path }` — another writer changed the path after this op wrote it. It
    is kept as found and may still carry this op's change. Nothing more can be undone, so the
    intent is cleared.
  - `Residue::IntentNotCleared { id }` — the disk is as found; the stray intent is harmless.

  `kind()` is the cause's when only intents were left, else `io`.
- **Crash**: the intent and whatever bytes landed. `Session::recover(dir) -> RecoverReport` keeps
  `Landed` intents; undoes each `Pending` intent path by path, touching a path only while it holds
  exactly that intent's after-image (commit: `applying`; lifecycle ops: `after`); converges a
  single-op move forward, removing the source only once the destination holds its bytes; removes
  `Torn` files. A prepared plan that never wrote needs nothing. Steps repeat until none can act, so
  several unfinished writes to one path undo in any replay order. When those writes form a cycle
  (content went A→B→A) the log cannot order them: the path is not touched. A path holding anything
  else, or with such a cycle, is left as found and listed in `RecoverReport.left`. Residual: a
  later write leaving byte-identical content is indistinguishable from this op's.
- **Crash before the intent exists**: create and move act on the target BEFORE the WAL append, so
  a pre-existing path is refused before any record names it. A crash in that window leaves, with no
  intent and nothing for `recover` to see: create — an empty file at the claimed path; move — a
  full copy at the destination, the source intact.
- **Durability**: file contents and their directories are fsynced before an op reports landed:
  `atomicwrite` syncs the directory after its rename, a create or move destination syncs its
  directory after its write, and `wal::clear` syncs the WAL directory. Unlinks of a delete or a
  move source are not directory-synced: after power loss the removed file can reappear.
- `recover` must run with no op in flight on `dir`. A failed write stops it with the WAL intact;
  running it again finishes the job.

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
- **Hylla's MVP code languages** are a subset: Rust, TypeScript/TSX, JavaScript, Python, Go.
  Every grammar above is still parsed, outlined and edited by Båge.
- **Outline names are verified** against each language's server for those five only. C and C++
  names are unreliable on macro-heavy code and on code the grammar misreads (C++ in a `.h` is
  read as C); C# names are lightly measured; Java and Ruby names are unmeasured.
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
- **Clean parse or misread — one rule, every code grammar.** A code `Symbol` is named by the
  field rules below only when the top-level item holding it (the root's child it sits in)
  contains no ERROR or MISSING node and the root itself is not an ERROR (an unbalanced
  include guard makes it one, and then no top-level item is a unit the grammar read, however
  clean its own nodes). Otherwise the grammar misread the code around it, and the
  block keeps EXACTLY the name v0.11.0 gave it: the text of its first identifier-kind direct
  child, else of the first one a level down — raw, so it may be a keyword, a type, a macro, or
  text spanning lines with comments in it. Why: the grammar cannot expand macros and reads
  every `.h` as C, so inside a misread its fields hold whatever word landed there (a keyword, a
  macro, a parameter, a callee), and per-shape recovery rules fixed the cases they were written
  for and broke code nobody had read. Keeping v0.11.0's name there changes nothing a host
  already relied on. The rule is coarse by design: one ERROR anywhere in an include-guarded
  header or an `extern "C" {` block puts every declaration in it on the v0.11.0 name.
- On a clean parse, a code `Symbol`'s `name` is read from the grammar's FIELDS, never from the
  first identifier child, because a C-family declaration starts with its type. In order: the `name` field (kept
  whole, so qualified names survive: `Fixture.Geo`, `inner::deep`); else the `declarator` field,
  followed through pointer / reference / array / function / parenthesized / init declarators to
  the declared identifier (`char *dup()` → `dup`, `Point Shape::area()` → `Shape::area`,
  `int (*get_cmp(void))(int, int)` → `get_cmp`); a declaration with several declarators takes the
  first. An MSVC calling convention is not a declarator: `static void __cdecl f(void);` → `f`.
  Template ids, structured bindings, nested namespaces and specializations are kept whole:
  `f<int>`, `Vec<int>`, `[a, b]`, `a::b::c`, `Box<int>`; operator names are written as declared,
  so `operator/` and `operator/=` contain a `/`.
- On a clean parse a C or C++ name is one line: whitespace runs collapse to one space and
  comments are dropped; a field-free fallback name that still holds a comment is `""`.
- Wrappers name what they wrap: a Python `decorated_definition`, a C++ `template_declaration`
  and a C++ `friend_declaration` take the name of what they hold (`friend class Vec<int>;` →
  `Vec<int>`).
- C# members without a name field are named `~Point` (destructor), `operator +`,
  `implicit operator int`, and `this` (indexer).
- A C++ conversion operator is `operator` plus its target type, pointer and reference kept, never
  its parameters or qualifiers: `operator const char *() const` → `operator const char *`,
  `operator int &()` → `operator int &`, `Widget::operator bool() const` →
  `Widget::operator bool`.
- A TypeScript/JavaScript member keyed by a string, number or computed key loses its brackets
  and quotes ONLY when what is left is a plain ASCII identifier or a number: `['KEY']` → `KEY`,
  `[42]` → `42`, `"field"` → `field`. Every other key stays exactly as written: `['a-b']`,
  `['a/b']`, `['.env']`, `[Symbol.iterator]`, `[KEY]`, `[KEY + '2']`, `['k' /* c */]`.
  Stripping those would put a separator into a path built from names, or make distinct keys
  look alike.
- A TypeScript construct signature is `new()`, as TypeScript's navigation tree names it, never
  its return type.
- A TypeScript/JavaScript arrow function, function expression, generator expression or class
  expression is named by the binding that holds it, never by a word from its parameters or body:
  the variable (`const f = () => …` → `f`); the assignment's left side, its last property when
  it is a member (`this.parseArg = (arg) => …` → `parseArg`, `obj.x = function () {}` → `x`,
  `g.R ??= class {}` → `R`); an object key or class field key, by the member-key rule above
  (`{ onload: () => … }` → `onload`, `{ 'on-error': … }` → `'on-error'`, `onClick = () => …` →
  `onClick`). Parentheses, `as`, `satisfies` and `!` around it are seen through. Otherwise it is
  bound to no plain name — a callback, an argument, a default value, a returned value, a
  computed target (`handlers[k] = () => …`) — and is `""` unless it spells its own name
  (`function helper() {}` → `helper`, `class Named {}` → `Named`).
- A Rust `impl` is named by its `type` field as written, whitespace collapsed and comments
  dropped (`impl BlobRef` → `BlobRef`, `impl<T> Wrapper<T>` → `Wrapper<T>`). A trait impl is that
  type qualified by its trait, as Rust spells it: `impl Validate for BlobRef` →
  `<BlobRef as Validate>`, `impl<T> From<T> for Wrapper<T>` → `<Wrapper<T> as From<T>>`,
  `impl !Send for X` → `<X as !Send>`. Why: one type commonly has an inherent impl and several
  trait impls in one file whose methods share names (`fmt` under `Display` and `Debug`); a host
  that keys a member by its containers' names can tell them apart only when the impls' names
  differ. Two impls with the same type AND trait (different generic bounds) can still share a
  name.
- **A clean C or C++ parse can still be a misread**: the grammar reads some unexpanded macros and
  C++-in-a-`.h` without any ERROR. Three shapes are handled, each seen in the corpora:
  - the declarator chain ends on a keyword (for C, the C keywords plus `operator`, which a C++
    `operator()` read as C leaves there; other C++ keywords stay valid C names) or on a builtin
    type spelling: `""`, except a typedef defining one (`typedef _Bool bool;` → `bool`);
  - a keyword that names no type sits in the type slot (`export C_LIB_NAMESPACE {…}`): `""`;
  - a definition whose declarator is bare parentheses (`do_library_init(void) {…}` after a
    macro return type on the line before): the parentheses are the parameter list and the type
    word is the function, `do_library_init`.
- Named `""` besides, on a clean parse: an anonymous declaration (a C++ `namespace { }`); a
  class, interface or enum BODY (`class_body`, `interface_body`, `enum_body`, Java
  `enum_body_declarations`), which declares nothing, so it never takes its first member's name;
  and a declarator chain that ends without a name. None falls back to the type.
- Only a node whose grammar has no naming field falls back to the first-identifier search.

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
the WAL records batch intent + undo bytes + after-images; `Session::recover` drives a crashed
mid-flip batch to fully-before (a single-op move to fully-after), never half (§4.7). **File-first ordering** keeps the graph leg from leading
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
(capped, `BAGE_LSP_NO_PRIME=1` to disable) — for servers that only see open files. `initialize`
declares the root both as `rootUri` and as the single entry of `workspaceFolders`; pyright ignores
`rootUri` and renames across files only with the folder declared. The dynamic
`workspace.workspaceFolders` capability is not claimed. `processId` is declared per client
(`ClientConfig::process_id`, default this process's id, `None` = `null`); a server in another pid
namespace needs `None`, or it exits when it cannot see the pid. For clangd, a minimal `compile_commands.json` is generated when absent (and removed
on close) so a rename crosses translation units. A server still indexing may answer with a
non-empty but partial edit (a cold clangd returns the target TU alone), which no wire signal
distinguishes from a complete one; a caller needing every reference passes `Client::await_ready`
at a position inside a referencing file first. The probe alone is necessary, not sufficient:
readiness is not monotonic while a server loads its workspace — rust-analyzer can pass the probe,
then answer empty until its load finishes, so an empty result after a probe-only gate is still not
proof of "none". A declared ready signal (§12.6) narrows that window to what the server reports;
it cannot close a reload the server has not yet announced.
Container-verified for gopls, pyright, and clangd
(`BAGE_DOCKER_LSP=1`), the clangd case behind that readiness gate.

### §12.6 Declared LSP session
A consumer declares every time bound of a session, and what the handshake says; none is fixed
at compile time.
`lsp::ClientConfig { initialize_timeout, call_timeout, rename_deadline, rename_retry,
query_deadline, query_retry, ready_deadline, ready_retry, shutdown_timeout, exit_deadline,
exit_poll, process_id, initialization_options, experimental_capabilities, ready_failures,
ready_signals, diagnostic_failures, hover_content_format, stderr_tail_bytes }` is applied by
`Client::configure` BEFORE
`initialize` (read back with
`Client::config`), or carried by `LspPool::with_client_config`, which configures each server
between spawn and handshake — the only point a pooled handshake can be bounded.
`initialize` is bounded by `initialize_timeout`, not `call_timeout`; `close` blocks at most
`shutdown_timeout + exit_deadline` before killing the child. `ClientConfig::default()` holds
bage's values (30 s / 30 s / 30 s / 300 ms / 30 s / 300 ms / 120 s / 500 ms / 2 s / 3 s / 50 ms;
no experimental capabilities, no ready failures, no ready signals, no diagnostic failures,
hover as markdown then plain text, 16 KiB of stderr).
The struct is exhaustive on purpose: a new bound breaks a full-literal declaration at compile
time rather than defaulting silently, and ships as a minor bump.
`initialization_options` reaches the server VERBATIM as `initializationOptions`, and an unset
one omits the member — an omitted member and a declared `null` are different requests, and
bage invents neither. bage defines no shape for it and encodes no policy: server behaviour the
protocol exposes nowhere else is the caller's to declare. A TypeScript server's automatic type
acquisition is the worked case — undeclared, it runs `npm install --ignore-scripts
types-registry@latest`, reaching the network and a user cache, and only
`initializationOptions.disableAutomaticTypingAcquisition` stops it. Accept-vs-control proof
against the real server: `tests/lsp_typing_acquisition.rs`.
`experimental_capabilities` reaches the server VERBATIM as `capabilities.experimental`, beside
bage's own capabilities, and is omitted when unset.
A server that SAYS it has failed ends `await_ready` at once. Status notifications are server
extensions with no common shape, so the caller declares each as a `ReadyFailure { method, when,
message_pointer, mode }`: the method to watch, members its params must carry (recursive subset
match), an RFC 6901 pointer to the message (absent or unresolved = the whole params), and a
`ReadyMode` read as a signal's is. Under `Latest` only the LATEST notification per watched method
counts, so a later recovery supersedes a failure; under `Once` the first match holds whatever
that method carries afterwards, for as long as that exact rule stays declared — a new or changed
rule starts with nothing.
Unwatched notifications are dropped, and a method no longer declared forgets its last status.
While a rule holds, `await_ready` returns
`LspError::ServerReported { method, message, locations, stderr }` instead of probing to the
deadline; with none declared, probing alone decides.
rust-analyzer is `experimental_capabilities: {"serverStatusNotification": true}` plus
`method: "experimental/serverStatus"`, `when: {"health": "error", "quiescent": true}`,
`message_pointer: "/message"` — an orphaned workspace member reports "Failed to load workspaces."
in under a second where the undeclared gate waits out its whole deadline.
A server that SAYS it has finished loading is the other half. A probe cannot tell a loaded server
from one mid-load: rust-analyzer resolves a declaration to itself while it rebuilds its crate
graph, then answers `outgoingCalls` and cross-crate `definition` empty until the load ends. So the
caller may declare `ReadySignal { method, when, mode }` entries in `ready_signals`, matched as a
`ReadyFailure`'s `when` is. `mode` says how the server speaks. `ReadyMode::Latest` (`"latest"`,
and what a declaration silent about it means) reads a STATE: met only while the LATEST
notification of the method matches. `ReadyMode::Once` (`"once"`) reads an EVENT: met for good by
any matching notification, whatever that method carries afterwards. gopls is the worked case: it
announces a finished load with ONE `window/showMessage` of `Finished loading packages.` and goes
on to send unrelated messages on that method, so read as a state its signal is withdrawn by the
next one and a loaded server times out. A `Once` signal stays met for as long as that exact
declaration is made, across `Client::configure` calls; a changed declaration starts over from
the latest notification kept for its method. As data a signal is `{"method", "when", "mode"}`;
an unknown member or mode name is refused, never read as `latest`. Real-server proof, with the
state reading as its control: `tests/lsp_session.rs` (`BAGE_LSP_REAL_TEST=1`, gopls).
A notification with no
params is stored as `null`, which no object matches, so `when: {}` is never met by it. `await_ready`
sends no probe until EVERY declared signal matches; once they do, a non-empty answer ends the wait
only if every signal still matches when it arrives — the reader applies notifications in wire
order, so a withdrawal the server sent ahead of its answer voids that answer and the wait goes on.
The wait ends at `ready_deadline` in one of two typed ways, so a caller reads which half never
came without parsing a sentence. `ReadySignalDeadline { signal, latest, after, stderr }`: the
first unmet `ReadySignal` and the latest params seen for its method (`None` when the server never
sent that method). `ReadyProbeDeadline { path, line, character, after, last, stderr }`: every
signal held and the probe position never resolved; `last` is a `ProbeAnswer` of `Empty`,
`Refused { message }` or `NoResponse { after }`. A server that dies while a signal is unmet fails
at once with
`LspError::Closed`, as a probe would, rather than waiting out the deadline. With none declared,
probing alone decides. WHAT NO GATE CLOSES: a server that answers while its signal still matches,
and only afterwards announces a reload, passed the gate on a status it had not yet withdrawn; a
later query may then answer empty. The signal narrows the window to what the server reports; it
cannot see a reload the server has not announced. rust-analyzer is
`experimental_capabilities: {"serverStatusNotification": true}` plus
`method: "experimental/serverStatus"`, `when: {"quiescent": true}` — `quiescent` being its own
"no pending background work which might change the status". A declared signal whose server never
sends it (the capability left undeclared, say) holds the gate to its deadline and says why.
Real-server proof, with that control: `tests/lsp_session.rs` (`BAGE_LSP_REAL_TEST=1`).
A stdio server's stderr is drained for the child's whole life into a ring of the last
`stderr_tail_bytes` (zero keeps nothing); `StderrTail { text, dropped_bytes, captured }` says how
much older output was discarded, so a cut is never silent. `ReadySignalDeadline`,
`ReadyProbeDeadline` and `ServerReported` carry it and print it; every other error keeps its
shape, and `Client::stderr_tail()` reads the
tail after any failure. A `from_conn` client has no stderr (`captured: false`). Real-server proof:
`tests/lsp_session.rs` (`BAGE_LSP_REAL_TEST=1`, rust-analyzer).
Diagnostics a server publishes are KEPT, per document: a later `publishDiagnostics` for a URI
replaces that document's list and an empty one clears it. `Client::published_diagnostics()`
reads every document's current list as `PublishedDiagnostic { uri, path, start_line, start_char,
end_line, end_char, severity, source, code, message, raw }`, with `path` relative to the root
when the document lies under it (a symlinked or differently spelt root included) and absolute
otherwise. The store holds one entry per document that currently has a diagnostic, declared or
not, so it is bounded by the server's view of the workspace. A publish with an element that is
not a `Diagnostic` changes nothing.
A caller declares which ONE diagnostic means the project did not load as
`DiagnosticFailure { when, file_name, mode }` in `diagnostic_failures`: `when` is matched against
the diagnostic as the server sent it (`{"source": "go list"}`, `{"severity": 1}`), `file_name` is
an optional pattern on the document's last path segment where `*` is any run of characters
(`"go.mod"`, `"*.go"`), and `mode` says whether a diagnostic the server withdrew still counts
(`Once`) or not (`Latest`). A `Once` rule that matched holds for as long as that exact rule stays
declared, whatever the server or another rule does after. Each `Once` rule remembers, per
document, the diagnostics IT matched in the latest publish that held one: a withdrawal, a publish
it selects nothing in, or another rule's match leaves them; its own later match in that document
replaces them. That store is, per `Once` rule, one publish's matches per document the rule ever
matched in. It grows with rules times documents that failed, not with republishing. A rule
declared again with any member changed starts with only what the server currently holds.
As data an unknown member is refused, as a `ReadyFailure`'s is. bage
picks no diagnostic: with none declared, none stops anything. A rule that holds ends `await_ready`
with `ServerReported` whose `locations` list every selected diagnostic; `method` and `message`
are the matched notification's, or `textDocument/publishDiagnostics` and the first location's
message when only a diagnostic rule held.
`Client::check_failures()` judges the same rules against everything received SO FAR and sends
nothing, for use after queries: servers keep reporting once they have answered. `Ok` means no
declared failure has ARRIVED, never that none will; the push model has no "that was all of them",
so a caller that must cover a server's delay waits that long before asking.
Real-server proof (`tests/lsp_session.rs`, `BAGE_LSP_REAL_TEST=1`): gopls on a `go.mod` with an
unclosed `require (` is reported at `go.mod` 5:0 with source `syntax`, and its `go list`
diagnostic on the opened source file arrives about a second after the gate and is found by
`check_failures`; undeclared, the same module waits out the deadline on `Refused` probes.
rust-analyzer on a `Cargo.toml` that does not parse publishes NO diagnostic: it reports
`Failed to load workspaces.` through its status notification, and the manifest position exists
only as cargo's text in `stderr` (`Cargo.toml:1:9`), so `locations` is empty there.
`Client::hover(path, content, line, character) -> Result<Option<Hover>>` asks what the server
shows for the symbol at a position, a symbol defined outside the workspace included.
`Hover { text, kind, range }` is the server's text, the `MarkupKind` (`Markdown` or `PlainText`)
the SERVER says it is in, and the span hovered when the server names one. `None` is a `null`
answer or one with empty content; it carries no readiness information, so gate with `await_ready`
first. A server without `hoverProvider` is refused with `Unsupported`, nothing sent. The older
`MarkedString` shapes are rendered to the markdown the protocol defines them as.
A position OUTSIDE the text is the server's to judge, and a server that refuses it costs the whole
`query_deadline` (30 s by default): a refusal is a JSON-RPC error, the same thing a loading server
sends, so `hover` and `definition` retry it every `query_retry` and return
`LspError::QueryDeadline { method, path, after, last }` only when the deadline is spent, `last`
being the server's refusal. Measured: gopls refuses a column past the end of its line (`column is
beyond end of line`) and a line past the end of the file (`line number 500 out of range 0-10`);
rust-analyzer refuses the line (`Invalid offset LineCol { line: 500, col: 0 }`) and ANSWERS the
column at once, `None` for hover and no location for definition. bage checks no position against
the text itself; a caller that cannot rule such positions out declares a `query_deadline` it can
afford. Real-server proof at a 1 s deadline: `tests/lsp_session.rs` (`BAGE_LSP_REAL_TEST=1`, gopls
and rust-analyzer).
`hover_content_format` is the `textDocument.hover.contentFormat` capability, most wanted first;
empty omits it. Markdown leads by default because it is the one format in which every server
measured keeps the signature (a code fence) apart from the documentation: asked for plain text,
rust-analyzer runs a heading into the code after it and tsc joins the signature to the first
sentence. With the member omitted gopls answers markdown and rust-analyzer, tsc and pyright plain
text.
A server shows only documentation it can read. rust-analyzer reads the standard library from the
toolchain's `rust-src` component and a dependency from the sources cargo resolved; with no
standard-library sources a std symbol is unresolved and its hover is `None`, while a dependency's
is unaffected. gopls reads `GOROOT/src`. tsc reads the `lib.*.d.ts` files it ships and a package's
declarations under `node_modules`. pyright takes a standard-library signature from the stubs it
ships and the docstring from the standard library of a Python interpreter on `PATH`: with no
interpreter it shows the signature alone.
Real-server proof (`tests/lsp_session.rs`, `BAGE_LSP_REAL_TEST=1`): rust-analyzer on
`HashMap::insert` and a vendored registry crate, with the no-sources control; gopls on
`fmt.Println`; `tsc --lsp` on `parseInt` and a `node_modules` package; pyright on `json.dumps`.
Document sync is public: `Client::did_open(path, text)` (close-then-open on a re-open, per the
spec's balanced open/close rule) and `Client::did_close(path) -> Result<bool>` (`false`, nothing
sent, when the document is not open). Real-server proof: `tests/lsp_session.rs`
(`BAGE_LSP_REAL_TEST=1`, rust-analyzer + gopls resolve a definition only present in the opened
buffer).

### §12.7 Implementation + type hierarchy
`Client::implementation(path, content, line, col) -> Vec<SymbolLocation>` issues
`textDocument/implementation` (a trait/interface method to each implementing method; a
`LocationLink` resolves to its target NAME span). `Client::prepare_type_hierarchy(..) ->
Vec<TypeTarget>` then `Client::supertypes(&TypeTarget)` / `Client::subtypes(&TypeTarget)`
walk `textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes`, `typeHierarchy/subtypes`
(LSP 3.17). A `TypeTarget` carries the server's verbatim item, `data` included, so only a server
mints one. `initialize` advertises `implementation.linkSupport` and `typeHierarchy` (no dynamic
registration) and keeps the server's `capabilities`; a method whose provider is absent, `null` or
`false` fails with `LspError::Unsupported { method, capability }` and sends nothing — never an
empty answer. Before a handshake nothing is known, so the request goes out. All four run under the
declared `call_timeout`, `query_deadline` and `query_retry` (§12.6); an empty result still carries
no readiness, so gate with `await_ready` first — and even past that gate an empty answer is not
proof of "none" while rust-analyzer is still loading its workspace (§12.5). Measured on the local servers: rust-analyzer
advertises implementation but NOT type hierarchy; gopls and clangd advertise both; pyright advertises
neither. Real-server proof: `tests/lsp_hierarchy.rs` (`BAGE_LSP_REAL_TEST=1`); a server that starts
advertising a capability fails its `Unsupported` row loudly so it moves to the positive rows.
