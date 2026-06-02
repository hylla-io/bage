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
| 3 | Båge cannot **read** a file — there is no `show`/`cat` command. | General dogfooding. | **WONTFIX (for now)** | Deliberate scope: Båge is an editor, not a viewer. Reading uses the harness's built-in read tool (documented fallback). Revisit only if a read primitive proves necessary for host integration. |
| 4 | Båge cannot **create** a new file — `apply` resolves a region in a file that must already exist. | Creating this file, CONTRIBUTING.md, etc. | **WONTFIX (for now)** | Deliberate scope: surgical edits to existing files only. New-file creation uses the built-in write tool (documented fallback). A future `bage create` is possible but unscoped. |

## How to add a finding

1. Reproduce the friction while editing a real file through `bage apply` / `bage rename`.
2. Add a row above with what broke and how it surfaced.
3. Fix it **test-first** (mage gates green); if it touches normalize/parse, run `mage fuzz`.
4. Keep test parity across file types — a Go-found fix lands with equivalent coverage for
   the other languages/file types it affects.
5. Flip the row to **FIXED** with the resolution, in the same PR as the fix where possible.
