# CLAUDE.md — project guidance for bage

Project-local guidance for working inside the `bage` tree. Global rules (Tillsyn coordination, Section 0 reasoning, evidence sources, worktree hygiene, output style) live at `~/.claude/CLAUDE.md` and are NOT duplicated here.

**Shorthand / domain language:** see [`CONTEXT.md`](CONTEXT.md) for the project glossary. In particular **`omp`** = oh-my-pi (`can1357/oh-my-pi`), the hash-anchored editor Båge's drift discipline must match in capability + safety; **`region_hash`** is Båge's per-block content anchor. All dispatched subagents working on edit/drift/concurrency must speak this shorthand.

## Architecture & Cascade Tracking

**Rust implementation** (cargo crate at the repo root; the original Go implementation is archived on the `go-legacy` branch — any Go-side fix lands THERE, never on `main`). The cross-project agent-dispatch + persona architecture (per-persona `settings.json` + `bin/agent-dispatch.sh` + `.claude/hooks/`) was sync'd from the source-of-truth sibling `ta`; the sync file-list + dev bootstrap checklist live in `R_SHIP_HANDOFF.md`. Per separate-repo discipline, `bage` has its own `.git` + remote: **orch DOES commit and push `bage`** to bage's own origin. The rule is only that bage's git stays SEPARATE — never bundle bage changes into a sibling repo's commit (`hylla`, `ta`), and run any sibling git op against that sibling's own checkout/remote.

- **Cascade tracking uses the `ta` MCP** (`mcp__ta__*` on `.ta/`-managed records), **NEVER `tillsyn` MCP** — only the `tillsyn` repo has Tillsyn MCP wired. Any leftover textual `mcp__tillsyn__*` ref in a persona body is INERT here.
- **`ta` records are the work-tracking source of truth.** Built-in `TaskCreate`/`TaskUpdate` are fine for granular sub-steps or tiny reminders — anything durable goes in a `ta` record.

## Cascade Methodology — Plan Down, Build Up

Canonical contract: [`CASCADE_METHODOLOGY.md`](CASCADE_METHODOLOGY.md) at repo root (byte-identical with tillsyn — tillsyn is the methodology SOURCE; this sibling consumes it). Key invariants:

1. **PLAN DOWN, BUILD UP.** Plan top-down (a plan node decomposes into child plans + atomic build droplets); build bottom-up (atoms land first, integration nodes follow once their inputs are green).
2. **RECURSE ON ATOMICITY.** 1-2 small code blocks per build droplet, ≤80 LOC incl. tests, ≤3 files. ≥3 distinct production symbols → split.
3. **PER-BRANCH PARALLELISM.** All unblocked work runs in parallel; only `blocked_by` serializes.
4. **DESCENT GATE per branch.** Plan-QA pair (proof + falsification) MUST both PASS before that node spawns children or builds.
5. **DROPLET-LEVEL QA = CI gate.** No LLM proof/falsification per droplet; the automated gate is enough.
6. **ORCH AUTO-ADVANCE.** Drive the cascade autonomously; don't ask permission per tick.

## Rust Development Rules

- **Hexagonal architecture**, trait-first boundaries where a seam earns its keep, dependency inversion (`ParserPort`, `Hasher`, `Formatter`/`Linter`).
- **TDD-first** where practical. Ship small tested increments.
- **Smallest concrete design.** No abstraction for hypothetical future variation.
- **Idiomatic Rust** — enums over tag+nilable-payload, RAII over idempotent close, `thiserror` error enums with a `kind()` classification, `usize` making invalid offsets unrepresentable.
- **Doc comments (`///` / `//!`)** on every public item.
- **Errors**: typed per module, wrapped with `#[from]`/`#[source]`, classified to the public `Kind` taxonomy at the session boundary; never swallowed.
- **Tests**: co-located `#[cfg(test)]` modules, behavior-oriented, table-style where it fits; concurrency claims exercised with real threads; cross-system contracts pinned by parity vectors generated from the reference binary.

## Build Verification

Before any `build` action item is `complete`, the full cargo gate must pass:

