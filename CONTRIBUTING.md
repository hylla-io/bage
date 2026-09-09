# Contributing to Båge

Thanks for helping. Båge is a surgical, lossless file editor — correctness and
"reject, never corrupt" come before features.

## Ground rules

- **TDD-first.** Ship small, tested increments. New behavior lands with a
  behavior-oriented test, table-driven where the cases are enumerable.
- **Idiomatic Rust**, smallest concrete design, traits only at real seams — no
  abstraction for hypothetical future variation.
- **Errors** are typed (`thiserror`) and bubble at clean boundaries; never
  swallow one.
- **Rustdoc on every public item.** Say *why*, not *what* — the code already
  states what it does.
- **Never claim a guarantee the code does not hold.** A doc comment that
  overstates coverage stops the next person from looking.

## Build and test

```sh
cargo build
cargo test                 # unit tests + the integration tests under tests/
cargo run -- --help        # the CLI
```

## Before you push

These three commands ARE the gate — `.github/workflows/ci.yml` runs exactly
them, in this order, as the required `check` status on `main`:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`.github/workflows/audit.yml` additionally runs `cargo audit` against the
RustSec advisory database when a manifest changes, weekly, and on demand. It is
advisory and does not gate a merge.

### Docker-gated LSP suite

`tests/lsp_containers.rs` drives real language servers (gopls, pyright, clangd)
in containers over stdio. It is skipped unless you opt in, because it needs
Docker and pulls images:

```sh
BAGE_DOCKER_LSP=1 cargo test --test lsp_containers -- --nocapture
```

Run it if you touched the LSP path. Without the variable the cases print a skip
line and pass — that is a reported skip, not evidence.

## Commits and PRs

- **Conventional commits, subject line only**: `type(scope): message` (`feat`,
  `fix`, `refactor`, `chore`, `docs`, `test`, `ci`). No body, no trailing
  period, under ~72 chars. Per-change detail goes in the PR description.
- **All changes go through a PR to `main`**, and CI must be green.
- Never `--amend` or force-push shared history — a bad message gets a follow-on
  commit.

## Adding a language

Grammars are ordinary Cargo dependencies; there are no bindings to vendor and
no build script to touch.

1. Add the `tree-sitter-<lang>` crate to `[dependencies]` in `Cargo.toml`.
2. Add a `Lang` variant in `src/parser.rs`, and extend `Lang::ALL`, `Lang::name`
   and `Lang::for_path` (extension, or basename for extensionless build files).
3. Register the grammar in `parser::Adapter::new`, mapping the new `Lang` to the
   crate's `LANGUAGE` constant. A crate exposing several languages has several
   constants — `tree-sitter-typescript` supplies both TypeScript and TSX.
4. Add a real snippet to `parse_polyglot_all_grammars` and a path case to
   `lang_for_path_is_total`, both in `src/parser.rs`.

If no grammar crate exists for the language, do nothing: `Lang::for_path` is
total and unknown types resolve to `Lang::Text`, the grammar-free fallback that
already round-trips any file losslessly.

Adding a language to the tier-2, facts, or scopes surfaces is separate and
optional — those carry per-language tables, and a language absent from a table
returns an empty result or a typed `Unsupported`, never an error. See
`HYLLA_NODE_CONTRACT.md` §8b–§8d for what each surface promises.

### Adding an LSP rename case

Add one `Case` row in `tests/lsp_containers.rs`: image, server argv, fixture
files, rename position, and the files the resulting `WorkspaceEdit` must touch.
The container is spoken to over stdio through `docker run -i --rm`, so no
bridge or driver change is needed.

## Architecture

See [`SPEC.md`](SPEC.md), [`code_graph_architecture.md`](code_graph_architecture.md),
[`HYLLA_NODE_CONTRACT.md`](HYLLA_NODE_CONTRACT.md), and `docs/adr/` for the
region-anchored edit model, the two-hash drift discipline, and the concurrency
design.

## Security

Report vulnerabilities per [`SECURITY.md`](SECURITY.md) — not through a public
issue.

## Licence

Båge is MIT licensed ([`LICENSE`](LICENSE)). Contributions are accepted under
the same terms.
