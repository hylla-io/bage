# Båge

> **Båge** (Swedish for *bow / arc*) — a bidirectional code-graph round-trip file editor.

Status: v0.6.0 — **Rust implementation** (the original Go implementation is archived on the
[`go-legacy`](https://github.com/hylla-io/bage/tree/go-legacy) branch; Go module consumers
keep resolving the existing `v0.4.x`/`v0.5.x` tags). Agent-IDE polyglot lib: surgical edits +
file-lifecycle ops (create / delete / move / batch), structured read (read / show / diagnose)
with `--format text|json|toon`, and a public, machine-branchable error taxonomy.

Båge edits source files **surgically and losslessly**. An agent (or a host like
[Hylla](https://github.com/hylla-io)) targets a content-anchored *region* of a file, sends
only the replacement text, and Båge resolves the region under a per-file lock, applies the
edit through a parse/format/lint gate, and writes it back atomically — or rejects it. It
never corrupts: an edit that does not cleanly resolve is refused, not misapplied.

It is a **library first** (the `bage` crate) and ships a thin standalone **CLI** (the `bage`
binary). Standalone it is an IDE-style edit engine; integrated, a host drives the library so
a single agent-facing edit lands in both a graph and the files with no possible drift.

## Why

- **Lossless round-trip.** Every file type opens and round-trips byte-for-byte — 20 real
  tree-sitter grammars, with a grammar-free text fallback for everything else. Verified by
  property tests.
- **Reject, never corrupt.** Region edits are anchored by a content hash (`region_hash`,
  omp-style); a drifted or ambiguous target is rejected. Proven under concurrent commits.
- **Minimal context.** The model is shown a region and echoes its hash — it never resends
  the old text or computes a hash. Smallest possible edit payload.
- **Concurrency-safe.** Per-file lock with resolve-under-lock at commit: concurrent edits to
  the same file serialize (no lost update); different files run in parallel.
- **Cross-system hash contract.** `normalize` + xxHash64 `{:016x}` digests are byte-identical
  with Hylla (and with the archived Go implementation) — pinned by parity-vector tests.

## Install

```sh
cargo build --release        # → target/release/bage (no CGO, no C toolchain gymnastics)
```

## Library

```rust
use bage::editor::{Config, Editor};

let ed = Editor::open(Config { wal_dir: dir.into(), ..Default::default() })?; // lang optional → auto-detect per file
let plan = ed.prepare(&edits, &anchors)?;
let results = ed.commit(&plan)?;             // Vec<EditResult>: new hashes + line spans
```

Read-only inspection without opening an editor:

```rust
let of = bage::inspect::open_file("main.go")?;   // auto-detects language, parses
let syms = bage::inspect::outline(&of.tree);     // declarations with byte + line ranges
let blocks = bage::inspect::read_blocks(&of, true); // blocks + region_hash + content
```

Structured read through an editor — whole-file, by symbol, by line, or by byte range:

```rust
let res = ed.read("main.go", &ReadOptions { symbol: "Greet".into(), include_content: true, ..Default::default() })?;
// res.blocks[i] = {kind, name, start_line, end_line, start_byte, end_byte, region_hash, content}
```

The hash primitives a host mirrors for cross-system agreement are exported:
`bage::normalize::normalize`, `bage::hashing::{raw_hash, norm_hash}`,
`bage::region::hash_region`, `bage::parser::Lang::for_path`.

Errors carry a machine-branchable kind so a wrapper never parses English:
`SessionError::kind()` → `conflict | drift | exists | not-found | usage | io`, and
`session::envelope(&err)` → `ErrorEnvelope { kind, path, message }` (JSON/TOON-serializable).

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
- **LSP rename:** UTF-16-aware client driving any stdio language server (gopls-verified
  end-to-end; the Go-era container matrix is a follow-up on this branch).

## Build gates

All via cargo:

```sh
cargo fmt --check                          # formatting
cargo clippy --all-targets -- -D warnings  # lints
cargo test                                 # unit + property + concurrency + TOON goldens
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
already caught a line-newline edit bug, a missing `--text-file` flag, and (during the Rust
rewrite) a Go-side TOON envelope bug. When dogfooding sharpens one language, the fix lands
with test parity across the other file types so the rest never falls behind.

## License

**MIT** — see [LICENSE](LICENSE). Permissive: use, modify, and distribute freely (including
commercially) with attribution; provided as-is, no warranty.
