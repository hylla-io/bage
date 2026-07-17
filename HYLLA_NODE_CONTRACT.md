# Hylla ↔ Båge Node Contract

> **Purpose.** This file is the load-bearing interface contract between **Hylla**
> (the polyglot code-knowledge-graph on DGraph) and **Båge** (this project — the
> bidirectional code-graph + round-trip file editor). It enumerates the per-node
> and per-file information Båge needs Hylla to persist so Båge can locate, display,
> drift-check, and round-trip-edit the underlying source files **without re-parsing
> every file itself**.
>
> Authoritative on the Hylla side: `hylla/polyglot-foundation/SPEC.md` §2.10 (node
> shape), §4.2 (type families + per-language extensions), §6 (polyglot tree-sitter
> ingestion). This doc is the Båge-side statement of what Båge consumes; the Hylla
> SPEC amendment + node-locator-contract drop are the Hylla-side commitment to
> provide it.
>
> Date: 2026-05-27; ratified + revised 2026-06-01 (two-hash drift gate, in-process `StoragePort` read path, official CGO tree-sitter). Status: RATIFIED — absorbed into Hylla SPEC §6.7.5–§6.7.7; the Hylla node-locator-contract drop is `drop_020`.

---

## 0. Mental model (shared invariant)

> **Files are the source of truth. Hylla nodes are projections + locators.
> Båge node identity = Hylla node ID. The byte range is a volatile locator,
> valid only while the file still hashes to the stored content fingerprint.**

This mirrors Båge's own core invariant (`code_graph_architecture.md` §1, §3) and is
reinforced by the oh-my-pi (`can1357/oh-my-pi`) hash-anchored-edit research
(2026-05-27): oh-my-pi addresses edits by line number but validates every edit
against a whole-file content tag, replays against a cached snapshot on drift with
`fuzzFactor=0` (exact-alignment-only), and rejects rather than corrupts on
unrecoverable drift. We adopt the **same drift discipline** at the graph layer:

- **Stable identity** = Hylla node ID (qualified symbol / structural path). Never the byte range.
- **Volatile locator** = `(file_raw_hash, byte_range)`. The byte range is trustworthy *iff* the live file still hashes (RAW bytes) to `file_raw_hash`. Byte offsets cannot survive whitespace normalization, so the offset gate hashes raw bytes; a second `file_norm_hash` (normalized) classifies whitespace-only vs real drift (§2, §4).
- **On drift** = Båge does NOT blindly apply offsets. It forces a re-ground (re-read from Hylla after Hylla re-ingests the changed file), or rebases via exact-alignment replay, or rejects. Never silently slides an edit onto the wrong region.

oh-my-pi's lesson, restated: **do not make the volatile locator the identity.** The
per-line-content-hash scheme some agents use (e.g. the third-party
`RimuruW/pi-hashline-edit` extension) is one way to anchor; we prefer
stable-node-ID + (file-hash, byte-range) because Hylla already has stable qualified
IDs and a graph.

---

## 1. Per-node fields Hylla MUST persist

These are populated for **code.\*** nodes AND **Tier-1 doc.\*** nodes (Markdown,
TOML, YAML, JSON, LaTeX, Typst, RST, AsciiDoc, Org, XML — anything tree-sitter
parses as plain text with meaningful byte offsets). They are **universal-optional**:
the field group is a nullable/pointer bundle on the node, `nil` for nodes that have
no file-located CST region (concept/paper nodes; OOXML/pandoc doc nodes — see §3).

| Field | Type | Båge use | Hylla home (recommended) |
|---|---|---|---|
| `file_path` | string | `Locator.Path` — which file the region lives in | `NodeCore` (universal) |
| `start_byte` | int | `TextLocator.StartByte` — the locator anchor | `NodeCore.Position` |
| `end_byte` | int | `TextLocator.EndByte` | `NodeCore.Position` |
| `start_line` | int (1-indexed) | display + LSP `Range.start.line` | `NodeCore.Position` |
| `end_line` | int (1-indexed) | display + LSP `Range.end.line` | `NodeCore.Position` |
| `start_col` | int (0-indexed, UTF-8 byte-col) | LSP `Range.start.character` (convert to UTF-16 at LSP boundary) | `NodeCore.Position` |
| `end_col` | int (0-indexed, UTF-8 byte-col) | LSP `Range.end.character` | `NodeCore.Position` |
| `region_hash` | string (hex) | per-node drift check (Båge `Node.Hash`) | `NodeCore` (universal) |
| `parent_id` | string (node ID) | containment / `Metadata{parent}` | `NodeCore` (universal) |
| `tail_symbol` | string | leaf symbol name / `Metadata{kind-name}` | `NodeCore` (universal) |
| `content` | string | `DisplayText` source — raw region text | `NodeCore` (**already persisted today**) |
| `signature` | string | function/method signature for display | `CodeNodeExt` (code-only) |
| `visibility` | enum | public/private/protected/internal/package/unknown | `CodeNodeExt` (code-only; promote to universal-code) |

Notes:
- **`content` already exists** in Hylla's current schema (`summary`, `content`, `docstring` are universal predicates today). Båge uses the raw region `content`, NOT the LLM `summary`, as the round-trip display/edit text.
- **Byte range is authoritative**; line/col are derived conveniences. Tree-sitter provides all of them for free on every node (`start_byte`/`end_byte`/`start_point`/`end_point`), so populating them costs nothing at parse time.

---

## 2. Per-FILE field Hylla MUST persist (the drift gate)

| Field | Type | Båge use | Hylla home |
|---|---|---|---|
| `file_raw_hash` | string (hex) | RAW-byte whole-file tag — the AUTHORITATIVE byte-offset-validity gate | File/Module node predicate (or denormalized onto every node from the file) |
| `file_norm_hash` | string (hex) | normalized whole-file tag — drift CLASSIFIER (whitespace-only vs real) | same |

