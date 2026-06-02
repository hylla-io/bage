# Contributing to Båge

Thanks for helping. Båge is a surgical, lossless file editor — correctness and
"reject, never corrupt" come before features.

## Ground rules

- **Build gates are [mage](https://magefile.org) only.** Never run the raw Go
  toolchain (`go test` / `go build` / `go vet` / `gofmt` / `gofumpt`). If a gate
  is wrong, fix the mage target — don't bypass it.
- **TDD-first.** Ship small, tested increments. New behavior lands with a
  behavior-oriented, table-driven test.
- **Idiomatic Go**, interface-first boundaries, smallest concrete design — no
  abstraction for hypothetical future variation.
- **Errors** wrap with `%w` and bubble at clean boundaries; never swallow.
- **Go doc comments** on every exported declaration.

## Before you push

```sh
mage ci      # format-check + vet + race + coverage + tidy + build (must pass)
mage lsp      # containerized LSP-rename suite — run if you touched the LSP path (needs Docker)
mage fuzz     # property fuzzing — run if you touched normalize/hashing/parser
```

`mage -l` lists every target.

## Commits & PRs

- **Conventional commits, subject line only**: `type(scope): message` (`feat`,
  `fix`, `refactor`, `chore`, `docs`, `test`, `ci`). No body, no trailing period,
  under ~72 chars. Per-change detail goes in the PR description.
- **All changes go through a PR to `main`**; CI (`mage ci`) must be green.
- Never `--amend` or force-push shared history — a bad message gets a follow-on
  commit.

## Adding a language

- **Grammar (parse + round-trip):** add the `tree-sitter-<lang>/bindings/go`
  module, register it in `internal/parser/treesitter`, add a `Lang` + a
  `LangForPath` mapping + a real fixture in `TestParsePolyglot`. If no Go binding
  exists, the text fallback already covers it losslessly.
- **LSP rename:** add one `lspServerCase` row in `internal/lsp/container_test.go`
  (image + install/serve command + source fixture + rename position). The socat
  bridge drives any stdio language server; no driver changes needed.

## Architecture

See [`code_graph_architecture.md`](code_graph_architecture.md), [`SPEC.md`](SPEC.md),
and `docs/adr/` for the region-anchored edit model, the two-hash drift discipline,
and the concurrency design.
