---
status: accepted
---

# Read primitive, output-format serialization, and a public error taxonomy

## Context

Båge could mutate files (`apply`/`create`/`delete`/`move`/`rename`) and expose an
addressable *structure* map (`show`), but it had no first-class **read**: a standalone or
MCP caller could see *where* a block is (`region_hash` + ranges) but not its *content*, and
had to re-read the file itself. In integrated mode Hylla holds content in its node, so it
needs structure only; standalone, the caller has nothing. Båge is **library-first** (Hylla
links `pkg/bage`; the CLI/MCP is a thin edge), so the read must be a Go API first.

Output also needed an encoding seam. `show`/`diagnose` hand-rolled a `--json` bool; an MCP
wrapper that wants to decide *what the model sees* needs a uniform, structured surface — and
a token-efficient one, since the read map is the agent's cheapest view of a file.

Errors were typed (`ConflictError`, `ErrExists`, `ErrNotFound`) but carried no
machine-branchable *kind*: a wrapper had to `errors.Is` plus inspect English. Worse,
`ConflictError` conflated a region **conflict** with raw_hash **drift**.

## Decision

**A read leg in `pkg/bage`.** `ReadBlocks(opened, includeContent) []Block` is the
OpenedFile-level primitive (a host that already parsed reuses its tree); `(*Editor).Read(ctx,
path, ReadOptions) ReadResult` is the facade. Addressing is mutually exclusive: whole-file,
`Symbol` (name match), line, or byte range; `IncludeContent` is opt-in so Hylla never pays
for content it has. `Block` is a **flat** struct `{Kind, Name, StartLine, EndLine, StartByte,
EndByte, RegionHash, Content}` — deliberately not an embed — so JSON keys stay snake_case and
a block slice is a uniform array TOON renders tabular. `region_hash` is computed once by
`region.HashRegion`; `show` and `read` share it (the cmd-layer hash loop is lifted down).

**A `pkg/render` serialization layer.** `Format{text|json|toon}` + `ParseFormat` + one
`Emit(w, Format, v)`. JSON is `MarshalIndent`; text dispatches to a `RenderText(io.Writer)`
the result type *owns* — implementing the interface needs no import of `render`, so domain
packages stay render-free and there is no import cycle; `toon` uses
`github.com/toon-format/toon-go`. `--format` **hard-replaces** the old `--json` on
`show`/`diagnose` (a pre-1.0 breaking change); JSON output is byte-identical.

**A public error taxonomy.** `Kind{conflict|drift|exists|not-found|usage|io}`, `KindOf(err)
Kind`, and `Envelope(err) ErrorEnvelope{Kind, Path, Message}` — re-exported from `pkg/bage`
so an external MCP module (a separate Go module that cannot import `internal/`) branches on
`kind`. `ConflictError` gains a kind so drift and conflict are distinguishable; `KindOf` also
maps `os.ErrNotExist` to `not-found`.

## Considered options

- **In-tree TOON encoder vs the dependency** — chose the `toon-format/toon-go` dependency
  (unpinned pre-1.0 pseudo-version) over a hand-rolled encoder, accepting the dep to avoid
  owning TOON-spec correctness. (Revisit if the dep stalls; the `Emit`/`MarshalTOON` seam
  isolates it.)
- **Keep `--json` as a deprecated alias** — rejected: pre-1.0, a clean single `--format`
  flag is worth the breaking change; carrying two flags invites precedence ambiguity.
- **`Block` embeds `Symbol`** — rejected after live testing: the embed made JSON leak Go
  field names and made TOON render *nested* (losing the tabular token win). Flattened.
- **Error taxonomy in `internal/session` only** — rejected: an external MCP module can't
  import `internal/`; the taxonomy is re-exported through `pkg/bage`.
- **Read content always** — rejected: Hylla holds content in its node; content is opt-in so
  the integrated path stays cheap.

## Consequences

- New CLI verb `bage read` (whole-file/`--symbol`/`--line`/byte, `--content`, `--format`);
  `show`/`diagnose` move from `--json` to `--format text|json|toon`.
- `pkg/render` is a new public package; result structs (`ReadResult`, cmd views,
  `EditResult`-style) implement `RenderText` to stay text-renderable without a cycle.
- `pkg/bage` re-exports the error taxonomy; `ConflictError` carries a kind.
- Adds the `toon-format/toon-go` dependency.
- **Open / follow-ups**: `--format` + error-envelope on the edit verbs (shipped v0.4);
  per-key addressing for data-format grammars, an insertion/append primitive, prebuilt
  binaries, and pyright/clangd cross-file rename completeness — all resolved in
  [ADR-0006](0006-clipboard-and-insertion-primitives.md) (v0.7).
