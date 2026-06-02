// Package bage is the public, curated facade for Båge — the bidirectional
// code-graph round-trip file editor. It is the only import surface consumers
// (Hylla's cross-store coordinator, IDEs, standalone tools) should depend on;
// the internal/* packages are implementation detail and are deliberately not
// re-exported beyond the types and behavior gathered here.
//
// # Files are the source of truth
//
// Båge treats the files on disk as authoritative. A graph (when present) is a
// projection of the files, never the reverse. Every edit is addressed by a
// Region — a (byte range, line/col range, region_hash) locator that is trusted
// only while the live file still contains the anchored content; on drift Båge
// rejects or re-grounds rather than silently misapplying an edit.
//
// # Region-anchored editing
//
// An Edit targets a Region by its region_hash — the xxHash %016x of the region's
// RAW bytes (HYLLA_NODE_CONTRACT.md §1, §2; SPEC §8.1). The hash is the content
// anchor and gives omp-parity with Hylla's per-node locator bundle (the same
// region_hash is the only seam, so file-mode and graph-mode resolve identically)
// and is the basis of concurrency safety. Resolution (ADR-0003) is layered:
//
//   - region_hash VERIFIES: the bytes at the region's own offset are the block
//     the edit targets when their hash matches (an Exact, in-place resolve).
//   - the CST RELOCATES: when the in-place hash no longer matches, Båge reparses
//     the live file and matches the region_hash against every node. Exactly one
//     match is a benign concurrent shift, re-resolved to the new offset (Shifted).
//   - identity DISAMBIGUATES — and Båge declines: zero matches is a conflict and
//     more than one match is ambiguous; both are hard rejects. Båge over-rejects
//     on purpose: corruption is never acceptable, a rejected edit is.
//
// A FileAnchor (per file: RawHash gates byte-offset validity, NormHash classifies
// whitespace-only drift) accompanies the edits as the file-level drift gate.
//
// # The FILE-LEG two-phase contract
//
// Båge owns only the FILE leg of an edit. The two-phase protocol is:
//
//	Prepare(ctx, edits, anchors) -> *Plan       // optimistic: resolve + stage + WAL
//	Commit(plan)                 -> []EditResult // atomic: resolve-under-lock + write
//	Rollback(plan)                              // discard staged edits
//
// Prepare is OPTIMISTIC and holds no lock: it reads each live file, resolves every
// edit against those bytes (rejecting a Conflict/Ambiguous with a *ConflictError,
// matchable via errors.Is(err, ErrConflict)), preview-splices, runs the optional
// Formatter/Linter, reparses to confirm the result still parses, and durably
// records a write-ahead-log intent. Prepare never mutates a source file — its sole
// on-disk effect is the WAL record.
//
// Commit is the ATOMIC, lossless point. Per file, UNDER A PER-FILE LOCK, it
// RE-READS the live bytes and RE-RESOLVES every edit (resolve-under-lock, so a
// concurrent commit that benignly shifted a region is picked up and the edit lands
// at the current offset, never the stale one; a same-region conflict is rejected),
// atomic-writes, and computes one EditResult per edit — the write-back contract
// (changed byte range, recomputed region/file hashes, new line range) Hylla reads
// to re-ingest only the changed region (SPEC §8.2). Same-file commits serialize on
// one lock; cross-file commits take different locks and run in parallel.
//
// # Standalone vs integrated use
//
// In standalone mode Båge is a pure file/LSP edit engine with no graph: callers
// typically use Apply (Prepare-then-Commit) for a one-shot edit. In integrated
// mode Hylla's cross-store coordinator drives Prepare/Commit/Rollback on the FILE
// leg interleaved with its own graph leg, so a single agent-facing edit lands in
// both the graph and the files as an all-or-nothing operation with no drift between
// them. The coordinator — not Båge — sequences the two legs; Båge exposes only the
// FILE-leg verbs.
//
// # Recover is the crash path
//
// Recover replays any WAL intent left behind by a crash between Prepare and Commit,
// restoring the affected files to their pre-Prepare state so the files converge back
// to a consistent, committed state. A clean Commit leaves nothing to replay, so
// Recover is then a no-op.
package bage
