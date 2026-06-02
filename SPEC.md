# Båge — Specification

> Bidirectional code-graph round-trip file editor. Standalone IDE-style edit engine over
> files + LSP; in integrated mode, Hylla links Båge as a library so one agent-facing edit
> lands in both the graph and the files with no possible drift. This SPEC is the buildable
> contract; design rationale lives in `docs/adr/` and `CONTEXT.md`. Module:
> `github.com/hylla-io/bage`. Status: foundation drop. Date: 2026-06-01.

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
- **§1.5 Hexagonal.** Ports define boundaries; adapters implement them. Interface-first,
  dependency inversion, smallest concrete design.
- **§1.6 Gates.** Every package is TDD-built and green under `mage` (`testPkg`, `racePkg`,
  `vetPkg`, `formatFile`); the composite gate is `mage ci`. NEVER raw `go`/`gofmt`/`gofumpt`.

## 2. Scope

- **In:** byte-addressable Tier-1 text formats (code; markdown, toml, yaml, json, html, rst,
  …) — parsed via tree-sitter, round-trip editable by byte-range splice + LSP `WorkspaceEdit`.
- **Out:** non-byte-addressable formats (EPUB/ebooks, docx/OOXML, pdf, ipynb) — Hylla's
  read-only doc adapters own these; their nodes are not editable (`Position == nil`).

## 3. Architecture & module layout

```
cmd/bage/                 # standalone CLI entrypoint
internal/atomicwrite/     # atomic temp+rename+fsync file writer
internal/normalize/       # shared LF/BOM/trailing-ws normalization (MUST match Hylla)
internal/hashing/         # Hasher port (raw + normalized content hashes)  [xxHash adapter: dep-gated]
internal/locator/         # Locator interface + TextLocator + Edit/FileEdit
internal/parser/          # ParserPort interface + CST node DTOs (engine adapter: cgo, dep-gated)
internal/wal/             # edit-intent log (durable, file-based; NO SQLite)
internal/format/          # Formatter/Linter port + configured-command exec adapter + fake
internal/edit/            # single-region round-trip: drift-check (two hashes) → apply → reparse
internal/session/         # FILE-LEG two-phase Prepare/Commit/Rollback over multi-file edits + WAL
                          #   (the cross-store graph+file coordinator lives in HYLLA per ADR-0001)
internal/lsp/             # LSP client + lifecycle; rename → WorkspaceEdit → []FileEdit
```

Dependency direction: Hylla → Båge only. Båge imports nothing from Hylla.

## 4. Foundation ports & types (this drop)

### §4.1 `internal/atomicwrite`
- `func Write(path string, data []byte) error` — write to a temp file in the same dir,
  `fsync`, then `os.Rename` over the target. Clean up temp on error. POSIX-atomic.

### §4.2 `internal/normalize`
- `func Normalize(b []byte) []byte` — the shared rule, byte-identical with Hylla:
  1. normalize line endings to LF (drop `\r`);
  2. strip trailing horizontal whitespace per line: remove `[ \t\r]+` before each `\n` and at EOF;
  3. **LAST**, on the CR-free output, strip ALL consecutive leading UTF-8 BOMs (`EF BB BF`).
     Order matters: BOM-strip runs after `\r` removal and strips all leading BOMs, so neither a
     double BOM nor a `\r`-split BOM (`EF BB \r BF`) breaks idempotency (fuzz-enforced; Hylla MUST match).
- This is the input to the **normalized** hash only; raw byte ranges index the **raw** file.

### §4.3 `internal/hashing`
- `type Hasher interface { Sum(b []byte) string }` — hex digest of bytes.
- `func RawHash(h Hasher, raw []byte) string` — gates byte-offset validity.
- `func NormHash(h Hasher, raw []byte) string` — `h.Sum(normalize.Normalize(raw))`; drift
  classifier.
- MVP ships a stdlib adapter behind `Hasher`; the **xxHash64 adapter (`cespare/xxhash`, to
  match Hylla) lands once dependency-addition is unblocked**. The two-hash semantics are
  engine-independent.

### §4.4 `internal/locator`
- `type Edit struct { NewText string }`
- `type Locator interface { Path() string; Apply(e Edit) error; RawHash() string }`
- `type TextLocator struct { Path_ string; StartByte, EndByte int }` implementing `Locator`
  via `atomicwrite` (read file, splice `[StartByte:EndByte]`, write).
- `type FileEdit struct { Path string; StartByte, EndByte int; NewText string }` — the
  unit the coordinator/LSP path produces; multiple per file applied reverse-sorted by offset.