```sh
cargo fmt --check                          # formatting (CI-enforced)
cargo clippy --all-targets -- -D warnings  # lints (CI-enforced)
cargo test                                 # unit + property + concurrency + TOON goldens
```

CI (`.github/workflows/ci.yml`, job key `check` — the required status on `main`) runs exactly these three. The release workflow runs the same gate on tag push.

## Evidence discipline

Hylla ingest is Go-only, so **Hylla MCP does not index this repo's Rust source**. Evidence order for Rust work: (1) `git diff` / `git log` for changes; (2) Read/Grep/Glob + LSP (rust-analyzer) for code; (3) Context7 + docs.rs for external crate semantics. The archived Go implementation on `go-legacy` (and the Hylla artifact ref `github.com/hylla-io/bage@go-legacy`) remains the byte-contract REFERENCE for normalize/hash parity questions.

## Dogfooding — use bage to edit bage

Orch AND subagents edit files in THIS repo with **bage itself** (`bage apply` / `bage rename`, built via `cargo build --release` → `target/release/bage`). Built-in Edit is a **fallback only** — used when bage genuinely cannot do it.

- **Edits to existing files → `bage apply`** (`--file`; addressing: `--line`/`--lines`/`--start`/`--end`, `--all` whole-file replace, or an insertion point `--append`/`--before-line`/`--after-line`; `--text`/`--text-file`, optional `--region-hash`, `--lang` empty = auto-detect). **New files → `bage create`.** Lifecycle → `bage delete` / `bage move` (raw_hash-gated). Symbol renames → `bage rename` (LSP). Move/duplicate a region → `bage cut` / `bage copy` (`--symbol`/`--line`/`--lines`/`--start`/`--end`, optional `--clip` file clipboard) then `bage paste` (`--at-byte`/`--append`/`--before-line`/`--after-line`, from `--text`/`--text-file`/`--clip`). Inspect → `bage show` (block + `region_hash` map) / `bage read` (whole-file/`--symbol`/`--line`/byte-range, optional `--content`, `--format text|json|toon`) / `bage diagnose` (parse/LSP health).
- **Honest constraint:** bage **creates** (`bage create`), **reads content** (`bage read --content`), **shows** structure (`bage show`), and does **lifecycle** (delete/move) — so built-in Read/Write is a fallback only when bage genuinely cannot do it.
- **Report findings:** any bage friction/bug goes in `BAGE_DOGFOOD_FINDINGS.md`, then gets fixed (TDD).
- **Test parity:** when dogfooding sharpens one language, update tests + code for ALL other file types/langs to match — do not let them fall behind.
- Goal: dogfood until bage is proven excellent for every lang/file type, and right for what Hylla needs (keep `hylla/polyglot-foundation/BAGE_UPDATE.md` current).

## ta CLI usage

- All `ta <read-command>` invocations from dispatched roles MUST pass `--json`.
- `--json` accepted on: `ta get`, `ta list-sections`, `ta schema`, `ta search`.
- Build/test verification goes through the cargo gate above — do not invent ad-hoc scripts around it.

## MCP server pinning

The `ta` MCP server pins one project per process. Either:

- **Launch Claude Code FROM the active project checkout** (inherits cwd to spawned MCP servers).
- **Or pass `--project <abs-path>`** in the MCP server invocation:

  ```json
  {"mcpServers":{"ta":{"command":"ta","args":["--project","/abs/path/to/project"]}}}
  ```

## Git workflow & versioning

- **All work goes on a branch → PR → CI green → merge.** Never push code directly to `main` (the repo is public; the PR + CI check is the gate).
- **Delete merged branches immediately — local AND remote**: `git branch -d <branch> && git push origin --delete <branch>`. Never leave a merged branch lying around. (Exception: `go-legacy` is a PERMANENT archive branch — never delete it.)
- **Semver bumps follow CODE, never docs.** Tag a new version on `main` after a merged PR that changes CODE: a feature → bump MINOR (`v0.N.0`), a fix → bump PATCH (`v0.x.P`); while pre-1.0, a breaking change also bumps MINOR. A **doc-only** PR (README, CONTRIBUTING, CLAUDE.md, SPEC, code comments) gets **NO version bump and NO tag**. Tag: `git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`.
