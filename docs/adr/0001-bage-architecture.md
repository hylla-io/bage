---
status: accepted
---

# Båge is a linked library with a files-as-truth two-phase dual-write

## Context

Båge is a bidirectional code-graph round-trip file editor. It must (a) run standalone
as an IDE-style edit engine over files + LSP with no graph database, and (b) integrate
with Hylla (the polyglot code-knowledge-graph on DGraph) so that one agent-facing edit
lands in **both** the graph and the files with no possible drift between them. The hard
requirement from the project owner: there may NEVER be separate state in the Hylla DB
and the actual files; a partial failure must roll back.

## Decision

**Båge is a standalone Go library (plus a `bage` CLI), and Hylla imports it and links
it statically into the Hylla binary.** Not built into Hylla (that would kill standalone
use); not a separate sidecar process (a process boundary turns the dual-write into a
distributed commit across filesystem + DGraph + IPC and makes "no drift" expensive and
fragile).

The sync guarantee comes from two things, not from where the code lives:

1. **Both legs run in one process.** A statically-linked library shares the process and
   transaction with in-repo code; only a process boundary would weaken atomicity.
2. **Both legs expose a two-phase `Prepare` / `Commit` / `Rollback` API.** The
   **coordinator lives in Hylla** (it owns the graph leg and the single MCP edit
   entrypoint) and sequences: write a durable **WAL / edit-intent log** (a plain file —
   Hylla forbids SQLite) → `bage.Prepare` (apply edit to a staged sibling temp buffer,
   run the project's configured formatter + linters, parse the CST from those exact
   bytes) → `graph.Prepare` (stage node/edge updates) → `bage.Commit` (atomic
   temp→rename) → `graph.Commit` (DQL mutation). On any handled failure: restore the
   file from the WAL and abort the graph transition (clean synchronous failure for the
   agent). On crash: replay the WAL on restart and converge the graph to the files.

**Files are the source of truth; the graph is a reconcilable projection.** Commit is
file-first so the projection never leads the truth: if the graph leg fails after the
file is written, the file is re-ingestable and the graph converges; the reverse would
leave the graph asserting something the file doesn't contain.

**Drift discipline (oh-my-pi-derived, adapted for byte offsets):** the volatile locator
is `(file_content_hash, byte_range)`; node identity is stable and separate. We persist
**two** per-file hashes: a **raw hash** (xxHash64 of raw bytes) that gates byte-offset
validity, and a **normalized hash** (LF/BOM/trailing-whitespace-normalized) that
classifies whitespace-only vs real drift. On `raw` mismatch + `norm` match →
whitespace-only drift → re-resolve the node's range from the graph cheaply. On `norm`
mismatch → real drift → re-ground from Hylla or reject. Never silently slide an edit
onto the wrong region.

**Båge is locator-addressed / ID-blind.** Båge operates only on
`(file_path, byte_range, hashes)` and LSP symbol operations. It never constructs or
parses Hylla node IDs; the coordinator translates an agent's node-ID request into a
locator before Båge sees it. This avoids two drift-prone ID derivers and keeps
standalone Båge identical to integrated Båge.

## Considered options

- **Built into Hylla** — rejected: Båge must run standalone.
- **Separate sidecar process** — rejected: distributed commit across FS + DGraph + IPC;
  "no drift" becomes a partition problem. A process boundary is the only thing that
  actually weakens atomicity, so we avoid it.
- **Always-converge rollback** (never restore the file, graph always chases) — rejected
  as the default: it gives agents async "applied, reconciling" semantics. We want atomic
  synchronous edits, so we restore on handled failure and reserve convergence for crash
  recovery.
- **Single normalized hash** (as Hylla SPEC §6.7.5 currently mandates) — rejected: a
  byte-offset locator cannot survive whitespace normalization, so the gating hash must
  be over raw bytes; the normalized hash is kept only as a drift classifier.

## Consequences

- Hylla must expose its `StoragePort` (or an equivalent in-process surface) so the
  coordinator can resolve node-ID → locator and apply graph updates; Båge depends on
  Hylla for nothing (dependency points Hylla → Båge only).
- Hylla's deferred "mode 3 (Hylla-applied edits)" editor work is rehomed to Båge.
- Incremental re-ingest in Hylla moves from "fast-follow" to a dependency of the
  dual-write tool.
- The Hylla↔Båge node contract gains a **write contract**: after an edit Båge returns
  `{changed_byte_range, new_raw_hash, new_norm_hash}` so Hylla re-ingests only the
  changed range.
- See `BAGE_INTEGRATION_PLAN_ADJUSTMENT.md` in `hylla/polyglot-foundation/` for the
  Hylla-side SPEC deltas this implies.