### §4.5 `internal/parser`
- `type ParserPort interface` (engine-agnostic contract; cgo adapter later):
  - `Parse(ctx, lang Lang, src []byte) (*Tree, error)`
  - `ParseIncremental(ctx, lang Lang, src []byte, old *Tree, edit InputEdit) (*Tree, error)`
  - `ChangedRanges(old, new *Tree) []ByteRange`
- DTOs: `Node{Kind string; StartByte, EndByte int; StartPoint, EndPoint Point; ...}`,
  `Point{Row, Col int}`, `ByteRange{Start, End int}`, `InputEdit{...}`, `Lang` enum.
- Interface + DTOs only this drop; the official-CGO `go-tree-sitter` adapter is a later,
  dependency-gated drop (ADR-0002).

### §4.6 `internal/wal`
- `type Intent struct { ID string; Edits []FileEdit; Originals map[string][]byte; ExpectedRawHash, ExpectedNormHash map[string]string }`
- `func Append(dir string, in Intent) error` / `func Replay(dir string) ([]Intent, error)`
  — durable, file-based (e.g. one fsynced JSON record per intent). NO SQLite.

## 5. Drift discipline (edit-time; later drops use it)

Before applying any locator: hash the live file (raw). `raw` match → byte range valid →
apply. `raw` mismatch + `norm` match → whitespace-only drift → re-ground (re-resolve range)
then apply. `norm` mismatch → real drift → re-ground from Hylla or **reject** (never slide).

## 6. Modes

- **Standalone:** files + LSP; no graph; same locator/edit engine.
- **Integrated:** Hylla's coordinator drives the two-phase saga (WAL → `bage.Prepare`
  [stage temp, fmt/lint, parse CST] → `graph.Prepare` → `bage.Commit` → `graph.Commit`),
  restore-on-handled-failure / converge-on-crash.

## 7. Non-goals (parked, named)

