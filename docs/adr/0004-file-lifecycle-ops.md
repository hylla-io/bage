---
status: accepted
---

# File-lifecycle ops: create / delete / move ride the same anchored two-phase engine

## Context

Båge today edits **existing** files (region-anchored `apply` + LSP `rename`). To be a
real "IDE for agents" — and the file leg of Hylla's GDD — it must also **create**,
**delete**, and **move** files, and do so as **one all-or-nothing batch** alongside edits
(the common refactor is "create `new.go` + edit callers + delete `old.go`"). These ops have
no pre-existing region to anchor, so naïvely bolted on they would punch a hole in Båge's one
promise: *reject, never corrupt — every mutation is gated by a content hash.*

In GDD mode the **agent edits the graph**, not files; **Hylla derives the file work** and
drives Båge as a library, owns project-level correctness (does the app compile / run / pass
tests) and commit **timing**. Båge must stay **graph-agnostic and open** so a non-Hylla MCP
wrapper can drive the same primitives directly. So the design must serve Hylla first without
baking Hylla in.

## Decision

**One operation abstraction, one engine.** A batch is `[]Op` where `Op` is a tagged sum of
`{Edit, Create, Delete, Move}`. All four flow through the **same** `Prepare` (acquire
per-file locks in a deterministic sorted order → validate each op's anchor → run the gate →
stage every write as a sibling temp) and the same `Commit` / `Rollback` / WAL. A batch is
heterogeneous; the caller (Hylla, or an MCP wrapper) gets one EditResult-set spanning all op
kinds as a single logical change.

**Per-op anchors — the content-hash promise extended to the lifecycle:**
- **Create** is anchored by **non-existence**: the target path must not already exist. An
  existing path with content → **hard reject** (never clobber). Optional later escape hatch:
  an explicit overwrite asserting the current `raw_hash` (same drift gate as `apply`), so even
  an overwrite can't clobber drifted content.
- **Delete** (and a move's **source** side) is anchored by the expected **`raw_hash`**: the
  file must still hash to what the caller was shown. Drift → **hard reject** (never discard
  bytes the caller didn't see). Before unlinking, the **full prior bytes are captured in the
  WAL** so a failed/partial transaction restores them.
- **Move** = **anchored-delete-at-source + anchored-create-at-dest**, atomically. MVP move is
  **relocate + (Hylla) re-identify the moved file's nodes** only; resulting reference/import
  breakage surfaces as **post-move diagnostics**, not a silent magic fixup. LSP-driven import
  fixup (`workspace/willRenameFiles` → `WorkspaceEdit`) is an **opt-in `--fixup`** that reuses
  the existing `rename` → `WorkspaceEditToFileEdits` → atomic-batch path.

**Cross-file "atomic" is WAL-backed all-or-nothing *on recovery*, not a single syscall.**
POSIX has no multi-file atomic flip. The WAL records the whole batch intent + undo bytes; if
the process dies mid-flip, `Recover` drives it to fully-before or fully-after, never half.
With **file-first ordering**, the graph leg never commits ahead of a durable file state.

**Gate boundary.** Båge's gate is the **mechanical per-file parse floor** ("would the staged
bytes still parse?" → hard reject if not) plus **optional format/lint hooks that the caller
configures and Båge merely executes on the staged bytes**. **Project-level correctness —
does the whole module compile / run / pass tests + `-race`, and is *now* the right moment —
is the caller's (Hylla's), and Båge never runs the build or tests.** The `Prepare`/`Commit`
split hands the caller commit **timing**.

**Read / nav / housekeeping scope (the deliberate edges):**
- **Read (`show`)** — Båge gains a read primitive emitting the region + `region_hash` map (the
  addressable-block view). Standalone/MCP-facing; in GDD mode Hylla's graph is the read side.
- **Diagnostics** — after an edit/move Båge surfaces LSP `publishDiagnostics` + parse result in
  the result envelope; it surfaces, it does not fix.
- **LSP scope is write-adjacent only** — rename, `willRenameFiles` fixup, diagnostics. Not a
  full nav server: go-to-def / find-refs are graph edges (integrated) or an optional thin LSP
  passthrough (standalone).
- **No text search** (ripgrep / the harness), **no standalone directory ops** (`create` makes
  parent dirs; `delete` reaps empty dirs), **no undo stack** (git is history; the WAL is
  crash-recovery).

## Considered options

- **Thin create that skips the engine (just write bytes)** — rejected: bifurcates the safety
  model and can't participate in a mixed-op atomic batch with edits.
- **Unconditional delete (blind unlink)** — rejected: silent data loss, the exact failure Båge
  exists to prevent; the `raw_hash` anchor makes destruction gated and reversible.
- **Move with intrinsic import-fixup** — rejected for MVP: couples a filesystem primitive to
  uneven per-language LSP capability and makes "move a file" unboundedly large. Fixup is opt-in
  and visible instead.
- **Separate engine per op kind / homogeneous batches only** — rejected: makes the most common
  refactor (create + edit + delete) non-atomic, a non-starter for GDD's "one intent, one graph
  mutation" invariant.
- **Båge owns compile/test correctness** — rejected: project build/run/test + timing is the
  caller's; Båge is a correct-by-construction file mutator with a parse floor, not a verifier.
- **Full LSP nav server inside Båge** — rejected: nav is the graph's job in integrated mode;
  YAGNI for the write leg.

## Consequences

- The transaction unit generalizes from `[]Edit` to `[]Op` (`Edit | Create | Delete | Move`);
  `Prepare`/`Commit`/`Rollback` and the public facade gain the op kinds while `apply`/`rename`
  remain as today (an `Edit`-only / rename-only batch).
- The **WAL extends** to record op kind + undo bytes (deleted/overwritten content) so `Recover`
  delivers cross-file all-or-nothing.
- New CLI verbs land: `create`, `delete`, `move`, `show`; diagnostics ride the result envelope.
- **Hylla-side deltas** (create → N new nodes; delete → close node versions; move → re-identify
  + content-version referencers; mixed-op → one graph mutation; the gate boundary) are tracked
  in `hylla/polyglot-foundation/BAGE_UPDATE.md` and `BAGE_INTEGRATION_PLAN_ADJUSTMENT.md`, updated
  in lockstep with each primitive.
- Dogfood findings #3 (no read) and #4 (no create-file) move from WONTFIX → planned.
- Båge stays ID-blind and graph-agnostic: ops are locator-addressed primitives; Hylla originates
  them from a graph mutation in integrated mode, an MCP wrapper originates them in standalone.
