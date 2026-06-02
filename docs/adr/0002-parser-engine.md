---
status: accepted
---

# Tree-sitter via the official CGO Go bindings, behind a ParserPort

## Context

Båge needs incremental tree-sitter parsing to produce CSTs + byte-range locators, and the
same parser must serve Hylla's ingest of byte-addressable formats (one parser, so graph and
files can never disagree on structure). Båge **mutates files**, so a silent misparse
corrupts a user's source — parser fidelity is load-bearing.

Verified facts (2026): the **official** `github.com/tree-sitter/go-tree-sitter` (binding,
v0.25.0 as resolved; v0.24.0 was the prior tag) requires **CGO** and is the canonical/blessed binding (successor to the older
community `smacker/go-tree-sitter`, also CGO). Language grammars live in separate official
repos (e.g. `tree-sitter-go` v0.25.0). The only no-CGO alternatives are `gotreesitter` (a
pure-Go *reimplementation* — ~3.5 months old, release-candidate, ~97% single-author,
fidelity self-attested) and wazero/WASM (immature Go wrappers, incremental undocumented).
`zig cc` makes CGO cross-compilation near-pure-Go-easy (Linux/musl clean; macOS needs SDK +
`-w`; Windows is mingw-dynamic).

## Decision

**Use the official `github.com/tree-sitter/go-tree-sitter` CGO bindings — for both Båge and
Hylla — behind a `ParserPort`** (contract: byte-range-located CST nodes + incremental
reparse + queries). It is the byte-exact reference runtime and the correctness-safe choice
for a file-mutating editor. Cross-compile via `zig cc`.

The `ParserPort` is kept (per the project's hexagonal rule) so the engine is an adapter, but
the official CGO binding is the chosen and intended engine — not a placeholder. gotreesitter
/ WASM are recorded as considered-and-rejected, not as a planned migration.

## Considered options

- **gotreesitter (pure-Go)** — rejected: for a file-mutating editor, "real full tree-sitter"
  means proven structural parity on real corpora; gotreesitter is an unaudited single-author
  release candidate. Not trustworthy on the write path today.
- **wazero/WASM over real grammars** — rejected: immature Go wrappers, undocumented
  incremental support (a hard requirement).
- **`smacker/go-tree-sitter`** — rejected: stale (2024), superseded by the official binding.

## Consequences

- **CGO enters the binary.** This requires amending Hylla SPEC §1.1.12 to permit CGO
  (decoupling "single static binary / no runtime shared-lib" — still satisfied via static
  linking — from "no CGO at build time"). Båge and Hylla both accept CGO. Flagged for the
  Hylla team in `BAGE_INTEGRATION_PLAN_ADJUSTMENT.md`.
- A `zig` C cross-toolchain enters the release pipeline; Linux/musl static is clean,
  macOS/Windows carry minor cross-build friction.
- Grammars are per-language official modules (`tree-sitter-<lang>/bindings/go`); the
  `ParserPort` adapter registers the target set (Go first, then the polyglot set).
- All Tier-1 text formats (code, markdown, toml, yaml, json, html, sql, …) ride this one
  engine via byte-range locate + splice; no per-format editing libraries are needed.
- Manual lifecycle discipline: the binding requires `Close()` on Parser/Tree/Cursor
  (SetFinalizer+CGO); the adapter must own this.
