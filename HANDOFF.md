# Båge — Session Handoff (for post-compaction resume)

Date: 2026-06-01. Read this first after compaction, then `SPEC.md`, `docs/adr/0001..0003`,
`CONTEXT.md`, and `hylla/polyglot-foundation/BAGE_INTEGRATION_PLAN_ADJUSTMENT.md`.

## What Båge is
A bidirectional code-graph **round-trip file editor** (Go library + `bage` CLI). Standalone
it's an IDE-style edit engine; integrated, **Hylla** links it as a lib so one agent-facing
edit lands in both the graph and the files with **no possible drift** and **lossless concurrency**.
It is built *expressly for Hylla* (graph-driven development) but stays graph-agnostic itself.

## Current state — GREEN
- `mage ci` = 0, `mage race` = 0. **292 tests / 14 packages, ~78.7% coverage.** Nothing committed (dev owns git).
- Packages: `internal/{atomicwrite, normalize, hashing(XXHasher %016x), locator, parser(+treesitter: 14 langs), wal, edit, format, session, lsp, region}`, `cmd/bage` (apply, rename), `pkg/bage` (PUBLIC API).
- Polyglot: Go, TS, TSX, JS, Python, Rust, Java, C, C++, C#, Ruby, JSON, HTML, CSS (parse + round-trip tested; LSP rename proven live with gopls; testcontainers harness exists for containerized LSPs).

## Key decisions (ADRs)
- **0001** — Båge is a linked library (not sidecar, not built-in); files-as-truth two-phase `Prepare/Commit/Rollback` + WAL; the cross-store graph+file coordinator lives in **Hylla**; Båge is **ID-blind / locator-addressed**.
- **0002** — official **CGO** `tree-sitter/go-tree-sitter` (both Båge and Hylla), behind a `ParserPort`, cross-compiled via `zig cc` (Hylla SPEC §1.1.12 must be amended to allow CGO).
- **0003** — **region-anchored edits + concurrency**: `region_hash` *verifies identity + detects conflict*; the *CST/graph re-ingest relocates exactly*; *node identity disambiguates* twins. Per-file lock; **resolve-under-lock at Commit**; benign shift → re-resolve; conflict → hard reject. Matches omp (`can1357/oh-my-pi`) discipline, beats it via exact CST relocation.

## The public library API (how Hylla/IDEs use it) — `pkg/bage`
`Open(Config) (*Editor, error)`; `Editor.{Prepare(ctx,[]Edit,[]FileAnchor)→*Plan, Commit(*Plan)→[]EditResult, Rollback, Apply, Rename(ctx,file,line,col,newName)→*Plan, Recover, Parser()→ParserPort, Close}`. Data types are aliases to `internal/{region,parser,session}` (Region, Edit, FileAnchor, EditResult, Plan, ConflictError, ErrConflict, Lang, …). Edit unit = `region.Edit{Region{Path,StartByte,EndByte,StartLine,EndLine,StartCol,EndCol,RegionHash}, NewText}`. Model **echoes** a shown `region_hash` (omp-style), never computes one, never resends old text; addressing is line-facing/byte-internal/node-via-Hylla.

