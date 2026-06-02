# Båge

Båge is a bidirectional code-graph round-trip file editor. It compiles graph-node
edits into precise, atomic edits in the underlying source files. It runs standalone
(like an IDE edit engine, files + LSP, no graph database) and also integrates with
Hylla so that a single agent-facing edit lands in both the graph and the files with
no possible drift between them.

## Language

**Båge**:
The round-trip file-edit engine this repo builds. Owns the write/file side of
graph-driven development.
_Avoid_: editor, baged (that is the binary name, not the system)

**Hylla**:
The separate polyglot code-knowledge-graph engine (DGraph-backed) that ingests and
queries code. In integrated mode it is Båge's graph backend; Båge is its file-write
engine.
_Avoid_: the graph DB, the index

**Source of truth**:
The files on disk. The graph is a projection of the files, never the reverse. Any
reconciliation converges the graph toward the files.
_Avoid_: canonical state, the model

**Round-trip edit**:
An edit expressed at the graph/node level that is compiled down to a precise file
edit, applied atomically, and reflected back into the graph by re-ingest.
_Avoid_: sync, write-back

**Drift**:
Divergence between a stored locator (or graph node state) and the live file it points
at. Detected by hash, never silently tolerated.
_Avoid_: staleness, mismatch, desync

**Standalone mode**:
Båge operating as a pure file/LSP edit engine with no Hylla and no graph database —
an IDE-style edit surface.
_Avoid_: local mode, offline mode

**Integrated mode**:
Båge linked into Hylla so one MCP entrypoint applies an edit to both the graph and
the files as a single all-or-nothing operation.
_Avoid_: hosted mode, server mode

**Dual-write**:
The integrated-mode operation that updates the graph and the files together, with
rollback/reconciliation guaranteeing the two never diverge.
_Avoid_: two-phase write, sync write

**Locator**:
The volatile pointer from a graph node to a file region — `(file_content_hash,
byte_range)`. Trustworthy only while the live file still hashes to the recorded
fingerprint. Distinct from node identity, which is stable.
_Avoid_: pointer, reference, address

**Surgical edit**:
A minimal-context, single-target change an agent makes by node identity rather than
by re-reading whole files — the unit that lets a small model edit cheaply.
_Avoid_: patch, diff, micro-edit

**Coordinator**:
The component that sequences a dual-write as an all-or-nothing saga across the file
leg (Båge) and the graph leg (Hylla), and rolls both back on any failure.
_Avoid_: orchestrator, manager, transaction manager

**Two-phase edit**:
The prepare/commit/rollback protocol both legs expose so the coordinator can stage
and validate every change before any of it becomes durable.
_Avoid_: 2PC, transaction, staged write

**Node identity**:
A node's unique, path-based ID constructed by Hylla — e.g. Go `path/to/package/<block
-name>`; Python includes the module file. The file path is a node field. Not stable
across rename (rename closes the old version, opens a new one; time-travel keeps the
old). Båge consumes it only as an opaque handle and never constructs or parses it.
_Avoid_: UUID, stable ID, anything implying Båge derives IDs

**Locator-addressed**:
Båge's boundary stance: Båge operates purely on `(file_path, byte_range, hashes)` and
LSP symbol operations — never on Hylla node IDs. The coordinator translates an
agent's node-ID request into a locator before Båge sees it, and translates Båge's
result back into graph updates. ID construction is wholly Hylla's.
_Avoid_: ID-aware, graph-aware editing

**Re-ground**:
Recovering from drift by re-resolving a node's locator from Hylla (after the changed
region is re-parsed), instead of trusting a stale byte range. The graph-native
recovery path; snapshot replay is the fallback.
_Avoid_: rebase, resync, refresh

**omp** (oh-my-pi):
The hash-anchored line-edit tool (`can1357/oh-my-pi`) Båge's drift discipline is
benchmarked against. Båge MUST match omp's capability, simplicity, and
reject-rather-than-corrupt safety; its intended differentiator is editing graph
"blocks"/regions (anchored by `region_hash`), not only whole files. Use the shorthand
"omp" everywhere.
_Avoid_: spelling out oh-my-pi after first use, pi

**Conflict** (edit conflict):
When a concurrent edit changed the *same* region this edit targets — detected because
the target's `region_hash` no longer matches. Conflicts are hard-rejected, never
merged. Distinct from a **benign shift** (a concurrent edit elsewhere moved the
region's position but not its content, so the hash still matches and the edit
re-resolves and applies).
_Avoid_: collision, race

**Region anchor**:
The `region_hash` (xxHash of a region's bytes) that identifies an editable block by
content, not position — Båge's per-node equivalent of omp's per-file snapshot tag. It
is what lets an edit survive concurrent offset drift (re-resolve the region by hash)
and what lets Hylla address a graph node/block rather than a whole file.
_Avoid_: block hash, content hash (those are ambiguous)

**Parser subsystem**:
Båge's single source of CSTs and byte-range locators, exposed behind a port so the
engine is a swappable adapter. Integrated Hylla consumes it for ingest of
byte-addressable formats — one parser, so the graph and files can never disagree on
structure. Non-byte-addressable formats (ebooks, docx, pdf) are Hylla's own, not
Båge's.
_Avoid_: the parser, tree-sitter (that is one possible engine, not the subsystem)