Two file-level hashes (dev decision 2026-06-01, replaces the single `file_content_hash`).
Before Båge trusts any node's byte range, it hashes the live file's **RAW** bytes and
compares to `file_raw_hash`: match → every byte range from that file is valid; mismatch
→ compare `file_norm_hash` (normalized): norm-match ⇒ whitespace-only drift ⇒ cheap
re-resolve from the graph; norm-mismatch ⇒ real content drift ⇒ re-ground from Hylla
(after re-ingest) or reject. A single normalized hash CANNOT gate byte offsets — strip
one trailing space and every later offset shifts — which is why the gate hashes raw
bytes. This is oh-my-pi's `¶PATH#TAG` snapshot-tag pattern lifted to the graph layer and
split for normalization-immunity.

Recommended storage: on the file-or-module-level node. If Hylla has no per-file node
kind yet, denormalize `file_content_hash` onto every node carrying that `file_path`
(so a node read alone is self-validating).

---

## 3. What does NOT get CST positional (and that's fine)

Per dev decision (2026-05-27), non-coding/non-Tier-1-text formats legitimately skip
clean CST positional info. The `Position` bundle is `nil` for these:

- **OOXML** (`.docx`, `.xlsx`, `.pptx`): ZIP+XML containers, no byte-offset locator. If
  ever round-tripped, they use a different locator (Båge's `DocxLocator{ParagraphID,
  RunIndex, CharRange}` keyed on stable `w14:paraId`), NOT byte ranges. **Out of scope
  for now** — ingest read-only.
- **markitdown-ingested docs**: Microsoft `markitdown` (researched 2026-05-27) is a
  **one-way** docx/office/pdf → Markdown extractor (mammoth → HTML → markdownify). It
  preserves heading/list/table structure but **zero OOXML locators** (no `w14:paraId`,
  run indices, or offsets) and has **no reverse path** (issue #1341 requests exactly
  the round-trip workflow, unanswered). So markitdown is a fine **read-only ingest**
  helper to give Hylla/LLMs a clean Markdown view of a docx, but it contributes
  **nothing** to write-back and is orthogonal to the byte-range locator contract.
- **Pandoc-mediated formats** (EPUB, ipynb, …): lossy AST; use Båge's `PandocLocator`
  + `OriginalContent` blob, not byte ranges. Out of scope for now.
- **concept.\* / paper.\* nodes**: pure metadata, no file region — `Position` nil.

**Rule:** `Position != nil` ⟺ the node is a tree-sitter-parsed Tier-1 code-or-text
region with valid byte offsets. Everything else carries `Position == nil` and is not
byte-range-addressable.

---

## 4. Shared normalization rule (LOAD-BEARING — both sides MUST match)

Hylla and Båge MUST compute the NORMALIZED hashes (`file_norm_hash`, `region_hash`)
**identically**, or every drift check is a false positive. `file_raw_hash` hashes the
RAW file bytes with **NO normalization** (normalizing it would defeat the byte-offset
gate). Adopt oh-my-pi's normalization verbatim for the normalized hashes:

1. Normalize line endings to **LF** (strip `\r`).
2. Strip trailing horizontal whitespace per line (see step 3 below).
3. **LAST**, on the CR-free output, strip **ALL consecutive leading BOMs** (`EF BB BF`).
   Order is load-bearing: BOM stripping MUST run *after* `\r` removal and strip *all* leading
   BOMs — otherwise (a) two BOMs, or (b) a `\r` embedded in a BOM run (`EF BB \r BF`, which
   `\r` removal collapses back into a BOM) make the rule non-idempotent, and Båge vs Hylla
   would compute different `region_hash`/`file_norm_hash` for the same file. Båge
   regression-fuzzes both cases: `FuzzNormalizeIdempotent`. **Hylla MUST match this order.**
3. Strip trailing horizontal whitespace per line: `replace /[ \t\r]+(?=\n|$)/ → ""`
   (so display-trimming and CRLF/LF differences don't trigger false drift).
4. Hash with **xxHash64** via `github.com/cespare/xxhash/v2` (`Sum64`), encoding every
   digest as **16-character zero-padded lowercase hex** — exactly `fmt.Sprintf("%016x",
   sum)`. This precise string encoding (width 16, zero-padded, lowercase), NOT merely
   "64-bit", IS the contract: a raw `uint64`, big-endian bytes, or unpadded hex would all
   satisfy "64-bit" yet produce non-matching strings. Applies to all three:
   `file_raw_hash` over the raw bytes, `file_norm_hash` + `region_hash` over the
   normalized bytes. Båge's canonical implementation is `internal/hashing.XXHasher`.

The hash is a **coarse integrity check backed by re-read on mismatch**, NOT a unique
locator — collisions degrade to "force re-read," never to corruption. Document the
exact algorithm + width + normalization on the Hylla schema predicate so the Båge
side can replicate it bit-for-bit.

---

## 5. Read path — Båge ↔ Hylla (RESOLVED 2026-06-01)

**Neither A (direct DGraph) nor B (MCP).** Integrated Båge reads the graph via an
**in-process port call into Hylla's `StoragePort`** — Hylla statically links Båge as a
Go library (Hylla → Båge, never the reverse) and the dual-write coordinator translates
node-ID ↔ locator at the boundary. (Standalone Båge runs with no graph at all.)

**Consequence:** Hylla's `internal/adapters/storage/dgraph/schema.dql` predicate names
are **NOT a frozen cross-repo API** — they stay internal to Hylla and may evolve. The
contract Båge depends on is the locator/hash **DTO shape** crossing the `StoragePort`
boundary (§1, §2, §4), not the DGraph predicate spelling. See Hylla SPEC §6.7.6.

---

## 6. Incremental re-ingest (architectural consequence)

Because Båge edits files, byte offsets go stale the instant a file changes. The full
contract requires Hylla to support **incremental re-ingest** keyed on file-change:

- tree-sitter `Tree.Edit(...)` + `Parser.Parse(oldTree, newSource)` reuses unchanged
  subtrees; `tree.ChangedRanges(oldTree)` names which nodes need refreshing.
- Hylla refreshes only overlapping nodes' `(start_byte, end_byte, region_hash)` + the
  file's `file_content_hash`, rather than re-ingesting the whole artifact.
- This mirrors oh-my-pi's "renumber + re-tag after every edit" loop.

Hylla's ingest today is whole-artifact. Incremental re-ingest is **a dependency of the
GDD dual-write coordinator** (promoted from fast-follow, dev decision 2026-06-01 — the
dual-write tool cannot ship without it), but the **node-field contract (§1, §2, §4)
must exist first** so there is a schema for the incremental path to refresh.
Recommended sequencing: ship the field contract now (parser-independent — provable via
a write→store→read round-trip test with no parser); incremental re-ingest lands with
the coordinator drop. See Hylla SPEC §6.7.5 sequencing + §6.7.6.

---

## 7. Implementation sequencing (Hylla side)

The node-locator-contract is a **parser-independent data-contract drop** — it can be
fully built and tested before any tree-sitter parser adapter ships:

1. **DTO** (`internal/ports/storage.go`): add `core.Position` struct +
   `NodeCore.{FilePath, RegionHash, ParentID, TailSymbol, Position *core.Position}` +
   `CodeNodeExt.{Signature, Visibility}` + `file_content_hash` placement + `Validate`
   rules (e.g. `Position != nil` requires `file_path != ""`).
2. **Schema** (`internal/adapters/storage/dgraph/schema.dql`): add the predicates +
   add them to the `Node` type block; index the ones Båge queries by (`file_path
   @index(hash)`, `tail_symbol @index(term)`, `parent_id @index(hash)`).
3. **Adapter** (`node.go buildNodeDoc` project + `query.go jsonToNode` decode): carry
   every field through both directions.
4. **Round-trip test** (FakeStorage + testcontainers): write a Node with a populated
   `Position` + hashes, read it back, assert equality. Proves the contract persists
   with **no parser involved**.
5. **SPEC amendment** (Hylla `SPEC.md`): promote the locator set (positional +
   file_path + region_hash + file_content_hash + parent_id + tail_symbol) to a
   universal **Tier-1 locator contract** that every tree-sitter Tier-1 adapter MUST
   populate. Promote `visibility` to a universal-code enum. Document the normalization
   rule (§4). Falsifiable: a Tier-1 adapter emitting a code/markup node with `nil`
   Position, or hashing with a different normalization.

Later parser-adapter drops POPULATE `Position` from tree-sitter's free
`StartPoint`/`EndPoint`/`StartByte`/`EndByte` — produced via **Båge's `ParserPort`**
(Hylla consumes the port; it does not own a second tree-sitter parser) — the contract
is their target, not their prerequisite.

---

## 8. Quick reference — Båge's locator types ↔ Hylla fields

| Båge locator (from `code_graph_architecture.md` §3) | Hylla fields | Status |
|---|---|---|
| `TextLocator{Path, StartByte, EndByte}` | `file_path`, `start_byte`, `end_byte` | **add (contract drop)** |
| `Node.Hash` (drift) | `region_hash` + `file_raw_hash` + `file_norm_hash` | **add (contract drop)** |
| `Node.DisplayText` | `content` (raw region) | ✓ already persisted |
| `Node.Metadata{language, kind, parent}` | `node_language`, `kind`, `parent_id` | language+kind ✓; `parent_id` **add** |
| `DocxLocator` / `XlsxLocator` / `PptxLocator` | — (OOXML, out of scope) | n/a |
| `PandocLocator` + `OriginalContent` | — (pandoc, out of scope) | n/a |

---

## 8b. Tier-2 analysis-substrate sites (opt-in, added 2026-07 — DL-45)

Beyond the tier-1 **declaration outline** (§1–§8, the stable floor Hylla anchors nodes on) Båge exposes an **opt-in tier-2 substrate**: control-flow and resource *sites* that Hylla's tier-2 graph consumes to reason about resource lifetimes (e.g. "opened here, never released before this return"). It is a SEPARATE surface — `tier2_sites(opened) -> Vec<Site>`, parallel to `read_blocks` — so the tier-1 outline contract stays byte-identical (regression-locked).

**Identity is DISPOSABLE.** Sites are re-derived on every parse and are NOT anchored across edits. Overlap is expected and intentional: a Go `defer f.Close()` is BOTH a `Defer` and a `ResourceRelease` site.

### `Site` fields

| Field | Type | Meaning |
|---|---|---|
| `family` | `Tier2Family` (enum, snake_case on the wire) | classified signal (table below) |
| `kind` | string | grammar node kind that produced the site (`call_expression`, `return_expression`, …) |
| `name` | string | best-effort call target (`Box::new`, `os.Open`); empty for bare statement forms |
| `start_byte` / `end_byte` | int | half-open byte range of the site node |
| `start_line` / `end_line` | int | 1-based line range |
| `region_hash` | string | xxHash64 `{:016x}` over the NORMALIZED range bytes — **byte-identical to §4 `region_hash`** (`region::hash_region`), so a host may correlate a site with a stored node |
| `content` | string | the exact source slice for the byte range |

### `Tier2Family` taxonomy

`Allocation` · `ResourceAcquisition` · `ResourceRelease` · `EarlyReturn` · `Panic` · `Defer` · `Spawn` · `LockAcquire` · `LockRelease`.

### Per-language coverage (first-drop set)

Only these languages carry classification tables; **every other `Lang` (and the grammar-free text fallback) returns an empty vec — a valid opt-out, never an error.** Absence of a family in a language is silent (e.g. Rust has no explicit `Defer`; lock release is RAII, not a call).

| Family | Rust | Go | Python | TS/TSX/JS |
|---|---|---|---|---|
| Allocation | `Box/Rc/Arc/Vec::{new,with_capacity}` | `make(…)` / `new(…)` | — | `new X(…)` |
| ResourceAcquisition | `File::open` / `*::{open,create}` | `os.Open`/`*.{Open,Create,OpenFile,Dial,Listen}` | `open(…)` | `*.{open,openSync,createReadStream,createWriteStream}` |
| ResourceRelease | `drop(…)` / `*.close` | `*.Close` / `close(…)` | `*.close` | `*.close` |
| EarlyReturn | `return_expression` | `return_statement` | `return_statement` | `return_statement` |
| Panic | `panic!`/`unreachable!`/`todo!`/`unimplemented!` | `panic(…)` | `raise` | `throw` |
| Defer | — | `defer_statement` | — | — |
| Spawn | `tokio::spawn` / `thread::spawn` / `*.spawn` | `go_statement` | `*.create_task` | `*.{spawn,fork}` |
| LockAcquire | `*.{lock,try_lock}` | `*.{Lock,RLock,TryLock}` | `*.acquire` | `*.lock` |
| LockRelease | — (RAII) | `*.{Unlock,RUnlock}` | `*.release` | `*.unlock` |

Classification is grammar-kind + call-target based and **never guessed**: an unrecognized call target is simply not a site (no coerced family). The table is intentionally conservative — false negatives (a missed site) over false positives (a wrong family). Call SHAPE is part of the match — the separator matters: Rust `*::{open,create}` (path) vs `*.close`/`*.lock` (dot) vs bare `drop`; Go dot-qualified capitalized methods vs bare builtins (`close`/`make`/`panic`); Python `open` is BARE-only (`x.open`/`webbrowser.open` do not classify); all TS/JS call sites are dot-qualified.

**Emitted-site invariant:** a site whose byte range is out of bounds **or is not valid UTF-8 is SKIPPED wholesale** (never emitted with an empty or lossy `content`). For every site that IS emitted, `content` is byte-for-byte the source slice and `region_hash` is computed over that same validated range.

## 8c. Import/export FACTS (opt-in, added 2026-07 — XB1)

A THIRD sibling read-only surface over an `OpenedFile`, parallel to §8b: structured **cross-file linkage facts** Hylla joins into edges (module A imports symbol S from module B; module B re-exports S). Entrypoint `extract_facts(opened) -> Facts`, distinct from `read_blocks`/`tier2_sites`, so the tier-1 outline contract stays byte-identical. Facts are DISPOSABLE (re-derived per parse, never anchored). Extraction uses **tree-sitter QUERIES** to locate import/export entry nodes at any depth (a `pub use` inside a `mod` is found), then field-addressed native-node walking pulls the typed parts — no text scanning, no cross-language leakage.

**NEVER-GUESS (typed opt-out):** `Facts` is an enum — `Supported { imports, exports }` OR `Unsupported { lang }`. A language with no query set returns `Unsupported` (typed, callers MUST branch); an unrecognized construct WITHIN a supported language is silently skipped, never coerced into a wrong-shaped fact. An empty `Supported` (a supported file with no imports/exports) is DISTINCT from `Unsupported`.

### `ImportFact` fields

| Field | Type | Meaning |
|---|---|---|
| `source_specifier` | string | source module path/string the names come from (quotes stripped) |
| `items` | `[ImportedItem]` | named symbols; **empty** for a bare module / side-effect / glob import |
| `glob` | bool | wildcard/dot import bringing all names into scope (`use a::*`, `from a import *`, Go `. "pkg"`, TS `* as ns`) |
| `re_export` | bool | this import is ALSO a public re-export (Rust `pub use`; TS `export … from` is modeled as an `ExportFact`, not here) |

`ImportedItem { name, alias: Option }` — `name` is the symbol at the SOURCE (`HashMap`, `default` for a TS default import, `*` for a namespace/dot bind); `alias` is the LOCAL rebinding when renamed.

### `ExportFact` fields

| Field | Type | Meaning |
|---|---|---|
| `name` | string | exported symbol (`*` glob re-export, `default` default export) |
| `kind` | `ExportKind` (enum, snake_case) | `function`/`class`/`struct`/`enum`/`trait`/`interface`/`type_alias`/`const`/`variable`/`module`/`reexport`/`default`/`other` |
| `re_exported_from` | string? | source module when the export forwards another module's symbol (Rust `pub use`, TS `export … from`) |
| `alias` | string? | exported-as name when renamed (`export { x as y }`) |

`ExportKind::other` = a named export whose underlying kind is not resolvable from syntax alone (e.g. TS `export { a }` of a local binding) — unspecified, never guessed.

### Per-language coverage matrix (first-drop set)

Only Rust, TS/TSX/JS, Python, and Go carry query sets; **every other `Lang` (and text fallback) → `Unsupported`.** Python and Go have no syntactic export construct → `exports` is always empty (imports only).

| Construct | Rust | TS/TSX/JS | Python | Go |
|---|---|---|---|---|
| Imports | `use` (paths / `{brace}` incl. NESTED `{a::{b,c}}` flattened to one item per leaf, leaf `name` path-qualified below the group source / `as` alias / `*` glob; nested-in-group glob → item whose `name` ends in `*` e.g. `sync::*`, fact `glob` stays false; leading `::` crate-root anchor stripped from `source_specifier`) | `import` default / `{named}` / `* as ns` / side-effect | `import` / `import … as` / `from … import …/as/*` / relative | `import "x"` / block / `alias "x"` / `. "x"` (glob) / `_ "x"` |
| Re-export (import side) | `pub use` → `re_export=true` | — (modeled as export) | — | — |
| Named exports | `pub` fn/struct/enum/trait/const/static/mod/type/union | `export {…}`, `export const/function/class/interface/type/enum` | — | — |
| Re-export (export side) | `pub use` → `ExportFact{reexport, re_exported_from}` | `export {…} from`, `export * from` | — | — |
| Default export | — | `export default …` | — | — |

Rust restricted visibilities (`pub(crate)`, `pub(super)`) are NOT public exports (only a bare `pub` exports). Fact PAYLOADS (`ImportFact`/`ExportFact`/`ImportedItem`/`ExportKind`) are serde-friendly for golden fixtures; the outer `Facts` result carries a typed `Lang` and is a live branch, not a wire type.

## 8d. Scope + binding resolution substrate (opt-in, added 2026-07 — XB2)

A FOURTH sibling read-only surface over an `OpenedFile`, parallel to §8b/§8c: the lexical **scope tree**, the local **bindings** each scope introduces, and every value-identifier **occurrence** tagged with its enclosing scope and a typed **resolution**. Hylla joins occurrences to their binding/import to draw use→def edges without an LSP. Entrypoint `extract_scopes(opened) -> Scopes`, distinct from `read_blocks`/`tier2_sites`/`extract_facts`, so the tier-1 outline contract stays byte-identical. Scopes are DISPOSABLE (re-derived per parse over the SAME bytes, never anchored). Extraction is a scope-aware native-tree walk (arena of scopes, parent links), then a second pass resolves each occurrence.

**NEVER-GUESS (RISKY unit):** resolution is `Undecidable` unless a lexical fact is certain. The governing rule: a DEFINITE variant (`LocalBinding`/`ImportedName`) is emitted ONLY when structurally certain; every residual ambiguity degrades to `Undecidable` — a **false `Undecidable` is acceptable, a false definite is the bug class.** Local bindings ALWAYS win over imports (shadowing): an inner `let x`/`const x`/`x :=` masking an imported `x` resolves `LocalBinding`. A **glob/wildcard import contributes NO specific names** (its members are unknown) — a name reachable only through a glob stays `Undecidable`, never coerced to `ImportedName`. `Scopes` is an enum — `Supported(ScopeTree)` OR `Unsupported { lang }` (typed opt-out; callers MUST branch).

**POSITION (sequential bindings):** a `Binding` carries `hoisted: bool`. SEQUENTIAL bindings (`hoisted = false` — Rust `let`, Go `:=`, TS `let`/`const`) are visible ONLY at/after their own `start_byte`, so a same-name use lexically BEFORE the declaration falls through to an outer scope / import (never a false `LocalBinding` for a pre-declaration use). HOISTED bindings (`hoisted = true` — item/fn/struct/class/`type` names, parameters, JS `var`/function declarations, pattern binds) are position-free (visible everywhere in their scope).

### `Scopes` / `ScopeTree` shape

| Type | Field | Meaning |
|---|---|---|
| `ScopeTree` | `scopes: Vec<Scope>` | Arena; `scopes[0]` = module root; `parent` links form the tree. |
| `ScopeTree` | `occurrences: Vec<Occurrence>` | Every value-identifier occurrence, source order. |
| `Scope` | `parent: Option<usize>` | Enclosing scope arena index; `None` for module root only. |
| `Scope` | `kind: ScopeKind` | `Module` (file / Rust `mod` / Python `class`), `Function` (params bind here), `Block` (Rust/Go/TS brace; Python emits none). |
| `Scope` | `start/end_byte`, `start/end_line` | Span of the scope-introducing node. |
| `Scope` | `bindings: Vec<Binding>` | Local names introduced directly in this scope. |
| `Binding` | `name`, `start_byte`, `end_byte` | Bound name + byte range of the binding IDENTIFIER. |
| `Binding` | `hoisted: bool` | `true` = position-free (items/params/`var`/pattern binds); `false` = SEQUENTIAL, visible only at/after `start_byte` (`let`/`const`/`:=`). |
| `Occurrence` | `name`, `scope`, `start/end_byte`, `line` | Identifier text + enclosing scope index + position. |
| `Occurrence` | `resolution: Resolution` | `LocalBinding { scope }` \| `ImportedName` \| `Undecidable`. Exactly ONE variant per occurrence (locked property). |

### Per-language coverage (first-drop set)

Only Rust, TS/TSX/JS, Python, and Go carry a scope model; **every other `Lang` (and text fallback) → `Unsupported`.** Binding recognition is precise for Rust (the shadowing acceptance language) and best-effort elsewhere; an unrecognized construct is SKIPPED, never a fabricated binding.

| Aspect | Rust | TS/TSX/JS | Python | Go |
|---|---|---|---|---|
| Scope openers | `mod`(Module) / `fn`,closure(Function) / `block`,`match_arm`,`for`/`if`/`while`-expr(Block) | fn/arrow/method(Function) / `statement_block`,`class_body`(Block) | fn/`lambda`(Function) / list/set/dict-comprehension,`generator_expression`(Function) / `class`(Module); NO block scope | fn/method/`func_literal`(Function) / `block`(Block) |
| Name binds (enclosing) | `fn`/`struct`/`enum`/`trait`/`const`/`static`/`type`/`mod`/`union` name | fn/generator/class decl name (HOISTs to nearest fn/module) | `def`/`class` name | fn/method/`type_spec` name |
| Locals (current scope) | `let` PATTERN idents through the exhaustive Rust pattern collector (SEQUENTIAL) — `let E(v)` binds `v` not the ctor path `E`, `let P { y, .. }` binds shorthand `y` | `let`/`const` `variable_declarator` name — incl. object/array DESTRUCTURING (shorthand + rest + `default` left + `pair` value) (SEQUENTIAL); `var` (HOISTS to nearest fn/module) | `assignment`/`for`/`for_in_clause` left + `except … as` / `with … as` alias, through the exhaustive Python target collector (minus `global`/`nonlocal` names); `attribute`/`subscript` targets (`obj.x=`, `d[k]=`) bind NOTHING; bare `except X:` type is a use | `:=` left, `var`/`const` spec, `switch x := v.(type)` alias, `for i := range` left (only `:=`, not `=`) through the exhaustive Go target collector (SEQUENTIAL; excl. type/value, selector/index targets) |
| Pattern binds (arm/body scope) | `match_arm` / `for` / `if let` / `while let` pattern idents — EXHAUSTIVE vs the `pattern` supertype incl. `shorthand_field_identifier` (G1); constructor `type` path + `match_pattern` guard `condition` excluded | — | comprehension `for_in_clause` target | — |
| Params (function scope) | `parameter` `pattern` idents + untyped COMPOSITE closure params (bare `tuple`/`struct`/… children of `closure_parameters`, G2) | `required/optional_parameter` `pattern`, bare `identifier`, + object/array DESTRUCTURING params (shorthand/rest/default-left/pair-value, G3) | idents / `typed`/`default`/`*`/`**` params | `parameter_declaration` name idents (excl. type) in `receiver` + `parameters` + NAMED `result` lists (G4) |

Occurrences are all `identifier` nodes (binding sites included — a binding site resolves to its own `LocalBinding`), EXCEPT non-referencing Python positions that are DROPPED so they never coerce a false `ImportedName`: an `attribute` field (`obj.json` → `json`), a `keyword_argument` name (`f(json=1)` → `json`), and `global`/`nonlocal` declaration names. A Rust `if let` binding reaches only the consequence block — the `else`/`else if` alternative is walked under the enclosing scope, so the pattern name does NOT leak into the else arm. A Python `class` body opens a scope whose bindings do NOT enter a nested method's chain: a method's `Function` scope is re-parented to the nearest NON-class ancestor (module / enclosing function), so class vars need `self.`/`ClassName.` and never resolve `LocalBinding` inside a method (G5). All `Scopes` payloads (`ScopeTree`/`Scope`/`Binding`/`Occurrence`/`ScopeKind`/`Resolution`) are serde-friendly for golden fixtures; the outer `Scopes` result carries a typed `Lang` and is a live branch, not a wire type. `Scope.poison` (the G7 backstop) is resolution-only, `#[serde(skip)]` — NOT part of the wire contract.

### G7 unknown-pattern poison backstop

The per-grammar binding-leaf/scope tables are EXHAUSTIVE against each vendored grammar's `node-types.json` (Rust 0.24.2, TS/JS 0.23.2/0.25.0, Python/Go 0.25.0). Any pattern-position node kind OUTSIDE its table (macro-expanded Rust patterns `c!(z)`, Python `match`/`case` structural patterns, walrus `:=` targets, and any FUTURE grammar addition) is UNCLASSIFIABLE — its contained identifier names are POISONED: every occurrence of such a name in that scope degrades to `Undecidable`, winning over both a same-scope binding and any outer import. This converts every future coverage gap from a false-definite (the bug class) into a conservative `Undecidable` (acceptable).

**Bypass-proof — SCOPED (XB2 round-4, H0 + J0).** There is NO naive "harvest every identifier as a binding" collector anymore — it was deleted. EVERY binding position (Rust `let`/pattern-scope/params, TS/JS declarators/params/`catch`/named-expr self-name, Python assign/for/`with`/`except`/params/PEP-695 type-params+alias, Go `:=`/range/type-switch/receive/spec) routes through its grammar's EXHAUSTIVE per-kind collector, each of which ends in the poison fallback above. A binding position therefore cannot reach the naive harvest even by mistake — the symbol does not exist, so a stray call is a COMPILE error. This closes the FALSE-`LocalBinding` / over-bind bug class (a non-binding ident harvested as a binding, a target head / attribute / subscript over-bound) structurally, not by patching.

**What "bypass-proof" does and does NOT cover (honest scope).** The poison guarantee is a **pattern-position** property: any node kind reached INSIDE a routed pattern/target collector that is outside its exhaustive table poisons its contained names → `Undecidable` (never a false definite). It is NOT a statement-level property. A **name-INTRODUCING statement kind that is never visited as a binding site at all** (a `KNOWN-GAP` row in §8d.1) leaves its name unbound; that name then falls through the scope chain to the import set and, IF it collides with a local import of the same spelling, resolves to a **false `ImportedName`**. Such gaps are FINITE (each grammar's node-kind set is enumerable) and are ENUMERATED per grammar in §8d.1 with their exact collision consequence. The XB2 property "no false definite" therefore holds ABSOLUTELY for pattern-position gaps and holds for statement-kind constructs ONLY up to the enumerated KNOWN-GAP rows.

### Per-language resolution precision matrix

Precision = how confidently the resolver can reach a DEFINITE verdict for the construct (a false definite is never emitted; residual ambiguity is `Undecidable`).

| Construct / rule | Rust | TS/TSX/JS | Python | Go |
|---|---|---|---|---|
| Shadowing (local beats import) | precise | precise | precise | precise |
| Sequential position (use-before-decl → import/Undecidable) | precise (`let`) | precise (`let`/`const`) | n/a (no TDZ — hoisted) | precise (`:=`, `var`/`const`) |
| Hoisting | items, params, pattern binds | `var` + fn/class decls → nearest fn/module | `def`/`class`, assignments (fn-scoped) | package/fn decls, params |
| Destructuring / composite patterns | precise (tuple/struct/slice/or/shorthand-field/captured, constructor path excluded; incl. closure composite params + `let E(v)`/`let P{y,..}`) | precise object/array destructuring in params, `let`/`const`, AND `catch (e)`/`catch ({code})` (keys/RHS excluded) | assignment/for tuple+starred; `except … as`/`with … as` alias; attribute/subscript excluded | multi-name `:=` / spec; receiver + named returns; type-switch alias; `range :=` |
| Pattern-scope binds (`match`/`for`/`if let`/`while let`) | precise (arm/body scope; else no leak) | — | comprehension/generator scope | — |
| Class-scope isolation (class vars invisible to methods) | n/a | n/a | precise (method scope re-parented past the class, G5) | n/a |
| Walrus `:=` target | n/a | n/a | `Undecidable` (poison — PEP-572 scope unclassifiable, G6) | n/a |
| Unknown / macro / `match`-case pattern (G7 backstop) | `Undecidable` (poison; e.g. `macro_invocation`) | `Undecidable` (poison; exhaustive table → future-proofing) | `Undecidable` (poison; `match`/`case`, walrus) | `Undecidable` (poison; exhaustive table → future-proofing) |
| Non-referencing positions dropped | field access uses `field_identifier` (n/a) | member = `property_identifier` (n/a) | `attribute` field, `keyword_argument` name, `global`/`nonlocal` names dropped | selector = `field_identifier` (n/a) |
| Glob/wildcard import member | `Undecidable` (never coerced) | `Undecidable` | `Undecidable` | `Undecidable` (dot-import) |

### 8d.1 Terminal name-introducer audit (XB2 round-4, J0)

TERMINAL enumeration of every node kind in each vendored grammar's `node-types.json` that can introduce a name binding, classified into exactly three classes. After this round the contract claims **exactly** what the code delivers.

- **HANDLED** — routed to an exhaustive binding collector (poison-terminated). Emits a correct `LocalBinding`.
- **NON-BINDING** — verified to introduce NO local name (its identifiers are uses / labels / property keys). No binding is emitted; correct.
- **KNOWN-GAP** — a name-introducing construct we DELIBERATELY do not bind. The name falls through to imports → **false `ImportedName` ONLY on a same-spelling import collision** (else correct `Undecidable`). Each row states that consequence. KNOWN-GAP rows are documented-accepted (routed findings), NOT XB2 violations.

Evidence = `node-types.json` field/child excerpts (Rust 0.24.2, TS 0.23.2, JS/Python/Go 0.25.0).

**Go** (`tree-sitter-go` 0.25.0)

| Class | Node kind(s) | Evidence + consequence |
|---|---|---|
| HANDLED | `function_declaration`/`method_declaration`/`type_spec` names; `parameter_declaration` (receiver/params/named results); `short_var_declaration`; `range_clause`/`for_clause` (`:=`); `type_switch_statement` alias; `var_spec`/`const_spec`; **`receive_statement` (`:=`, J1)** | `receive_statement`: `field left -> [expression_list]`, `field right -> [_expression]`; `communication_case`: `field communication -> [receive_statement, send_statement]`. `case v := <-ch` binds `v` (`:=`-token discriminator, same as `range_clause`). **L1 GENERAL RULE (Go spec: "each if/for/switch is its own implicit block"):** EVERY statement kind carrying a header/init `:=` binding position opens its OWN Block scope — `if_statement` (`if x := …; c {}`), `expression_switch_statement` (`switch x := …; x {}`), `type_switch_statement` (`switch x := y.(type) {}` alias — bound in that scope by `collect_param_bindings`), `for_statement` (`range`/`for`-init), `communication_case` (per-case receive alias). **M1 COMPANION RULE:** each switch/select CLAUSE is itself an implicit block — `expression_case`, `type_case`, and `default_case` (shared by expression/type switch AND select) each open their OWN Block scope, so a case/default BODY `:=` scopes to that clause. So an init/alias/loop/case-body var is invisible AFTER the statement (and a case/default-body binding invisible to SIBLING clauses); a post-statement or sibling-clause use of the same spelling resolves to the outer/import binding (precise, NEVER a false `LocalBinding`). |
| NON-BINDING | `send_statement`; `labeled_statement`; `receive_statement` (`=` reassign) | `send_statement`: `field channel/value -> [_expression]` (both uses). `labeled_statement`: `field label -> [label_name]` (jump target, not a value). |
| KNOWN-GAP | Go generics: `type_parameter_declaration` under `type_parameter_list` (`func F[T any]`) | `type_parameter_declaration`: `field name -> [identifier]`, `field type -> [type_constraint]`. `T` is NOT bound → collides with a same-spelling import → false `ImportedName`. LOW surface: `T` uses are mostly `type_identifier` (type position), not tracked occurrences. |

**TS/TSX/JS** (`tree-sitter-typescript` 0.23.2, `tree-sitter-javascript` 0.25.0)

| Class | Node kind(s) | Evidence + consequence |
|---|---|---|
| HANDLED | `function`/`generator_function`/`class`/`abstract_class` declaration names; `variable_declarator` (`let`/`const`/`var`); params; `catch` param; **named `function_expression`/`generator_function` self-name (J2)**; **named `class` EXPRESSION self-name in `class_body` (J2)** | `function_expression`: `field name -> [identifier]` → binds inner-scope-only (recursive self-ref). `class` (expr, `named=true`, distinct from `class_declaration`): `field name -> [type_identifier]` → binds in the `class_body` scope. |
| NON-BINDING | `labeled_statement` | `field label -> [statement_identifier]` (jump target, not a value binding). |
| KNOWN-GAP | `enum_declaration`; `internal_module`/`module` (namespace); `import_alias` (`import X = …`); **`using`/`await using` declaration (J5)** | `enum_declaration`/`module`: `field name -> [identifier]` — enum/namespace name unbound → collides with a same-spelling import → false `ImportedName` (surface REAL: `E.X` value use is an `identifier` occurrence). **J5:** `tree-sitter-typescript` 0.23.2 has NO using-declaration node — `lexical_declaration`: `field kind -> [const, let]` only, and `using` is an anonymous (`named=false`) token → `using x = …` MISPARSES; the binding is invisible → collision → false `ImportedName`. Upstream grammar limitation; revisit on grammar bump. |

**Python** (`tree-sitter-python` 0.25.0)

| Class | Node kind(s) | Evidence + consequence |
|---|---|---|
| HANDLED | `function_definition`/`class_definition` names + params; `assignment`/`for`/`for_in_clause` targets; `except … as`/`with … as` aliases; **PEP-695 `type_parameters` on `def`/`class` (J3)**; **PEP-695 `type_alias_statement` name (J3)**; **`augmented_assignment` (`x += 1`) bare-name target at ALL scopes (L3)** | `function_definition`: `field type_parameters -> [type_parameter]` → each `type_parameter` `type` child's LEADING identifier binds in the definition scope (`def f[T]` binds `T`, `T: bound`→`T` only). `type_alias_statement`: `field left -> [type]` → leading identifier of `left` binds (`type X = …`→`X`). **L3 GENERAL RULE:** `augmented_assignment` `field left -> [pattern, pattern_list]` — a bare-name target binds `LocalBinding` in EVERY scope where plain `assignment` binds (function / class / module), NO scope special-case. CPython compiles it to STORE_FAST (function → whole-function-local, `UnboundLocalError` proof) or STORE_NAME (class body → class attr; module → module global) — all three CREATE the name, so a later same-scope read resolves to it, never a false `ImportedName`. An `attribute`/`subscript` target binds nothing (H2 collector skips). |
| NON-BINDING | `global_statement`/`nonlocal_statement`; `attribute`/`subscript` targets | `global`/`nonlocal`: `children [identifier]` names DROPPED (F6). `obj.x += 1` / `d[k] += 1`: attribute/subscript targets introduce no local name (H2 collector skips). |
| KNOWN-GAP | PEP-695 type param `T` inside a `class C[T]` METHOD; generic-alias RHS type params (`type X[T] = …T…`) | `class C[T]` binds `T` in the class scope, but a method's `Function` scope is re-parented PAST the class scope (G5) → `T` invisible in the method body → collision with a same-spelling import → false `ImportedName`. K4: for a generic alias `type X[T] = …`, ONLY the alias NAME `X` binds — the `type_alias_statement` `type_parameters` (`[T]`) are NOT collected (only `left`'s leading identifier is) → a `T` USE on the RHS falls through to imports → false `ImportedName`. Both LOW surface (rare PEP-695 forms). |

**Rust** (`tree-sitter-rust` 0.24.2)

| Class | Node kind(s) | Evidence + consequence |
|---|---|---|
| HANDLED | `fn`/`struct`/`enum`/`trait`/`const`/`static`/`type`/`mod`/`union` names; `let` pattern; `match_arm`/`for`/`if let`/`while let` pattern-scope; `parameter`/closure params; **ALL generic params — TYPE (`type_parameter`) AND CONST (`const_parameter`) — at fn / impl / struct / enum / trait / union level (L2)** | Pattern/param binds route through the exhaustive `pattern`-supertype collector (poison-terminated). **L2 GENERAL RULE:** every item carrying a `field type_parameters -> [type_parameters]` opens its OWN Block scope (`scope_kind_of`), and each `type_parameter` (`field name -> [type_identifier]`) + `const_parameter` (`field name -> [identifier]`) binds there HOISTED. Binding is strictly SAFER than the old "no collision surface" claim: a type param in VALUE position (`T::default()` scoped-path head, an assoc-fn/const call) IS a tracked `identifier` occurrence and would mis-resolve to a same-spelling import without the binding. `lifetime_parameter`/`metavariable`/`attribute_item` carry no identifier surface (excluded per L2). **M2 RULE:** an ASSOCIATED `function_item`/`const_item` NAME (direct member of an `impl_item`/`trait_item` `declaration_list`) does NOT bind into the item's L2 Block scope — assoc items are path-only (`Self::`/`Type::`), so a BARE use in a sibling/default method body resolves to the import/outer binding, never the assoc item (would otherwise be a false `LocalBinding`). Module-level `fn`/`const` (under `source_file`/`mod_item`) still bind by bare name; struct/enum/union GENERIC binding at L2 is unaffected. |
| NON-BINDING | `const_item`/`static_item` value; lifetime generic params (`<'a>`) | `const_item`: `field value -> [_expression]` (RHS = use). `lifetime_parameter` USES are `lifetime` tokens, NEVER recorded as `identifier` occurrences → NO collision surface. |
| KNOWN-GAP | — (none beyond the G7 poison backstop) | Every pattern-position gap poisons → `Undecidable`, never a false definite. No unvisited name-introducing statement kind remains. |

### 8d (bypass-proof scope — see the two paragraphs above)

The XB2 "no false definite" property holds **absolutely** for pattern-position gaps (poison) and holds for statement-kind constructs **up to the enumerated §8d.1 KNOWN-GAP rows** (each a false-`ImportedName`-on-collision, documented-accepted).

---

## 9. Decision log (ratified 2026-06-01)

- [x] §1 positional bundle on `NodeCore` (universal-optional) — **ratified**.
- [x] §1 `file_path` unification (migrate `DocNodeExt.uri` → `NodeCore.FilePath`) — **ratified**.
- [x] §2 **two** file hashes — `file_raw_hash` (byte-offset gate) + `file_norm_hash` (drift classifier) + per-node `region_hash` — **ratified** (replaces the single `file_content_hash`).
- [x] §1 promote `visibility` to universal-code `CodeNodeExt` enum (SPEC amendment) — **ratified**.
- [x] §4 normalization + **xxHash64** (`file_raw_hash` over raw bytes; `file_norm_hash`/`region_hash` over normalized bytes) — **ratified**.
- [x] §5 Båge read path — **RESOLVED: in-process `StoragePort`** (predicate names internal, not frozen).
- [x] §6 incremental re-ingest — **dependency of the dual-write coordinator** (promoted from fast-follow).
- [x] §7 standalone node-locator-contract drop BEFORE Hylla Phase 2 — **ratified (`drop_020`)**.
- [x] Parser engine — **official `tree-sitter/go-tree-sitter` CGO binding** for both Hylla + Båge behind Båge's `ParserPort`, cross-compiled via `zig cc` (Hylla SPEC §1.1.12 amended).