## Load-bearing facts
- **`region_hash` is over NORMALIZED bytes** (xxHash %016x), matching `HYLLA_NODE_CONTRACT §4`, so Hylla and Båge produce identical strings AND a whitespace reformat doesn't false-conflict. (This was the one real cross-system bug found this session — fixed + tested.)
- **Two file hashes**: `file_raw_hash` gates byte offsets, `file_norm_hash` classifies whitespace-only drift. In the file leg the per-region resolve subsumes them (FileAnchor carried for Hylla's whole-file fast path).
- **Hard errors, never silent**: parse failure / lint failure / unresolvable region_hash → reject. fmt is applied. "build" is consumer policy.
- **Edit re-ingest ≠ full enrichment** (hylla-poly MD §8b): the EditResult-driven re-ingest is structural-only + stages summary context; the LLM enrichment pipeline is a SEPARATE, explicit, batch-level call, gated on all configured checks (lint/vet/test/race/format/tier-2) passing.

## What's DONE
Standalone polyglot round-trip editor with region-anchored, concurrency-safe (resolve-under-lock, no-lost-update, conflict-reject — all proven under `-race`) editing; LSP rename (Go live, others via testcontainers); the public `pkg/bage` API; the Hylla integration contract docs (SPEC §8, ADR-0003, hylla-poly MD §8/§8a/§8b).

## What's LEFT (for "MCP-feature-complete + used as a lib in hylla-poly")
1. **graphify ingest + control-flow verification** (the immediate next step — see below). Verify the wiring on the graph, with the dev viewing graph.html in a browser, confirming readiness for concurrent graph-based editing.
2. **Hylla-side coordinator** (NOT in this repo): node-id→locator resolve-under-lock, drive `bage.session`, do the graph leg, consume `EditResult` for incremental re-ingest, amend SPEC per the plan-adjustment MD. This is what makes the MCP edit tool real.
3. **LSP `didChange` propagation + incremental tree reuse** at the coordinator/LSP layer (file-leg deliberately reparses-under-lock; perf/propagation deferred — SPEC §8.3 note).
4. **More polyglot LSP rename** via the testcontainers harness (add languages as table rows).
5. **End-to-end Hylla+Båge dual-write test** (one MCP edit → graph + files move together, drift-checked, concurrent) — the only test that proves "plugs into Hylla and works."
6. Optional hardening: Swift/Kotlin grammars (community), per-language fmt/lint config wiring, external-edit file watcher, shadow-graph/multi-agent semantics.

## graphify — WIRED + INGESTED (this session). Resume here.
graphify (CLI `~/.local/bin/graphify` v0.8.28; `uv` present) is now a **project-local MCP** in **bage + hylla-poly ONLY** (never global):
- `bage/main/.mcp.json` and `hylla/polyglot-foundation/.mcp.json` each have a `graphify` server: `uv run --with graphifyy --with mcp -m graphify.serve <abs>/graphify-out/graph.json`. bage's `.claude/settings.json` has `enableAllProjectMcpServers: true`, so the `mcp__graphify__*` tools (query_graph, get_node, get_neighbors, get_community, god_nodes, graph_stats, shortest_path, …) **auto-load on the next session restart**.
- Both repos ingested (AST-only, no LLM): **bage = 874 nodes / 1430 edges / 63 communities**; **hylla-poly = 2651 / 3374 / 219**. Artifacts in `graphify-out/{graph.json, graph.html, GRAPH_REPORT.md}`; `graphify-out/` is git-ignored in both.
- Dev views the graph in a browser: `open /Users/evanschultz/Documents/Code/hylla/bage/main/graphify-out/graph.html`.
- Re-ingest after edits: `graphify update .` (AST, no API key). Optional `/graphify` skill (CLI `graphify install --platform claude --project` from inside a repo — writes a skill + appends a `## graphify` section to that repo's CLAUDE.md + a PreToolUse hint hook). NOT installed (would mutate the curated CLAUDE.md); the MCP path was chosen instead.

**ON RESUME (post-compaction + restart):** the graphify MCP tools are live. Use them to walk Båge's control flow on the graph WITH the dev viewing `graph.html` in the browser, and confirm everything is wired for Hylla's concurrent graph-based editing. Then proceed to the remaining integration items in "What's LEFT".

## Conventions (do not drift)
- **omp** = oh-my-pi (`can1357/oh-my-pi`) — drift-discipline benchmark. **NEVER use codebase-memory MCP** (removed by dev); use **graphify**. Gates are **mage only** (never raw go/gofmt/gofumpt/gopls). Orch never runs git (dev owns history). Section 0 reasoning is orchestrator-facing only, never in committed files.