External-edit file watcher; shadow-graph interaction (Hylla's); MCP edit-tool naming;
OOXML/pandoc editing.

## 8. Region-anchored edit model + concurrency (ADR-0003)

The edit unit is **region-anchored**, not byte-only, so a model sends the fewest tokens,
mistakes reject instead of corrupting, and concurrent edits are lossless. This matches omp
(`can1357/oh-my-pi`) and improves on it by editing CST regions/"blocks", not whole files.

### §8.1 Edit input
- `type Region struct { Path string; StartByte, EndByte int; StartLine, EndLine int; StartCol, EndCol int; RegionHash string }` — mirrors Hylla's per-node locator bundle (`HYLLA_NODE_CONTRACT.md` §1) **minus graph identity** (no `parent_id`/`tail_symbol` — Båge is ID-blind).
- `type FileAnchor struct { Path, RawHash, NormHash string }` — the per-file gate (§2 of the contract).
- `type Edit struct { Region Region; NewText string }`. The model **echoes** a shown `RegionHash` (omp-style) — it never computes a hash or resends old text. Addressing is line-facing (model) / byte-internal; graph-mode uses `node_id` resolved Hylla-side to a `Region`.

### §8.2 Write contract (back to Hylla)
- `type EditResult struct { Path string; ChangedStart, ChangedEnd int; NewRegionHash, NewFileRawHash, NewFileNormHash string; NewStartLine, NewEndLine int }`.
- `Commit` returns `[]EditResult` so Hylla incrementally re-ingests **only** the changed region.

### §8.3 Concurrency (per ADR-0003)
- **Per-file serialization** (one writer per file); cross-file edits run parallel.
- **Resolve the locator under the file lock, immediately before applying**, so every edit sees prior concurrent commits (no lost update).
- `region_hash` matches at a shifted offset = **benign shift** → re-resolve (graph re-ingest / file-mode reparse-and-match) → apply. `region_hash` no longer matches = **conflict** → hard reject.
- After each apply: incremental tree-sitter reparse + LSP `didChange`; the CST/graph is the exact relocator (omp uses a heuristic snapshot replay — kept only as the file-mode fallback). **Layer note:** the FILE-LEG session does not cache/reuse a tree or push `didChange` — `region.Resolve` reparses the live file fresh **under the per-file lock** on every relocation, which is correctness-equivalent (no stale tree is ever trusted); the *incremental* tree reuse + `didChange` propagation belong to the integrated coordinator/LSP layer. The per-region `region_hash` is the live gate; the per-file `FileAnchor` hashes are informational (subsumed by the per-region resolve in the file leg, but carried for Hylla's whole-file fast-path).

### §8.4 Hard errors (never silent)
tree-sitter parse failure → reject (always); configured lint failure → reject; fmt → applied; region_hash unresolvable → reject.

### §8.5 omp parity proof
Snapshot = live file + region_hash; replay = reparse-and-match-by-hash; reject = conflict/ambiguity. Falsifiable tests MUST include: concurrent same-file edits (benign-shift re-resolves, conflict rejects, no lost update), cross-file parallel, and reject-not-corrupt on every drift class.

### §8.6 Fuzz-enforced invariants
Two properties are **fuzz-enforced** (`mage fuzz`), not merely table-tested:
1. **Normalize idempotency** (`internal/normalize` `FuzzNormalizeIdempotent`): `Normalize(Normalize(b)) == Normalize(b)` for arbitrary bytes. Holds **only because** BOM-stripping runs LAST on the CR-free output and strips ALL leading BOMs — a `\r`-split BOM (`EF BB \r BF`) collapses to a BOM under `\r` removal and is then stripped (§4.2). Hylla MUST reproduce this exact order so `NormHash` agrees cross-system.
2. **Text-fallback losslessness** (`internal/parser/treesitter` `FuzzTextFallbackLossless`): `Parse(LangText, src)` preserves `Source` byte-for-byte and spans `[0,len)` for any bytes (binary, multibyte UTF-8, lone CRs, BOMs) — the property that lets an agent IDE open ANY file without corruption.

## 9. Agent IDE surface & file-type coverage

Båge presents a uniform, file-type-agnostic editing surface so an agent can open, inspect, and edit **any** file.

### §9.1 `parser.LangForPath` — total language selection
`func LangForPath(path string) Lang` maps a path to a `Lang` by extension (case-insensitive) plus a few basenames (`Makefile`/`GNUmakefile`/`.mk` → `LangMakefile`; `Dockerfile`/`Containerfile`/`Justfile` → `LangText`). It **never returns `LangUnknown`** — unknown extensions, dotfiles (`.env`, `.gitignore`), extensionless and empty paths resolve to `LangText`. Every file is therefore at least text-editable (table- + fuzz-enforced, `TestLangForPath`). `session.Session.Lang` is now an **optional override**: `LangUnknown` (the zero value) means auto-detect per file via `LangForPath`; a set value forces that language for every file. `bage.Open` no longer requires a `Lang`.

### §9.2 `LangText` text-fallback contract
Under `LangText` the adapter builds a `document` root spanning the whole file with **`Tree.Native == nil`** and one named `line` child per source line (each line keeps its terminating `\n`, so concatenating children reproduces the source EXACTLY). `Tree.Source` is the input bytes verbatim — a byte-range splice + reparse is lossless for any bytes. Because `Native` is nil, `Tree.Close` is a no-op and `ChangedRanges` degrades to `nil` (full reparse). This is the contract that makes the grammar-less open→edit→write path corruption-proof.

### §9.3 Grammar + LSP coverage matrix
- **20 tree-sitter grammars** (real parse + round-trip fixtures, `TestParsePolyglot`): Go, TypeScript, TSX, JavaScript, Python, Rust, Java, C, C++, C#, Ruby, JSON, HTML, CSS, YAML, TOML, XML, Makefile, Bash, **Markdown**.
- **Text fallback** (lossless, no grammar binding): MDX, SCSS, Dockerfile, Swift-source, `.txt`, dotfiles, and any unknown type.
- **LSP rename availability VARIES by design** — it is an accelerator, not a precondition (the graph/LLM path covers what an LSP can't):
  - **Proven via live containerized rename** (`mage lsp`, 10 rows through one socat-TCP bridge; gopls is the native-TCP exception): Go, Python, TypeScript, TSX, JavaScript, JSX, Rust, C, C++, **Swift** (sourcekit-lsp, local rename, no index build).
  - **Documented extension seam, not yet active** (`lspServerCase` rows defined, container hardening pending): C# (csharp-ls), Java (jdtls).
  - **No LSP rename by design**: Ruby (ruby-lsp has no functional rename — grammar parses, rename absent) and all data/markup/build/script types and `LangText`.

### §9.4 Inspect surface (`pkg/bage`)
- `OpenFile(ctx, path) (*OpenedFile, error)` — read + `LangForPath` + parse; `Close()` frees the native tree.
- `Outline(tree) []Symbol` — documentSymbol-like listing: named declaration nodes (grammar-agnostic, by node kind) with byte + 1-based line ranges; for the text fallback (`Native==nil`) it returns one `line` Symbol per source line.
- `LangForPath` is re-exported on the facade so callers branch on language without importing `internal/*`.
