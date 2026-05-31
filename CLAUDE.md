# CLAUDE.md — project guidance for bage

Project-local guidance for working inside the `bage` tree. Global rules (Tillsyn coordination, Section 0 reasoning, evidence sources, worktree hygiene, output style) live at `~/.claude/CLAUDE.md` and are NOT duplicated here.

## Architecture & Cascade Tracking

Freshly-bootstrapped Go-only sibling. The cross-project agent-dispatch + persona architecture (per-persona `settings.json` + `bin/agent-dispatch.sh` + `.claude/hooks/`) was sync'd from the source-of-truth sibling `ta`; the sync file-list + dev bootstrap checklist live in `R_SHIP_HANDOFF.md`. Per `feedback_no_sibling_git_mutations`, orch never runs git here — dev owns git history.

- **Cascade tracking uses the `ta` MCP** (`mcp__ta__*` on `.ta/`-managed records), **NEVER `tillsyn` MCP** — only the `tillsyn` repo has Tillsyn MCP wired. Any leftover textual `mcp__tillsyn__*` ref in a persona body is INERT here.
- **`ta` records are the work-tracking source of truth.** Built-in `TaskCreate`/`TaskUpdate` are fine for granular sub-steps or tiny reminders — anything durable goes in a `ta` record.

## Cascade Methodology — Plan Down, Build Up

Canonical contract: [`CASCADE_METHODOLOGY.md`](CASCADE_METHODOLOGY.md) at repo root (byte-identical with tillsyn — tillsyn is the methodology SOURCE; this sibling consumes it). Key invariants:

1. **PLAN DOWN, BUILD UP.** Plan top-down (a plan node decomposes into child plans + atomic build droplets); build bottom-up (atoms land first, integration nodes follow once their inputs are green).
2. **RECURSE ON ATOMICITY.** 1-2 small code blocks per build droplet, ≤80 LOC incl. tests, ≤3 files. ≥3 distinct production symbols → split.
3. **PER-BRANCH PARALLELISM.** All unblocked work runs in parallel; only `blocked_by` serializes.
4. **DESCENT GATE per branch.** Plan-QA pair (proof + falsification) MUST both PASS before that node spawns children or builds.
5. **DROPLET-LEVEL QA = `mage ci` gate.** No LLM proof/falsification per droplet; the automated gate is enough.
6. **ORCH AUTO-ADVANCE.** Drive the cascade autonomously; don't ask permission per tick.

## Go Development Rules

- **Hexagonal architecture**, interface-first boundaries, dependency inversion.
- **TDD-first** where practical. Ship small tested increments.
- **Smallest concrete design.** No abstraction for hypothetical future variation.
- **Idiomatic Go** — naming, package structure, import grouping (stdlib / third-party / local).
- **Go doc comments** on every top-level declaration and method.
- **Errors**: wrap with `%w`, bubble at clean boundaries, log context-rich failures at adapter/runtime edges, don't swallow.
- **Tests**: `*_test.go` co-located, table-driven, behavior-oriented; `-race` via mage targets.

## Build Verification

Before any `build` action item is `complete`:

1. All relevant mage targets pass (`mage -l` for the list).
2. **NEVER raw Go toolchain** (`go test` / `go build` / `go run` / `go vet`). Always `mage <target>`. If a target has a bug, fix the target — don't bypass.
3. All template-generated QA subtasks completed.

### Canonical 12-target shape (per tillsyn P6 — 2026-05-29)

```
TestFunc(pkg, fn)  builder + build-QA       go test -run "^<Func>$" -count=1 -race <pkg>
TestPkg(pkg)       plan-QA read-only        go test -count=1 <pkg>
Test               closeout/orch            go test ./...
RacePkg(pkg)       build-QA                 go test -race -count=1 <pkg>
Race               closeout/orch            go test -race ./...
FormatFile(file)   builder + build-QA       gofumpt -w <file>
Format             closeout/orch            gofumpt -w .
FormatCheck        ci                       gofumpt -l . && fail if non-empty
VetPkg(pkg)        builder + build-QA       go vet <pkg>
Vet                closeout/orch            go vet ./...
Tidy               orch-only                go mod tidy + diff-exit-code
CI                 closeout/orch            FormatCheck + Vet + (Race+Coverage) + Tidy + Build
```

This shape is enforced across all sibling projects for naming consistency so agents always know the gate name. Hyphenated aliases (`format-check`, `format-file`, `test-func`, `test-pkg`, `race-pkg`, `vet-pkg`) preserved for human ergonomics.

## Hylla discipline — Go-only, primary evidence source

Evidence order for Go work: (1) Hylla (`mcp__hylla__*`) for committed symbols/refs/graphs; (2) `git diff` for uncommitted; (3) Read/Grep/Glob for non-Go and post-edit pre-push Go; (4) Context7 + `go doc` + LSP for external semantics.

**Hylla is Go-only.** Never query for `.toml`, `.json`, `.md`, `.yml`, scripts.

**Push-often + ingest-after-push**: after every commit batch push to origin, then trigger `mcp__hylla__hylla_ingest`. Between push and ingest, fall back to `git log` / `Read`.

Spawn prompts for dispatched `ta-go-*` roles MUST include the Hylla artifact ref `github.com/hylla-io/bage@main`.

## What's missing (bootstrap TODO for dev)

This project was sync'd with the agent infrastructure but is not yet a working Go project. To bring it online, the dev needs to:

1. `git init` + remote setup
2. `go mod init github.com/hylla-io/bage`
3. Bootstrap `magefile.go` to the canonical 12-target shape (see tillsyn or ta for reference)
4. Bootstrap `.github/workflows/ci.yml` calling `mage ci`
5. Add `cmd/bage/main.go` (or whatever entrypoint the project chooses)
6. Fill in project-specific sections in this CLAUDE.md when domain decisions are made (architecture, dependencies, target users, etc.)

This CLAUDE.md is intentionally generic until bage's domain is decided.

## ta CLI usage

- All `ta <read-command>` invocations from dispatched roles MUST pass `--json`.
- `--json` accepted on: `ta get`, `ta list-sections`, `ta schema`, `ta search`.
- **NEVER invoke raw `go test` / `go vet` / `go build` / `gofmt` / `gofumpt`.** Always route through mage.

## MCP server pinning

The `ta` MCP server pins one project per process. Either:

- **Launch Claude Code FROM the active project checkout** (inherits cwd to spawned MCP servers).
- **Or pass `--project <abs-path>`** in the MCP server invocation:

  ```json
  {"mcpServers":{"ta":{"command":"ta","args":["--project","/abs/path/to/project"]}}}
  ```
