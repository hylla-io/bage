# Båge

> **Båge** (Swedish for *bow / arc*) — a bidirectional code-graph round-trip file editor.

Status: v0.1.0 — first tagged release (agent-IDE polyglot lib).

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
```

The hash primitives a host mirrors for cross-system agreement are exported:
`bage.Normalize`, `bage.RawHash`, `bage.NormHash`, `bage.RegionHash`, `bage.LangForPath`.

## CLI

```sh
bage apply  --file path --lines 3-5 --text "new text" [--lang go] [--region-hash HASH]
bage rename --file path --line L --col C --new newName   # LSP-driven (needs a language server)
```

`--lang` is optional; empty auto-detects from the file path.

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

## License

See [LICENSE](LICENSE).
