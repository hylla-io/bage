---
status: accepted
---

# Clipboard verbs, insertion primitives, and issue-sweep completeness

## Context

The v0.3 dogfood surfaced two structural gaps and left two follow-ups (ADR-0005). `apply`
could only replace a **non-empty** existing region: adding a function or appending config meant
computing the last byte offset and replacing `[lastline:end]`, and a too-short `--lines` range
silently left a **stale tail** (dogfood finding #12, which corrupted a release workflow). There
was also no way to **move** a region across files without hand-copying bytes through the
harness. Separately, data-format grammars (JSON/YAML/TOML/XML) had empty outlines (#21), and
pyright/clangd renamed single-file only (#23).

## Decision

**Insertion + whole-file primitives.** `inspect::resolve_insertion` resolves a zero-width
region for `Append`/`BeforeLine`/`AfterLine`; `apply --all` resolves `[0, len)` for a lossless
whole-file replace. Both are hash-free (no content to anchor) so the per-file anchor is the sole
drift gate, and both are mutually exclusive with each other and range addressing.

**Clipboard verbs.** `cut`/`copy`/`paste` ride the anchored engine. `copy` extracts read-only
(bare-content text output); `cut` extracts and removes (WAL-backed, `region_hash`-gated);
`paste` inserts at a `PastePoint` (`AtByte` verbatim, or an `InsertionPoint`) from
`--text`/`--text-file`/`--clip`.

**A single-slot file clipboard.** `--clip` writes/reads a JSON `Clip{content, source_path,
region_hash, cut}` at `$BAGE_CLIPBOARD` (default `~/.bage/clipboard.json`), atomically; `cut`
writes it before the removal commits. This makes a region move cross-file and cross-process. An
empty slot is a distinct `Empty` error. Båge never touches the OS/GUI clipboard.

**Issue sweep.** Data-format outlines gain per-grammar declaration kinds with name extraction
(#21). `Client::rename` primes the workspace (didOpen same-language siblings) for pyright and
generates a temporary `compile_commands.json` for clangd (#23). The release workflow builds a
native per-OS binary matrix (#22).

## Considered options

- **`--at-end` flag vs a general insertion point** — chose one `InsertionPoint` enum
  (`Append`/`BeforeLine`/`AfterLine`) shared by `apply` and `paste` over a one-off append flag,
  so insertion has a single resolver and both verbs behave identically.
- **OS/GUI clipboard integration** — rejected: a file slot is process-agnostic, inspectable,
  scriptable, and carries provenance + the `region_hash` gate; the GUI pasteboard carries none
  of that and is unavailable headless.
- **Clipboard carries only bytes** — rejected: provenance (`source_path`, `region_hash`, `cut`)
  lets a paste (or a human) see where bytes came from and whether the source was removed.
- **Reuse `apply`/`read` for move** — rejected: an explicit `cut`/`copy`/`paste` triple is the
  obvious agent-facing verb set and keeps the removal WAL-backed and hash-gated.

## Consequences

- New CLI verbs `bage cut` / `bage copy` / `bage paste`; `apply` gains `--all` and
  `--append`/`--before-line`/`--after-line`.
- New `clipboard` module (single-slot JSON, atomic write); `$BAGE_CLIPBOARD` override.
- Closes #20 (insertion/append), #21 (data-format key outline), #22 (prebuilt binaries), #23
  (pyright/clangd cross-file rename). Resolves dogfood findings #10 and #12.
- The docker-gated LSP suite (`BAGE_DOCKER_LSP=1`) proves cross-file rename for gopls, pyright,
  and clangd; the host suite covers `compile_commands.json` generation.
