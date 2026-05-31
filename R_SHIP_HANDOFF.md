# R-SHIP-BAGE — Handoff to Dev

**Date:** 2026-05-30
**Tillsyn refinement:** `64b105df-7e7f-4881-a114-cf88eb6db056` (R-SHIP-BAGE)
**Source-of-truth sibling:** `ta` (architecture) + `tillsyn` (CASCADE_METHODOLOGY.md canon)
**Memory rule:** `feedback_no_sibling_git_mutations` — orch wrote files only; ALL git is yours.

bage is a **fresh Go-only sibling**, treated identically to lagom. Orch laid down the full agent + build infrastructure but cannot `git init` / `go mod init` (yours). This handoff is the bootstrap playbook.

---

## What orch wrote (no git touched)

| Path | What | Source |
|---|---|---|
| `bin/agent-dispatch.sh` + `bin/agent-audit-toon.py` | byte-identical dispatch + audit | ta |
| `.claude/hooks/ta_action_gate.py` + `.claude/hooks/post_tooluse_agent_audit.py` | byte-identical gate + audit hooks | ta |
| `.claude/agents/<persona>/settings.json` × 7 (Go-only) | per-persona tool gates, `mcp__tillsyn__*` stripped | ta |
| `.claude/agents/<persona>.md` × 7 | Go-only personas, Path B 2.2.A (tillsyn refs inert) | ta |
| `.claude/settings.json` | Go-only allow/deny + PreToolUse Bash hook + PostToolUse Agent hook | orch |
| `CASCADE_METHODOLOGY.md` | methodology canon (sha256 `87708e81…`) | tillsyn |
| `CLAUDE.md` | **generic** Go-quality CLAUDE.md (no domain specifics) | orch |
| `magefile.go` | canonical Go-only magefile, BYTE-IDENTICAL with lagom (sha256 `7774bff…`) | orch (canonical) |
| `go.mod` | skeleton: `module github.com/hylla-io/bage` + `go 1.26.2` + laslig require | orch |
| `cmd/bage/main.go` | minimal compilable entrypoint | orch |
| `.gitignore` | standard agent-runtime ignores | orch |

The 7 Go-only personas: ta-go-builder, ta-go-build-qa-proof, ta-go-build-qa-falsification, ta-go-plan-qa-proof, ta-go-plan-qa-falsification, ta-go-planning, ta-closeout.

## The magefile is the canonical 12-target shape

`mage -l` (after bootstrap) shows: Test, TestPkg, TestFunc(pkg,fn), Race, RacePkg, Format, FormatFile, FormatCheck, Vet, VetPkg, Tidy, CI + Build + Clean, with hyphenated aliases. BYTE-IDENTICAL to lagom's magefile (project-agnostic `Build` globs `cmd/*/main.go`).

---

## Bootstrap playbook (YOUR hands — use LATEST versions)

Dev directive: **use the latest versions of everything**. Run from `/Users/evanschultz/Documents/Code/hylla/bage`:

1. **git init + remote**
   ```sh
   git init
   git branch -M main
   gh repo create hylla-io/bage --private --source=. --remote=origin
   ```
2. **Resolve modules at LATEST**
   ```sh
   go get github.com/evanmschultz/laslig@latest
   go mod tidy
   ```
   - Skeleton `go.mod` pins `laslig v0.2.4` as a floor; `@latest` + tidy moves it forward.
   - `mage` auto-installs `gofumpt@latest` on first Format/FormatCheck.
   - Use latest `go` toolchain + `go install github.com/magefile/mage@latest`.
3. **Verify the gate**
   ```sh
   mage ci   # FormatCheck + Vet + Cover (race+cover) + Tidy. 0 tests yet → passes.
   ```
4. **Smoke a persona** (optional) — dispatch `ta-go-builder`, confirm `git commit` is hook-blocked.
5. **Commit + push**
   ```sh
   git add -A
   git commit -m "chore: bootstrap bage — agent infra + canonical magefile + skeleton"
   git push -u origin main
   gh run watch --exit-status   # once ci.yml exists (orch writes it post-init)
   ```
6. **Hylla ingest** (after push + CI green)
   ```
   mcp__hylla__hylla_ingest(source_url="https://github.com/hylla-io/bage.git", ref="<SHA>", branch="main", enrichment_mode="full_enrichment", stream=true)
   ```
7. **Tell orch** the SHA + ingest task id so R-SHIP-BAGE closes. Orch then writes `.github/workflows/ci.yml`.

---

## What goes in CLAUDE.md + how to structure it

`CLAUDE.md` is **intentionally generic** — LOAD-BEARING scaffolding, NO domain content (bage's domain isn't decided). When bage starts for real, fill it in following this structure (mirrors the other siblings):

1. **Title + one-line scope** — what bage IS.
2. **Architecture & Cascade Tracking section** (KEEP) — terse agent-infra sync record + the "ta MCP not tillsyn" rule (the P5 tidy moved the full synced-files list to this handoff).
3. **Cascade Methodology section** (KEEP) — references `CASCADE_METHODOLOGY.md`; ta is the cascade-tracking MCP, NOT tillsyn.
4. **Go Development Rules** (KEEP/extend) — add domain architecture once decided.
5. **Build Verification** (KEEP) — canonical 12-target shape; extend only if bage adds project-specific targets.
6. **Hylla discipline** (KEEP) — update artifact ref to `github.com/hylla-io/bage@main` once the repo exists.
7. **Project Structure** (ADD when domain known) — package table.
8. **Tech Stack** (ADD when domain known) — actual libs.
9. **Domain-specific rules** (ADD).

**P5 light tidy applied (2026-05-30):** undated the `2026-05-29 Architecture Sync (LOAD-BEARING)` header → `## Architecture & Cascade Tracking` (synced-files list moved here to the handoff — it duplicated CLAUDE.md), undated `## Cascade Methodology — Plan Down, Build Up`, added the TaskCreate dual-use note. ~6.3k, recursive flow stays in-file. **The generic skeleton otherwise stands — do NOT pare it further;** fill in domain sections when bage's domain is decided. Stage `CLAUDE.md` with the bootstrap commit.

---

## What's still orch's after you bootstrap

- `.github/workflows/ci.yml` (orch writes post-`go mod init`).
- Final R-SHIP-BAGE verdict comment once you confirm SHA + CI green + ingest.

## Reference siblings

- ta — architecture source-of-truth. sand / valv — Go-only siblings, same persona set + canonical magefile.
