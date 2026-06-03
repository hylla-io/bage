# Båge Dogfood Findings

Running log of friction, gaps, and bugs found by **using Båge to edit Båge** (see the
[Dogfooding](README.md#dogfooding) policy). Each finding is fixed test-first; when a fix
sharpens one language it lands with test parity across the other file types.

Status legend: **FIXED** · **OPEN** · **WONTFIX** (deliberate constraint).

## Findings

| # | Finding | Surfaced by | Status | Resolution |
|---|---------|-------------|--------|------------|
| 1 | `bage apply --line N` consumed the line's trailing newline, so a `--text` without a newline merged the line into the next one (a blank line in README vanished). | Editing README status line through bage. | **FIXED** | Line mode is now newline-agnostic: `applyRegion` excludes a trailing `\n` from the resolved region and `runApply` strips one trailing `\n` from `--text` in line mode. Covered by `cmd/bage` line-edit tests. |
| 2 | No practical way to apply a multi-line replacement from the CLI without hand-escaping a giant `--text`. | Dogfooding multi-line doc edits (CLAUDE.md, README sections). | **FIXED** | Added `--text-file` to `bage apply`; replacement text is read from a file. Used for every multi-line dogfood edit since. |
| 3 | Båge cannot **read** a file — there is no `show`/`cat` command. | General dogfooding. | **PLANNED** | Scoped in [ADR-0004](docs/adr/0004-file-lifecycle-ops.md): a `show` primitive emits the region + `region_hash` map (the addressable-block read view). Standalone/MCP-facing; in GDD mode Hylla's graph is the read side. |
| 4 | Båge cannot **create** a new file — `apply` resolves a region in a file that must already exist. | Creating this file, CONTRIBUTING.md, etc. | **PLANNED** | Scoped in [ADR-0004](docs/adr/0004-file-lifecycle-ops.md): `create` rides the same two-phase engine, anchored by non-existence (hard-reject-on-clobber), and joins mixed-op atomic batches. |
| 5 | No first-class **append / insert-at-EOF**: adding content to a file's end (this session: SPEC §10, CONTEXT terms, README) requires `grep`-ing the last line's byte offset and replacing `[lastline:end]`, because `apply` needs a non-empty existing region to anchor. | Dogfooding the file-lifecycle design docs through bage. | **OPEN** | Candidate: a zero-width insert (`--start N --end N`) or `--at-end` mode, validated as a pure insertion (no region content to hash — anchor on the file `raw_hash` + offset instead). Fold into the §10 `Op` work. |

## How to add a finding

1. Reproduce the friction while editing a real file through `bage apply` / `bage rename`.
2. Add a row above with what broke and how it surfaced.
3. Fix it **test-first** (mage gates green); if it touches normalize/parse, run `mage fuzz`.
4. Keep test parity across file types — a Go-found fix lands with equivalent coverage for
   the other languages/file types it affects.
5. Flip the row to **FIXED** with the resolution, in the same PR as the fix where possible.
