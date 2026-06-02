---
status: accepted
---

# Region-anchored edits + concurrency: region_hash verifies, CST relocates, identity disambiguates

## Context

Båge must serve small local models doing tiny edits and many models editing
concurrently, with the fewest tokens, the least chance of silly mistakes, and
**no lost/misapplied edits** — while staying graph-agnostic (the editing lib) yet
baking in Hylla's philosophy so it works for a graph system. It must match omp
(oh-my-pi: `can1357/oh-my-pi`) in capability + reject-rather-than-corrupt safety,
and improve on it by editing graph "blocks"/regions, not just whole files.

## Decision

**Edit-input model (omp-style, not old+new).** An edit is `{ref, region_hash,
new_text}`: the model is *shown* a target with a hash and **echoes** it (it never
computes a hash, never resends old text). Two ref forms — graph-mode `node_id`
(Hylla resolves it to file+byte-range+region_hash), or file-mode a **line range**
(model-facing; bytes are internal) + the shown region_hash. Fewest tokens; an
off-by-one range produces a hash mismatch → reject, never misapply.

**`region_hash` is the content anchor** (xxHash `%016x` of the region's bytes). It
does three jobs, and only the first is its own:
1. **Verify identity + detect conflict** — region_hash matches ⇒ same block; differs
   ⇒ the block itself was changed (a **conflict**) ⇒ hard reject.
2. **Relocate** is NOT the hash's job — the **CST/graph re-ingest** does it: after a
   concurrent commit, Hylla incrementally reparses and updates the node's locator, so
   a `node_id` ref always resolves to the *current* offset; file-mode reparses and
   matches by hash.
3. **Disambiguate** twins (identical content ⇒ identical hash) is **node identity**
   (graph) or structural path (file-mode), not the hash.

**Concurrency.** Per-file serialization (one writer per file); **resolve the locator
under the file lock, immediately before applying**, so every edit sees prior
concurrent commits (no lost update). On apply: a stale offset with a still-matching
region_hash is a **benign shift** → re-resolve (graph re-ingest / file-mode
reparse-and-match) → apply; a non-matching region_hash is a **conflict** → reject.
Cross-file edits run fully parallel. After each apply: incremental tree-sitter reparse
+ LSP `didChange`, so later edits resolve against current state.

**Hard errors, never silent.** tree-sitter parse failure → reject (always); configured
lint failure → reject; fmt → applied; region_hash unresolvable → reject.

## Considered options

- **old+new (search-replace)** — rejected: more tokens (resends old), duplicate-ambiguous,
  and omp itself chose hash-echo over old-text. The region_hash gives the same
  staleness-catch at lower cost.
- **omp's whole-file + line anchor with snapshot 3-way-merge** — matched in discipline
  but improved on: the node model relocates *exactly* via the CST instead of a
  heuristic fuzzFactor=0 replay. We keep snapshot-replay only as the file-mode fallback.
- **Optimistic concurrency (apply + retry on conflict)** — rejected for MVP: per-file
  serialization with resolve-under-lock is simpler and lossless.

## Consequences

- The edit unit changes from byte-only `FileEdit` to a region-anchored `Edit{Region,
  NewText}` where `Region` carries byte range, line range, cols, and region_hash; `Commit`
  returns `[]EditResult` (new range + new region/file hashes + new line range) — the write
  contract Hylla needs for incremental re-ingest.
- SPEC §7 "concurrency parked" is superseded; per-file locking + resolve-under-lock is now
  in scope, with falsifiable concurrency tests (benign-shift re-resolve, conflict reject,
  cross-file parallel, no lost update).
- Båge stays ID-blind: `node_id` lives only on Hylla's side of the resolve; region_hash is
  the only seam, so one engine serves file-mode and graph-mode identically.
- Hylla-side deltas are tracked in `hylla/polyglot-foundation/BAGE_INTEGRATION_PLAN_ADJUSTMENT.md`.
