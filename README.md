# Båge

> **Båge** (Swedish for *bow / arc*) — a bidirectional code-graph round-trip file editor.

Status: v0.4.0 — agent-IDE polyglot lib: surgical edits + file-lifecycle ops (create / delete / move / batch), structured read (read / show / diagnose) with `--format text|json|toon`, and a public, machine-branchable error taxonomy.

Båge edits source files **surgically and losslessly**. An agent (or a host like
[Hylla](https://github.com/hylla-io)) targets a content-anchored *region* of a file, sends
only the replacement text, and Båge resolves the region under a per-file lock, applies the
edit through a parse/format/lint gate, and writes it back atomically — or rejects it. It
never corrupts: an edit that does not cleanly resolve is refused, not misapplied.

It is a **library first** (`pkg/bage`) and ships a thin standalone **CLI** (`cmd/bage`).
Standalone it is an IDE-style edit engine; integrated, a host links it as a Go library so a
single agent-facing edit lands in both a graph and the files with no possible drift.

## Why

- **Lossless round-trip.** Every file type opens and round-trips byte-for-byte — 20 real
  tree-sitter grammars, with a grammar-free text fallback for everything else. Verified by
  property fuzzing.
- **Reject, never corrupt.** Region edits are anchored by a content hash (`region_hash`,
  omp-style); a drifted or ambiguous target is rejected. Proven under `-race`.
- **Minimal context.** The model is shown a region and echoes its hash — it never resends
  the old text or computes a hash. Smallest possible edit payload.
- **Concurrency-safe.** Per-file lock with resolve-under-lock at commit: concurrent edits to
  the same file serialize (no lost update); different files run in parallel.

## Install

```sh
go get github.com/hylla-io/bage@latest      # library
go build -o bin/bage ./cmd/bage             # CLI (requires CGO + a C toolchain)
```

## Library

```go
ed, _ := bage.Open(bage.Config{WALDir: dir}) // Lang optional → auto-detect per file
plan, _ := ed.Prepare(ctx, edits, anchors)
results, _ := ed.Commit(plan)                // []EditResult: new hashes + line spans
```

Read-only inspection without opening an editor:

```go
of, _ := bage.OpenFile(ctx, "main.go")       // auto-detects language, parses
defer of.Close()
syms := bage.Outline(of.Tree)                // declarations with byte + line ranges
blocks := bage.ReadBlocks(of, true)          // blocks + region_hash + content (Hylla holds the OpenedFile)
```

Structured read through an editor — whole-file, by `Symbol`, by line, or by byte range:

```go
res, _ := ed.Read(ctx, "main.go", bage.ReadOptions{Symbol: "Greet", IncludeContent: true})
// res.Blocks[i] = {Kind, Name, StartLine, EndLine, StartByte, EndByte, RegionHash, Content}
```

The hash primitives a host mirrors for cross-system agreement are exported:
`bage.Normalize`, `bage.RawHash`, `bage.NormHash`, `bage.RegionHash`, `bage.LangForPath`.

Errors carry a machine-branchable kind so a wrapper never parses English:
`bage.KindOf(err)` → `conflict | drift | exists | not-found | usage | io`, and
`bage.Envelope(err)` → `bage.ErrorEnvelope{Kind, Path, Message}` (JSON/TOON-serializable).

## CLI

```sh
bage apply    --file path --lines 3-5 --text "new text" [--lang go] [--region-hash HASH] [--format text|json|toon]
bage create   --file path --text "..." [--lang go] [--format text|json|toon]       # new file (rejects if exists)
bage delete   --file path [--raw-hash HASH] [--format text|json|toon]              # delete, gated on raw_hash
bage move     --from path --to path2 [--raw-hash HASH] [--format text|json|toon]   # relocate (drift + no-clobber)
bage read     --file path [--symbol NAME | --line L | --lines A-B | --start S --end E] [--content] [--format text|json|toon]
bage show     --file path [--format text|json|toon]             # read view: blocks + region_hash map
bage diagnose --file path [--lsp CMD] [--format text|json|toon] # parse-health (+ optional LSP diagnostics)
bage rename   --file path --line L --col C --new newName [--format text|json|toon] # LSP-driven (needs a server)
```

`--lang` is optional; empty auto-detects from the file path. `--format` (every verb, default
`text`) selects the output encoding: `text` (human), `json` (interop), or `toon` (compact
tabular, fewest tokens). On failure it encodes a `{kind, path, message}` error envelope
(`kind` ∈ `conflict | drift | exists | not-found | usage | io`), so a wrapper branches on
`kind` instead of parsing text.

## Languages

- **Grammars (20, parse + round-trip):** Go, TypeScript, TSX, JavaScript, Python, Rust, Java,
  C, C++, C#, Ruby, JSON, HTML, CSS, YAML, TOML, XML, Makefile, Bash, Markdown.
- **Text fallback (lossless, no grammar):** MDX, SCSS, Dockerfile, `.txt`, dotfiles, anything else.
- **LSP rename (10, container-verified):** Go, Python, TypeScript, TSX, JavaScript, JSX, Rust,
  C, C++, Swift. (C#/Java rows defined, pending hardening.)

## Build gates

All via [mage](https://magefile.org) — never the raw Go toolchain:

```sh
mage ci      # format-check + vet + race + coverage + tidy + build
mage lsp      # containerized LSP-rename suite (requires Docker)
mage fuzz     # property fuzzing (normalize idempotency, text-fallback losslessness)
```

## Dogfooding

Båge edits Båge. Every change to a file in this repo is made **through Båge itself** —
`bage apply` for edits, `bage create` for new files, `bage delete` / `bage move` for
lifecycle, `bage rename` for symbols — not an external editor, so the project is its own
first integration test. `bage show` exposes a file's addressable blocks + `region_hash` map
(the read view an agent edits against), and `bage diagnose` surfaces parse/LSP problems. The
built-in editor of your agent harness is now a **fallback only** for one thing:

- **Raw whole-file reading** — Båge has no `cat` that dumps raw bytes (`bage show` gives the
  structured block + hash view); use the harness's reader when you need the raw content.

Everything else — surgical edits to existing source, docs, config, even this README — goes
through `bage apply` with a byte/line region and a `--text` / `--text-file` replacement, so
the round-trip and region-anchor machinery is exercised on real content on every commit.

Friction or bugs found while dogfooding are logged in
[`BAGE_DOGFOOD_FINDINGS.md`](BAGE_DOGFOOD_FINDINGS.md) and fixed test-first. This loop has
already caught a line-newline edit bug and a missing `--text-file` flag. When dogfooding
sharpens one language, the fix lands with test parity across the other file types so the
rest never falls behind.

## License

**MIT** — see [LICENSE](LICENSE). Permissive: use, modify, and distribute freely (including
commercially) with attribution; provided as-is, no warranty.
