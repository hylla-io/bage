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
