# Cascade Methodology

This is the canonical methodology document for the **Cascade SDD** approach Tillsyn implements and dogfoods. It describes how work decomposes top-down through a recursive cascade tree, how each node is classified along three orthogonal axes, how agents reason and converge, and what gates keep the cascade honest.

This document is a **skeleton** today. Each section captures the methodology's *shape* in 1-3 paragraphs, with placeholder markers indicating where post-dogfood measurement and benchmark data will be filled in. Adopters can read this end-to-end to understand the methodology; depth and worked benchmarks land after the first dogfood cycles produce real numbers.

For the canonical vocabulary used throughout (cascade / drop / segment / confluence / droplet, the closed `kind` enum, the `role` enum), see `WIKI.md` § "Cascade Vocabulary" — that section is the single source of truth and this document cross-references rather than redefines it. Companion docs: `AGENTS_CONFIG.md` (per-machine `agents.toml` configuration reference) and `GDD_METHODOLOGY.md` (Graph-Driven Development methodology, which composes with this one post-Hylla-rev).

**Vocabulary note (2026-05-21):** Level-1 nodes (direct children of the project) are **cascades** — whole cascade trees of work that decompose into drops, segments, confluences, and droplets. **Drop** means a vertical decomposition step BELOW level-1; the level-1 unit is the cascade itself. Adding `cascade` as the 5th `structural_type` enum value is tracked at Tillsyn action_item `62569299-6522-401e-a15b-c6f61e2dc609`; until that Go work lands, level-1 nodes carry `structural_type=drop` as a placeholder.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Plan Down, Build Up

The methodology's spine is a single rule: **plan top-down, build bottom-up.** Planning starts at the highest level of the work — a cascade, a feature, a release — and decomposes recursively into drops, sub-drops, segments, and finally atomic build droplets. Building inverts this: the smallest droplets land first, integration nodes follow once their inputs are green, and higher-level deliverables emerge from the bottom up. This is not a waterfall — every level of decomposition has its own QA pair, every level can fail and trigger a wipe-and-replan, and the recursion depth is bounded only by atomic-droplet sizing rules set per-template.

There is **no cap on the number of children at any planning level.** A planner is free to emit two children or twenty, depending on what the work needs. The only hard constraint is *atomic-droplet sizing* — each leaf `build` droplet must be small enough that one builder agent can finish it cleanly in one shot (the till-go + till-fe templates default to **1-2 code blocks / ≤80 LOC + tests**, but those numbers are template-defined, not methodology-hardcoded; adopters running other templates may differ). When work exceeds the atomic budget, planners emit a `kind=plan` sub-plan child instead of an oversize `kind=build` child, and the sub-plan recurses with its own planner agent. **Multi-level decomposition is the norm, not the exception** — a 3-block "build droplet" is the anti-pattern; emit a kind=plan child instead and let a sub-planner decide whether the work is actually 3 blocks or genuinely 2 sub-plans of 1-2 blocks each. Default to recursion when uncertain.

**Atomic sizing is MEASURED, not labelled.** "Small enough for one shot" is operationalized as a COUNT so planners and plan-QA apply the same test instead of trusting a self-assigned label. A *code block* is one new/changed top-level production symbol (a type, a function, a method) OR one cohesive same-purpose edit cluster — a new type, a new helper, and a rewrite of a *different* function are SEPARATE blocks, never folded under one label. Before emitting a `build` droplet a planner COUNTS the distinct new/changed production symbols it names (tests excluded from the count) and estimates its diff size; a droplet exceeding the template's budget (for till-go / till-fe: ≥3 distinct production symbols, or >80 LOC, or >3 production files) is over-budget and becomes a `kind=plan` sub-plan, never an oversize droplet. **Plan-QA-falsification re-derives this count from the droplet's own spec and FAILS the plan on any over-budget droplet — it never accepts the planner's sizing label on faith — and on any plan AMENDMENT it re-measures EVERY droplet, not just the changed one** (a sibling's stale budget claim does not survive a plan edit). This is the enforcement that makes the atomic-sizing cap load-bearing rather than aspirational: the specific thresholds are template-defined, but the measure-don't-label discipline is methodology-level. Two corollaries make it bite: (i) plan-QA-proof STATES each droplet's prod-LOC and test-LOC *separately* (`d3_writer: ~90 prod + ~120 test = 210 ✗ SPLIT`) so the size estimate is auditable, never hand-waved; (ii) **"one coherent concern" / "a single cohesive function" / "a non-separable unit" is NOT an exception to the budget** — a droplet adding a whole new symbol plus its full test suite is almost always over-budget and splits (the symbol + 1–2 happy-path tests as one droplet, edge/table tests as a follow-on). This guards a twice-observed failure mode: a planner labels a multi-symbol droplet "one non-separable unit," QA estimates its size low and accepts the label — both a 340–460-LOC droplet batch and a ~150-LOC / 4-symbol droplet would have been caught by a discrete symbol-count plus a stated prod-vs-test LOC breakdown.

**The tree is ASYMMETRIC by design.** Branches nest as deep as each sub-goal needs — depth is per-branch, not uniform. A shared interface / type / helper that must exist before several siblings can build is emitted as a SHALLOW leaf node (often just 1-2 blocks), and the deeper branches that consume it carry a `blocked_by` edge to it; meanwhile an unrelated sibling sub-goal may recurse several levels deeper. Planners decompose to where each branch's atomicity actually bottoms out, never to a fixed depth. Real cross-branch ordering is always expressed as `blocked_by` (a concrete shared file/package or a must-exist-first symbol), never as artificial nesting or forced serialization.

The build phase reverses the flow. Atomic droplets at the deepest level run first, in parallel where their `blocked_by` graph allows. Their outputs feed integration / confluence nodes that merge sibling streams. Each level's QA pair gates the next: a level-2 plan cannot be marked `complete` until every level-3 droplet under it is `complete`, and the level-2's own plan-QA-proof + plan-QA-falsification both PASS. This bottom-up assembly is what produces "atoms first, integration next" — the methodology's hedge against integration risk, because every atom is independently verified before it joins anything larger.

**Parallelism is per-branch, not per-phase — every role, every level, runs concurrently across code-independent branches.** The descent gate ("a planner's plan-QA pair PASSES before it launches its child planners", see QA Placement below) serializes only ONE branch's depth — it never serializes the tree. Concretely: (1) sibling sub-planners for code-independent branches dispatch **concurrently** (decomposition fans out in parallel across siblings; only each branch's own depth waits on that branch's plan-QA); (2) plan-QA pairs run in parallel **up the tree** — at any moment many nodes' proof+falsification twins are in flight across different branches; (3) a node's builders launch **in parallel** as soon as THAT node's plan-QA is green, even while a sibling subtree is still decomposing or plan-QA'ing — builds are gated per-subtree, never on a global "all planning done" barrier; (4) build-QA pairs run in parallel up the tree the same way; (5) across code-independent branches, planners, plan-QA, builders, and build-QA can ALL be in flight at once. The only two ordering constraints are `blocked_by` (real code dependency) and the per-node gates (plan-QA-pair before that node's child planners + builds; `mage ci` + build-QA before that build completes). The orchestrator's job is to keep every unblocked node of every kind moving simultaneously.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Subagent Discipline (2026-05-27)

The canonical source is `feedback_subagent_scope_tightening.md` in the orchestrator's persistent memory; this section mirrors the load-bearing rules for adopters reading the methodology end-to-end. The 2026-05-27 dogfood cycle surfaced two failure modes that hardened into rules: (a) builders silently drop spec scope and self-grade BUILD COMPLETE (B.8 anti-pattern); (b) plan-QA misses upstream dependencies because it doesn't read integration-seam TODOs (B.8 plan-QA missed `spawn.go:393-410` deferring `ResolveAgentPath`). The rules below close both holes.

**Per-persona test surface — minimum only.** Every persona has a SPECIFIC permitted test target; anything broader is a scope breach.
- **Planners**: NO test execution. Specify test commands for builders to run; do not execute.
- **Plan-QA (proof + falsification)**: `mage test-pkg <full-import-path>` for read-only verification of a plan's code claim. NEVER `mage ci` or `mage test-func`.
- **Builders**: `mage test-func <full-import-path> <TestFuncName>` for EACH new/modified test func they wrote. NEVER `mage test-pkg`, `mage ci`, `mage build`, raw `go test`/`go build`/`go vet`, `gofmt`/`gofumpt`, `go list`. `mage format` allowed ONCE at the end. Orch runs the batch `mage ci`.
- **Build-QA (proof + falsification)**: `mage test-func <full-import-path> <SpecificFunc>` for the specific funcs they verify / attack-test. NEVER `mage test-pkg`, `mage ci`, raw `go *`, `mage build`.
- **Closeout**: `mage ci` ONCE (unique role privilege; cascade-end final gate; no concurrent builders).

**Hylla mandate for planners + plan-QA.** Use `mcp__hylla__hylla_search` / `hylla_node_full` / `hylla_search_keyword` / `hylla_refs_find` / `hylla_graph_nav` BEFORE Read/LSP for any committed Go code understanding. Zero Hylla calls in the closing `## Hylla Feedback` = automatic FAIL. Fall-back to Read/LSP is allowed only when (a) Hylla MCP is offline or (b) the queried path is stale per `git diff` — and the specific reason MUST be recorded in `## Hylla Feedback`.

**Plan-QA-falsification — Rule 3.5: hunt deferred-infrastructure TODOs at integration seams.** For EVERY integration seam the plan wires (resolve seam, dispatch seam, populate site, hook site), `hylla_node_full` the seam's surrounding code (~30 lines either side of the wire point). Surface every inline `// TODO`, `// DEFERRED`, `// follow-up droplet`, `// not yet`, "blocked on" comment as a `## Critical Findings` entry. **Any plan that wires a seam with an active deferral is FAIL** — the build will dead-end on un-landed infrastructure. PLUS family-level existence checks: when the plan claims function X exists/doesn't, query Hylla for sibling/caller/called-by symbols (the FAMILY X is part of) — partial families are common planning traps.

**Failure-attribution rule (sibling-WIP coexistence).** Parallel builders share the worktree. When any `mage test-*` returns an error, classify BEFORE acting:
1. Compile/test error in a file OUTSIDE your declared `paths` → report `BLOCKED-by-sibling-WIP` in closing comment with file path + line + error text; STOP, never edit it. Orch routes to the responsible droplet.
2. Compile/test error inside your `paths` or in your declared test funcs → MINE; attack it.
3. Test failure in a func NOT yours → observation only in closing comment; DO NOT touch.

The rule preserves cross-droplet parallelism by serializing only when there's a real conflict. Without it, agents misclassify sibling-caused failures as their own (or vice versa) and the orchestrator can't route the fix.

**No self-rescoping.** If work would exceed the atomicity budget (1-2 small code blocks, >80 prod LOC, >3 prod files, or ≥3 distinct top-level production symbols), STOP and report BLOCKED for re-split. **NEVER ship partial work + grade BUILD COMPLETE.** This is the load-bearing rule — silently dropping scope while claiming completion poisons the cascade's audit trail and ships un-shippable code into integration. The B.8 cascade-of-2026-05-27 anti-example: builder dropped the populate-seam wiring + self-graded COMPLETE → had to be superseded after the fact.

**Closing-comment veracity (`## Hylla Feedback` + `## Tools Used` MANDATORY).** Every closing comment from every subagent role MUST list: every Hylla call (Query / Worked-via / Suggestion, or "None — Hylla answered everything needed"), every mage invocation by FULL name, every distinct Read/Grep/LSP call, LOC counts from `wc -l` on each verified/written file. Self-LOC-misreporting is a discipline breach.

**Orchestrator audits EVERY agent EVERY time** via jq-filter on the JSONL transcript:

```sh
jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | "\(.name)\t\(.input | tostring | .[0:120])"' <agent-transcript.jsonl>
```

The audit checks: raw `go *` invocations (Hard Rule violation), `mage ci`/`mage test-pkg` from a builder (scope breach), zero `mcp__hylla__*` from a planner/plan-QA (Hylla-mandate breach), Edit/Write paths outside declared `paths` (write-scope breach), git mutations from any subagent (git-floor breach), `till.auth_request operation=create` mid-run (renewal-pile-up anti-pattern; must report BLOCKED instead), Read of sibling builder's WIP files (cross-droplet snooping), `grep`/`sed`/`awk` via Bash instead of native Grep/Read (tool-discipline breach), missing required closing-comment sections.

**Test-concurrency escalation criterion.** Today the methodology relies on prompt-tightening + failure-attribution + Go's process isolation to handle parallel-builder test races. If 3+ cascade groups observe `mage test-*` failures attributable to RUNTIME test-state collision (fixed ports, shared `~/.tillsyn/` paths, env-var mutation — NOT compile-breakage), add a per-package flock to `mage test-*` targets. Until then, prompt-tightening is sufficient (`mage test-func <SpecificFunc>` is already minimum surface).

<!-- TODO populate post-dogfood with measured benchmarks -->

## Three Orthogonal Axes — `kind` × `metadata.role` × `metadata.structural_type`

Every non-project node in the cascade is classified along three independent axes, set explicitly at create time. None of them are inferred from the others. Templates' `child_rules`, gate rules, and agent bindings dispatch on combinations of all three. The orthogonality matters: collapsing any two axes into one produces ambiguity at the dispatch layer and breaks plan-QA's ability to attack misclassification.

The three axes are: **`kind` (what work)** — the closed 12-value enum (`plan`, `build`, `research`, `plan-qa-proof`, `plan-qa-falsification`, `build-qa-proof`, `build-qa-falsification`, `closeout`, `commit`, `refinement`, `discussion`, `human-verify`); **`metadata.role` (who does it)** — the closed role enum (`builder`, `qa-proof`, `qa-falsification`, `qa-a11y`, `qa-visual`, `design`, `commit`, `planner`, `research`); and **`metadata.structural_type` (where it sits)** — the closed 5-value cascade-shape enum (`cascade`, `drop`, `segment`, `confluence`, `droplet`). The dual `kind` + `role` axes earn their keep on QA kinds where parent context disambiguates: `build-qa-proof` and `plan-qa-proof` both carry `role=qa-proof`, but the QA agent's verification axis differs based on parent kind.

For the canonical definitions of each enum value, the worked-combinations table, and atomicity rules (e.g. "`droplet` MUST have zero children" / "`confluence` MUST have non-empty `blocked_by`" / "`cascade` MUST be level-1"), see `WIKI.md` § "Cascade Vocabulary." This methodology doc cross-references rather than duplicates that vocabulary, per the single-canonical-source rule in the wiki.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Closed 12-Value `kind` Enum

`action_items.kind` is a closed 12-value enum, chosen by the creator at create time. There is no inferred default and no fallback kind. The enum partitions cascade work into named work-types: planning-dominant decomposition (`plan`); read-only investigation (`research`); code-changing leaf work (`build`); QA passes attached to both planning and build parents (`plan-qa-proof`, `plan-qa-falsification`, `build-qa-proof`, `build-qa-falsification`); cascade-end coordination (`closeout`, `commit`); long-lived umbrellas and decision parks (`refinement`, `discussion`); and dev sign-off hold points (`human-verify`).

Each kind has specific structural rules. `plan` and `build` auto-create QA-twin children via template `[[child_rules]]` — every `plan` gets `plan-qa-proof` + `plan-qa-falsification`, every `build` gets `build-qa-proof` + `build-qa-falsification`, both pairs `blocked_by` their parent. `research` does NOT auto-create QA twins — research outputs are findings, not implementation claims, so the proof/falsification asymmetry doesn't apply; the orchestrator reviews findings via comment thread. `closeout` / `refinement` / `discussion` / `human-verify` are standalone — they don't auto-create QA twins either; they have their own bespoke gates.

The 12-value enum is closed: extension happens via templates that register custom kinds attaching as sub-action-items of specific generics (e.g. a custom `ledger-update` under `closeout`). Adopters never modify the core 12-value Go enum; they extend via template customization. This is the canonical extension surface and is documented in the templates layer (see `WIKI.md` § "Closed 12-Value `kind` Enum" for the full reference).

<!-- TODO populate post-dogfood with measured benchmarks -->

## `metadata.role` Enum

The `role` axis names *who does the work* — a closed enum: `builder`, `qa-proof`, `qa-falsification`, `qa-a11y`, `qa-visual`, `design`, `commit`, `planner`, `research`. Roles bind to agent definitions via the `[agent_bindings]` section of a template's TOML — the dispatcher reads `(kind, role)` and looks up the corresponding agent file (e.g. `(build, builder) → till-go/builder-agent.md`). The same role can apply to multiple kinds: `qa-proof` applies to both `plan-qa-proof` and `build-qa-proof`, and the QA agent branches on `parent.kind` at runtime to pick the correct verification axis.

The dual-axis `(kind, role)` design is what enables a single agent file to serve two kinds. The `qa-proof-agent.md` and `qa-falsification-agent.md` files in the till-go group are each authored once, and the agent reads `parent.kind` in its system prompt to determine whether to apply the plan-QA verification axis (atomic decomposition + parallelization graph + Specify-block well-formedness) or the build-QA axis (acceptance-criteria conformance + KindPayload-vs-diff drift + adversarial DecisionLog review).

Pre-Drop-2 the role lived in description prose (`Role: builder`, `Role: qa-proof`); post-Drop-2 it lands on `metadata.role` as a first-class field. Either way, the role is set at create time and is mandatory — no inferred defaults, just like `kind` and `structural_type`.

<!-- TODO populate post-dogfood with measured benchmarks -->

## `metadata.structural_type` Enum (cascade / drop / segment / confluence / droplet)

`structural_type` names *where a node sits in the cascade flow's structure*, independent of what kind of work it is or who does it. Picture water flowing down a series of waterfalls: a **cascade** is the whole waterfall sequence (the level-1 tree of work); a **drop** is one vertical step within the cascade; **segments** are parallel streams within a drop; **confluences** are merge points where streams rejoin; **droplets** are atomic, indivisible units that finish in one shot. The metaphor orients the vocabulary; enforcement happens at the create/update boundary.

The 5-value enum is mandatory on every non-project node, validated at the create/update boundary. Atomicity rules: `cascade` MUST be a level-1 node (empty `parent_id` — the project is not modeled as a parent action_item); `droplet` MUST have zero children (any child indicates misclassification — the parent should be `segment` or `drop`); `confluence` MUST have non-empty `blocked_by` (empty is a definitional contradiction since a confluence merges upstream streams); `segment` may recurse and contain droplets, sub-segments, or confluences; `drop` is any level-2+ vertical decomposition step within a cascade.

For the orthogonality table showing how `structural_type` composes with `metadata.role` (canonical combinations like `(droplet, builder)` for build leaves and `(confluence, orchestrator)` for integration points), see `WIKI.md` § "Cascade Vocabulary" — that section is the single canonical source. This methodology doc holds the methodology-level explanation of *why* the axis exists separately from `kind` and `role`; the wiki holds the enforced vocabulary.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Agent Shape

Each cascade kind binds to a specific agent at dispatch time. Agents are defined in template-shipped Markdown files under `internal/templates/builtin/agents/<group>/<name>.md` (where `<group>` is `till-gen`, `till-go`, or `till-gdd` and `<name>` is one of the 7 standard agent names: `planning`, `builder`, `qa-proof`, `qa-falsification`, `research`, `closeout`, `commit-message`). Adopters can override per-project at `<project>/.tillsyn/agents/<name>.md` or per-user at `~/.tillsyn/agents/<group>/<name>.md`; the resolver checks project → user → embedded in priority order.

Agent files carry YAML frontmatter (`name`, `description`) and a substantive body. Runtime configuration — model choice, tool allowlists, environment variables, MCP config — lives in `agents.toml` (per-project) or `agents.local.toml` (per-machine, `.gitignore`d), NOT in agent-file frontmatter. The `tools_allow` list is overridable per-machine; `tools_deny` is a safety floor and rejects user override at startup. Frontmatter `model:` and `tools:` keys, if present in agent files, are stripped at render time when `agents.toml` has the corresponding key set — see `AGENTS_CONFIG.md` for the full configuration reference.

Each agent has a tightly scoped role — planners decompose and never edit code; builders implement leaf work and never spawn other agents; QA agents read and verify but never edit. This separation-of-concerns is hardcoded structural invariant, not template-customizable. Cross-role boundary violations are rejected at the dispatch / MCP layer with structured errors citing the violated invariant.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Role and Model Bindings

Initial bindings during the hardcoded phase. Every binding below is configurable by path + kind in a refinement cascade. These are starting values for dogfood, not permanent law.

| Role                     | Default Model | Edits Code? | Scope                                          |
|--------------------------|---------------|-------------|------------------------------------------------|
| Planner                  | opus / codex  | No          | Writes directives for children, authors plan   |
| Plan-QA (proof)          | opus          | No          | Verifies evidence completeness of plan         |
| Plan-QA (falsification)  | codex gpt-5.x | No          | Attacks plan for missed cases / bad blockers   |
| Builder (droplet)        | sonnet/haiku  | **Yes**     | Implements one droplet                         |
| Build-QA (proof)         | opus          | No          | Verifies completed sub-tree against acceptance |
| Build-QA (falsification) | codex gpt-5.x | No          | Attacks completed sub-tree for integration gaps |
| Research                 | opus          | No          | Read-only investigation; findings via comment  |
| Commit agent             | haiku         | No          | Generates commit messages after gates pass     |

**Rationale**: Multi-backend routing per `project_multi_backend_dogfood_direction.md`. Planning + QA-falsification is where judgment + adversarial thinking concentrate — route to codex (gpt-5.x with reasoning-effort knobs) for falsification, opus for proof. Builders run sonnet or haiku — cheap, fast, retry-friendly. Each per-kind/per-role override lives in `agents.toml`.

<!-- TODO populate post-dogfood with measured benchmarks -->

## QA Placement

Three QA surfaces. None run at the droplet level.

### Package-Level Build+Test (Automated, Not LLM)

Every Go package that received droplet edits runs one `mage ci`-equivalent pass after all droplets targeting that package have reported complete. No LLM. No judgment. Pass/fail is deterministic.

- **Pass**: package is green. The enclosing planner node's build-QA runs next.
- **Fail**: enclosing planner node ingests the failure output, identifies which droplet(s) caused it, writes fix directives to those specific droplets, sets them back to `in_progress`. Siblings already green do not re-build. Repeat until the package is green.

### Planner-Level Build-QA (LLM, Proof + Falsification)

Once all direct children of a planner node are complete AND their package build+test gates are green, the planner node's **build-QA twins** run:

- `build-qa-proof` — verifies the claimed behavior of the completed sub-tree is supported by the actual diff + tests.
- `build-qa-falsification` — attacks the completed sub-tree for integration gaps, contract drift, missing edge-case coverage.

Twins run in parallel. Both must pass before the planner node itself reports complete up to its parent.

### Plan-QA (On Every Planner Node)

When any planner node is created (at any level), the dispatcher auto-creates two plan-QA children: `plan-qa-proof` and `plan-qa-falsification`. Both are `blocked_by` the planner's output (the plan).

- **Before descent — per branch, not per tree.** A planner node cannot spawn its child planners (or child droplets) until both its plan-QA twins pass — per the CLAUDE.md Hard Rule "Plan-QA twins MUST close BEFORE that node's child planners + build droplets start." This gate serializes only THAT branch's own depth: it does NOT block sibling branches, which decompose, plan-QA, build, and build-QA fully in parallel (see "Plan Down, Build Up" → parallelism-is-per-branch). So at any moment the tree has many planners, plan-QA pairs, builders, and build-QA pairs in flight at once across independent branches; each individual branch only goes one level deeper after its own plan-QA pair is green.
- **Parallel within a node.** Proof and falsification run concurrently against the same plan output.

### Second Plan-QA Sweep — Global L1 Re-Check

When the plan-building pass reaches the leaves (final droplets written into the tree) AND the total tree depth under any cascade is **≥ 3**, a **second plan-QA pass** runs with full visibility into the constructed tree rooted at that cascade. It checks:

- Blocker graph is acyclic.
- No two sibling droplets share a `paths` or `packages` entry without an explicit `blocked_by`.
- Acceptance criteria at the leaves actually compose into the cascade's stated outcome.
- No orphan droplets (every droplet leads to the cascade's outcome).

**Threshold is hardcoded to depth ≥ 3 for now. Configurable in a refinement cascade** (starting value, adjusted once we have dogfood data).

### Why No Droplet-Level LLM QA

Droplets are too small to QA meaningfully in isolation. Correctness at the droplet level is either trivially satisfied against the acceptance criteria or obviously wrong — `mage ci` catches the second case. LLM QA at this level pays full cost for near-zero signal. QA moves up to where integration actually happens.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Section 0 — Semi-Formal Reasoning Certificate

Every substantive agent response begins with a `# Section 0 — SEMI-FORMAL REASONING` block before the response body. The block contains either 5 named passes (orchestrator-facing: `Planner` / `Builder` / `QA Proof` / `QA Falsification` / `Convergence`) or 4 named passes (subagent-facing: `Proposal` / `QA Proof` / `QA Falsification` / `Convergence`). Each pass uses a 5-field certificate: **Premises** (what must hold), **Evidence** (grounded sources, not implicit background), **Trace or cases** (concrete paths through the reasoning), **Conclusion** (the claim), and **Unknowns** (what's still uncertain, routed via comment / handoff / attention item or explicitly accepted).

The shape is adapted from Ugare & Chandra's *Agentic Code Reasoning* (arxiv 2603.01896, Meta, 4 Mar 2026) — the paper shows structured certificates reduce patch-equivalence errors substantially — with two methodology-level extensions: **Evidence** and **Unknowns** as first-class fields, and an explicit **adversarial QA Falsification pass** the paper does not include. The falsification extension targets the paper's §4.3 residual failure mode where elaborate but incomplete reasoning chains produce confident but wrong answers; a dedicated adversarial pass is the methodology's hedge against that mode.

Convergence is the gate: an agent declares Convergence only when (a) QA Falsification produced no unmitigated counterexample, (b) QA Proof confirmed evidence completeness across every claim, and (c) remaining Unknowns are explicit and routed. If any of (a)/(b)/(c) fail, the agent loops back to the earliest pass needing rework before declaring Convergence. The reasoning lives in the orchestrator-facing response only — it never gets written into Tillsyn `description`, `metadata.*`, `completion_contract.completion_notes`, comments, or any other durable artifact. Tillsyn stores finalized artifacts, not process.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Tillsyn-Flavored Specify Pass

Each plan and build node carries a structured **Specify** block in its description (or post-Drop-2, in `metadata.specify`). The block has five fields: **Objective** (what this node accomplishes, in one sentence); **AcceptanceCriteria** (a bulleted, testable list of what "done" looks like); **ValidationPlan** (concrete commands or steps to verify acceptance); **RiskNotes** (known hazards and mitigations); and **ContextBlocks** (typed references to upstream decisions / constraints / warnings / reference docs that bound the work).

The shape is inspired by spec-driven development frameworks (Specify, GitHub Spec Kit) but Tillsyn-flavored: the block sits inside the cascade tree as first-class metadata, not in a separate spec file. Plan-QA-proof verifies AcceptanceCriteria support the Objective and that the ValidationPlan exercises every criterion. Plan-QA-falsification attacks the Specify for under-constraining Objectives, over-constraining AcceptanceCriteria (untestable bullets), missing RiskNotes, and ContextBlocks that don't bound the cited risks.

Specify blocks compose with the cascade's recursive structure: a level-2 plan's Specify constrains its level-3 children's Specifies, and child Specifies inherit ContextBlocks from their parents (the dispatcher's context aggregator merges them at spawn time). This makes the Specify pass scale with decomposition depth — high-level intent flows down without being repeated, and low-level details bubble up through QA back to the parent level.

<!-- TODO populate post-dogfood with measured benchmarks -->

## TN-Per-Section Response Style

Substantive agent responses follow a stable numbered-Markdown shape: top-level sections are `## 1. <Title>` / `## 2. <Title>` / etc., with sub-bullets `- 1.1 <text>` / `- 1.2 <text>` / etc. The response closes with a `## TL;DR` containing **one `TN` item per top-level section** (`T1` summarizing section `1`, `T2` summarizing section `2`, etc.) — no extras, no gaps. This pairs with Section 0 to make responses both auditable (every claim has evidence) and addressable (every section has a stable reference like "T2.1 in the plan").

The TN-per-section invariant is enforced at the orchestrator-facing layer; subagents inherit the convention via spawn-prompt directives. Trivial responses (one-line factual lookups, terse confirmations, simple yes/no answers) skip both Section 0 and the numbered body — the rule prevents premature judgment on substantive work, not ceremony for small answers.

The shape is what makes long agent threads navigable. Devs reviewing a plan-QA-proof verdict can address `T3 in the QA verdict` and the agent or orchestrator knows exactly what's being cited. Without stable numbering, address-by-quote degrades into address-by-paraphrase, and audit trails get lossy.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Hylla-First Evidence Ordering

Agents working on Tillsyn use **Hylla** (the project's graph-of-symbols indexer) as the primary source for committed-code understanding. The ordering is: (1) Hylla for committed Go code; (2) `git diff` for files changed since the last Hylla ingest; (3) Context7 + `go doc` + LSP for external library / language / tooling semantics the repo can't answer itself. Non-Go files (markdown, TOML, YAML, magefile, SQL) fall through directly to `Read` / `Grep` / `Glob` since Hylla today indexes Go only.

Hylla-first matters because the indexer's graph traversal (`hylla_graph_nav`, `hylla_refs_find`) surfaces relationships LSP and grep miss — call-graph queries, symbol summaries, semantic search across the committed graph. Agents that skip Hylla and reach for grep tend to find string matches but miss the actual call-graph dependency. When a Hylla query misses (the indexer doesn't return what was needed), agents are required to record the miss in their closing comment under a `## Hylla Feedback` heading; the orchestrator aggregates these at cascade-end to drive Hylla improvements.

For projects that aren't Tillsyn (Hylla is Tillsyn-internal today), the equivalent rule is: use the project's primary semantic-graph indexer first, fall back to LSP and grep for misses, and record misses for tooling improvement. The principle is: prefer graph queries to string matches when a graph index is available.

<!-- TODO populate post-dogfood with measured benchmarks -->

## TDD Requirement

Build droplets follow test-driven development. Tests are authored alongside or before production code, not after. Coverage gates are enforced at the canonical `mage ci` path — the till-go template defaults to ≥70% line coverage on touched packages; below threshold is a hard failure. Coverage thresholds are template-defined (not methodology-hardcoded), so adopters running other templates may differ; the methodology-level rule is *coverage gates exist and are enforced at CI*.

Tests are table-driven where the work admits it, behavior-oriented (not implementation-coupled), and run with `-race` via mage targets. Builders never invoke raw `go test` / `go build` / `go vet` — every build / test / lint goes through `mage <target>` so the canonical CI path is the canonical local verification path. If a mage target is broken, builders fix the target rather than bypass it. Cold-cache equivalence between local `mage ci` and CI `mage ci` is the standard for "ready to push."

The TDD rule composes with the cascade: a `build` droplet's `build-qa-proof` child verifies that every AcceptanceCriteria bullet has a corresponding test, and `build-qa-falsification` attacks the test suite for missing edge cases, untestable assertions, and silent skips. TDD without QA enforcement degrades into "I wrote some tests"; the QA twin makes the rule load-bearing.

<!-- TODO populate post-dogfood with measured benchmarks -->

## QA Proof vs Falsification — Asymmetric Verification

QA in the cascade is **two distinct passes, not duplicate reviewers.** Proof and falsification are asymmetric by design and run in parallel as separate agent contexts so each gets a fresh window without parent-hindsight bias.

**QA Proof** verifies evidence completeness, reasoning coherence, trace coverage, and that the parent's claim is actually supported by the current code and evidence. Plan-QA-proof checks atomic decomposition + parallelization graph + Specify-block well-formedness + multi-level decomposition discipline. Build-QA-proof checks AcceptanceCriteria conformance + KindPayload-vs-diff alignment + CompletionContract checklist + DecisionLog evidence chains. The proof axis is: *"is the claim supported?"*

**QA Falsification** actively tries to break the parent's conclusion via counterexamples, alternate traces, hidden dependencies, contract mismatches, and YAGNI pressure. Plan-QA-falsification attacks over-decomposition, under-decomposition, missing `blocked_by`, over-`blocked_by`, untestable Specify bullets, and cascade-tree misclassification. Build-QA-falsification attacks KindPayload-vs-final-code drift, silently dropped acceptance criteria, parent-plan contract mismatches, and adversarial DecisionLog review. The falsification axis is: *"can the claim be false?"*

Both passes must PASS for the parent to close. A failed proof OR a failed falsification re-routes the work — for `kind=plan` failures, the system wipes the children atomically and respawns the planner with synthesized failure context; for `kind=build` failures, the build re-spawns with QA findings injected into its system prompt. The asymmetric pair is what catches both insufficient evidence (proof's lane) and over-confident reasoning (falsification's lane).

<!-- TODO populate post-dogfood with measured benchmarks -->

## Blocker-Failure Re-QA Invariant

When a node `A` fails and gets edits from its planner, the edits may change the assumptions `A`'s ancestors relied on. After `A` finally completes, the cascade runs mandatory re-QA sweeps.

### Ancestor Re-QA (Primary)

Every ancestor planner node from `A`'s parent up to the cascade root re-runs its **build-QA twins** (both proof + falsification). The twins verify the ancestor's claimed outcome still holds given `A`'s revised behavior.

**Scope**: all the way up to the cascade root. Not pruned at package boundaries — even ancestors that share no `paths`/`packages` with the edited droplet run build-QA re-check, because ancestor planners' **plans** may have been written against `A`'s original output, not just its code.

### Dependent Re-QA (Edge Case)

Nodes `D` with `D.blocked_by` including `A` that have already completed re-run their build-QA twins once `A` finally passes.

**This case should be rare.** Under correct `blocked_by` semantics, `D` should not have reached completion while `A` was in `failed` or post-failure-edit states — `D` would have been blocked from starting. The case opens only when:

- `A` initially completed successfully.
- `D` started and ran against `A`'s output.
- `A`'s ancestor re-QA later found `A` incorrect, reopening `A`.
- `A` got edited, completed again with different behavior.

In that narrow window, `D` already used `A`'s old output; `D`'s build-QA twins must re-verify against `A`'s new output.

### Parallel Sibling Non-Invalidation

Siblings `B` with no `blocked_by` linkage to `A` — parallel by construction — do not re-run QA when `A` is edited. They share neither paths nor packages with `A` (enforced at plan-QA time), so their correctness is independent of `A`'s revised behavior.

### Cost Acceptance

Re-QA cost is real. It is the price of in-place planner edits with audit retention instead of throwing away and rebuilding. The cascade accepts the cost because the alternative — full-subtree rebuild on every failure — is strictly more expensive.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Failure Handling

### `failed` Is A First-Class State

The cascade state model is `todo` / `in_progress` / `complete` / `failed`. Failed items remain in their original position in the tree (not moved), render in red in the TUI, trigger a warning notification, and parent nodes render a "has failed descendant" glyph so operators can jump to the failure without expanding every branch.

### Planner Edits In Place

When a droplet fails and its package build+test goes red:

1. The enclosing planner node (the one whose children contain the failed droplet) moves to `in_progress` if it had advanced.
2. The planner ingests failure output.
3. The planner edits the failed droplet's acceptance, directives, `paths`, or splits it into two droplets.
4. The failed droplet transitions `failed` → `in_progress`; the builder re-runs.
5. Siblings that succeeded stay complete. They do not rerun unless their `paths` or `packages` intersect with the edited droplet — in which case the plan-time `blocked_by` should already have serialized them.

### Wipe-and-Replan

When a `kind=plan` node's plan-QA-falsification finds a structural defect (orphaned droplets, missing blockers, wrong decomposition), the children get atomically wiped and the planner respawns with synthesized failure context. This is the cascade's "throw away and rebuild" mode — reserved for plan-level structural failures, not droplet-level code failures.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Parent-Children-Complete Invariant

A parent node cannot be marked `complete` while any child is incomplete, `failed`, or `blocked`. This is an **always-on invariant** — not a policy bit, not template-configurable, not relaxable. The rule applies recursively: a level-2 plan can't close until every level-3 droplet under it is `complete`; a cascade can't close until every level-2 node is `complete`; and the entire cascade rolls up cleanly only when every leaf has finished and every parent's QA twins have passed.

The invariant is enforced at the domain layer in `internal/domain/action_item.go` and at the `till.action_item(operation=update)` MCP boundary. Attempts to mark a parent `complete` with incomplete children return a closed sentinel error citing the offending children. Pre-Drop-1 the rule was policy-toggled; post-Drop-1 it's hardcoded.

This invariant composes with the `failed` terminal state: when a `build` droplet's QA twin returns a falsification verdict, the build moves to `failed`, the parent plan can't close, and the wipe-and-replan flow fires. Without the parent-children-complete invariant, the cascade would silently close partial trees and lose audit trail.

<!-- TODO populate post-dogfood with measured benchmarks -->

## `blocked_by` Ordering Primitive

`blocked_by` is the **only sibling and cross-cascade ordering primitive** in the cascade. Planners set `blocked_by` at creation time on every child that depends on a sibling's completion before its own work can start. The dispatcher reads `blocked_by` to gate `in_progress` transitions: a child cannot move to `in_progress` while any node in its `blocked_by` list is incomplete or `failed`.

`blocked_by` operates at two levels: planner-set (static, declared at creation) and dispatcher-inserted (dynamic, runtime). The dispatcher's lock manager inserts runtime `blocked_by` on `in_progress` promotion when sibling locks conflict — for example, when two `build` droplets share a Go package and would race on the package's compile/test unit, the dispatcher inserts a `blocked_by` between them so they serialize. Planners are required to set static `blocked_by` whenever sibling droplets share a file (`paths` overlap) or a package (`packages` overlap); plan-QA-falsification attacks missing static `blocked_by` as a primary risk.

The `blocked_by` primitive is what makes parallel builds safe. Without it, two builders touching the same Go package would race and both fail their compile / test runs. With it, the cascade fans out work as wide as the lock graph allows and serializes only where necessary. This is the fan-out enabler — the methodology's response to "how do you parallelize without losing correctness."

<!-- TODO populate post-dogfood with measured benchmarks -->

## Audit Trail

Every edit a planner makes to an in-flight or failed actionItem must be retained. The history is inspectable — operators look back at "what was the first draft of this droplet versus what actually shipped."

### What Must Be Retained

- Every write to `description`, `acceptance`, `paths`, `packages`, `blocked_by`, `directives`, or any planner-editable field.
- Every state transition with timestamp + transitioning principal.
- Every comment (already append-only).

### Storage — Full-Snapshot-Per-Change

Every write stores the full node JSON. Simple, robust, no reconstruction logic. Storage cost scales linearly with edit-count × node-size. Dogfood measures whether this bounds out acceptably. Diff-based or hybrid snapshot-plus-diff storage is deferred until the snapshot-per-change cost becomes unwieldy. Dev directive: "don't optimize too soon."

<!-- TODO populate post-dogfood with measured benchmarks -->

## Isolation Enforcement

Spawned subagents run in **bundle-isolated contexts** — they never see the orchestrator's `~/.claude/CLAUDE.md`, the project's `.claude/CLAUDE.md`, system skills, project plugins, or hooks. The isolation is enforced at the spawn layer: Tillsyn assembles a per-spawn bundle directory containing only the files the agent's role needs (the agent's own `<bundle>/plugin/agents/<name>.md`, the rendered system prompt, MCP config, settings) and invokes `claude --bare --plugin-dir <bundle>/plugin --agent <name> --setting-sources "" --strict-mcp-config --settings ... --mcp-config ...`. The `--bare` flag tells Claude Code to skip every normal context source (Path B / system CLAUDE.md / skills / project CLAUDE.md / hooks / `~/.claude/settings.json` / system plugins).

Isolation matters because the orchestrator's CLAUDE.md and skills are tuned for orchestration — they're loaded with planning rules, dispatch rules, multi-agent coordination semantics. A spawned QA agent reading those would be confused into orchestrator-shaped reasoning. By stripping the entire normal context surface and hand-assembling exactly what the agent needs, Tillsyn guarantees each spawned agent sees only its role-tuned context — no leakage from the orchestrator's configuration.

Sentinel-injection integration tests verify isolation end-to-end: synthetic "BLEED_SENTINEL" strings injected into `~/.claude/CLAUDE.md`, system agents, project CLAUDE.md, and hooks are asserted absent in the spawned process's actual prompt. The tests fail loudly if any normal context source leaks through. The post-render bundle validator checks the spawned bundle's agent body is non-empty and substantive (not a stub) before the spawn proceeds — a defense-in-depth gate against silent isolation regressions.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Cascade Tree — Side-By-Side Worked Example

Two cascades under one project. `CASCADE_0` blocks `CASCADE_1`. `CASCADE_0` is a domain-parallel scaffold (no shared packages, no shared paths). `CASCADE_1` is a sequential feature release (each L3 package consumes the package below via `blocked_by`).

Legend:
- `P` = planner node. Plan-QA twins implicit on every `P`.
- `BQ` = build-QA twins at a planner node.
- `•` = droplet (builder). Package build+test gate implicit per package.
- `═══▶` = cross-cascade or intra-cascade `blocked_by`.

```
═══════════════════════════════════════════════════════════════════════════════
CASCADE 0 — PLATFORM SCAFFOLD         ║  CASCADE 1 — AUTH FEATURE RELEASE
3 depths, package-parallel            ║  4 depths, package-sequential
                                      ║  blocked_by: CASCADE 0 (cross-cascade)
═══════════════════════════════════════════════════════════════════════════════

L1  CASCADE 0  (P + plan-QA)          ║  L1  CASCADE 1  (P + plan-QA)
     │                                 ║       │
     │                                 ║       L2  auth-feature-strategy
     │                                 ║            (P + plan-QA)
     │                                 ║            plans package order
     │                                 ║            │
     ├─ L2  pkg-logger     ──┐         ║            │
     │   (P + plan-QA)       │         ║            ├─ L3  pkg-user-entity
     │   │                   │         ║            │   (P + plan-QA)
     │   └─ L3 droplets:     │ parall  ║            │   │
     │      • pkg-scaffold   │ no pkg  ║            │   └─ L4 droplets:
     │      • config-bind    │ overlap ║            │      • user-struct
     │      • unit-tests     │ no path ║            │      • password-hash
     │   BQ twins            │ overlap ║            │   BQ twins
     │                       │         ║            │
     ├─ L2  pkg-config     ──┤         ║            ├─ L3  pkg-auth-service
     │   (P + plan-QA)       │         ║            │   (P + plan-QA)
     │   │                   │         ║            │   [blocked_by: pkg-user-entity]
     │   └─ L3 droplets:     │         ║            │   │
     │      • TOML-parser    │         ║            │   └─ L4 droplets:
     │      • validator      │         ║            │      • verify-password
     │      • defaults       │         ║            │      • token-issuance
     │   BQ twins            │         ║            │      • ratelimit-hook
     │                       │         ║            │   BQ twins
     └─ L2  pkg-storage    ──┘         ║            │
         (P + plan-QA)                 ║            └─ L3  pkg-http-adapter
         │                             ║                (P + plan-QA)
         └─ L3 droplets:               ║                [blocked_by: pkg-auth-service]
            • schema-ddl               ║                │
            • migrations               ║                └─ L4 droplets:
            • conn-pool                ║                   • POST-/login
         BQ twins                      ║                   • POST-/logout
                                       ║                BQ twins
  (L1 BQ twins roll up L2s)            ║
                                       ║       (L2 BQ twins roll up L3s)
                                       ║  (L1 BQ twins roll up L2)

             │
             │ ═══[blocks]═══▶  CASCADE_1
             │
```

### Parallelism Read

- **CASCADE_0** L2 children (`logger`, `config`, `storage`) have no shared `packages` and no shared `paths`. The dispatcher can spawn their planners in parallel, their droplets in parallel (within package boundaries — one package build gate per L2), and QA twins in parallel.
- **CASCADE_1** L3 chain (`user-entity → auth-service → http-adapter`) is strict `blocked_by`. The dispatcher must serialize. Within each L3, droplets targeting that single package serialize via implicit intra-package `blocked_by` — they share compile.

### Plan-Time Blocker Rules (validated by plan-QA)

- Every sibling pair with overlapping `paths` has an explicit `blocked_by` between them.
- Every sibling pair with overlapping `packages` has an explicit `blocked_by` between them.
- No blocker cycles anywhere in the tree.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Metrics and Instrumentation

The methodology is benchmark-aimed; each cascade emits metrics aggregated at cascade-end.

### Per-Droplet

- **Build-green rate** — percentage of droplets that pass `mage ci`-class package gate on first builder attempt.
- **Builder-retry count** — how many times a droplet was re-dispatched after a failed gate.
- **Planner-edit count** — how many times the planner edited the droplet's description/acceptance/paths between attempts.
- **Actual LOC delta** vs. the soft ~80 LOC target.
- **Actual file count** vs. the soft ≤3 file ceiling.
- **Builder model + time-to-completion + token cost**.

### Per-Planner-Node

- **Plan-QA pass rate** — does the plan survive plan-QA on first shot, or does it need revision?
- **Plan-QA round count** — if plan fails, how many revision cycles until pass?
- **Build-QA pass rate** — does the completed sub-tree survive build-QA?
- **Droplet count per planner** — are planners over-decomposing (too many trivial droplets) or under-decomposing (bloated droplets)?

### Per-Cascade

- **Total cost** — by model tier.
- **Total time-to-completion**.
- **Re-QA frequency** — how often does ancestor re-QA fire? Signals plan-quality at the top of the tree.
- **Parallelism extraction rate** — actual parallel spawns divided by the theoretical maximum the blocker graph permits.
- **Blocker-cycle detection count** — how many cycles did plan-QA catch before they shipped?
- **Path/package conflict count** — missing `blocked_by` between siblings that share paths or packages.

### Comparative

- **Cascade vs. monolithic-agent baseline**: patch-equivalence rate (per arxiv 2603.01896) and cost-per-cascade on matched workloads.
- **Cascade vs. single-agent-with-Section-0-only**: same.
- **Model-tier ablations**: builder sonnet vs. haiku on matched droplet workloads; planner opus vs. codex gpt-5.x.

Instrumentation lives in each droplet's completion comment + the cascade-end aggregation comment. This becomes the source data for benchmark articles.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Comparison Surface

Cascade Methodology sits in the same problem space as Spec-Driven Development (Specify, GitHub Spec Kit), Plan-Decompose-Execute agent frameworks, and the broader Agentic Code Reasoning literature. The methodology's distinguishing commitments are: (1) **closed-12-kind classification** with no inferred defaults, forcing every node to be explicitly typed; (2) **three orthogonal axes** (`kind` / `role` / `structural_type`) that templates and dispatchers compose on independently; (3) **proof-and-falsification QA asymmetry** running in parallel as separate agent contexts; (4) **bottom-up build assembly** with atomic-droplet sizing as the only recursion-cap; (5) **bundle-isolated agent spawns** that strip every normal context source and hand-assemble role-tuned prompts.

These commitments shape the comparison axes. Against pure Spec-Driven approaches, Cascade Methodology adds the recursive cascade tree + dynamic `blocked_by` lock graph + system-managed wipe-and-replan on failure. Against pure agentic frameworks, it adds the structured-Specify pass + Section 0 5-pass certificate + the asymmetric QA pair. Against feature-flagged "AI coding assistants," it adds end-to-end cascade-driven dispatch (no orchestrator hand-work in the steady state) and isolation-enforcement at the spawn boundary.

Concrete benchmark axes — token cost per cascade, end-to-end wall-clock per cascade, error-rate after wipe-and-replan, integration-defect rate at confluences, escalation rate to human review — will be populated post-dogfood per `project_methodology_docs_tracker.md`'s benchmark plan. The methodology is benchmark-aimed; the skeleton ships before the numbers, and the numbers ship after the first dogfood cycles produce them.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Dogfood Evidence (2026-05-15)

First worked-example data from running the methodology against Tillsyn's own development. Five cascades shipped end-to-end with growing methodology discipline; the progression itself is the most load-bearing finding.

### Phase 4.2 — `domain.Project.Language` removal (single-pass planner, no plan-QA gating)

**Shape:** 5-droplet plan → 7 droplets actually shipped. HEAD `94dd934`. `mage ci` GREEN 3290/3290 across 28 packages.

**Methodology gap surfaced:** the planner subagent ran ONCE at phase start, decomposed against the still-present field, and missed 3 transitive consumers because `git grep` against the current tree didn't reveal what would break post-removal. The misses were caught only by build-QA-falsification + `mage build` after intermediate commits:

- `internal/tui` — 5 compile errors caught after D2.
- `cmd/till` — 5 compile errors caught after D6.
- `internal/app/dispatcher` + `cli_claude/render` test fixtures — caught by D4 QA-falsification's `mage ci` rerun.

Each miss spawned a reactive add-on droplet (`D4 tui`, `D7 cmd/till`, would-be `D8 dispatcher` absorbed by D7). The cascade still shipped clean, but with 40% more droplets than planned and an orchestrator-direct stabilization commit chain on top.

**Refinement filed:** Phase 4.2 close — planner missed 3 of 7 surfaces. Proposes a "Cross-package consumer audit" step in planner decomposition: before sizing droplets, enumerate every struct-literal site that references the to-be-removed field + every reader of that field.

### Phase 4.3 — `--language` CLI/MCP/TUI surface teardown (Phase 4.2's refinement applied)

**Shape:** 4-droplet plan → 4 droplets shipped. HEAD `086f255`. `mage ci` GREEN 3280/3280.

**Methodology discipline:** plan-QA pair (proof + falsification) gated BEFORE any builder dispatched — caught 5 NITs upfront and folded them into builder spawn briefs:
- D2 line-729 mid-sentence-clause clarity (proof 1.1).
- D2 doc-comment 548 wording (proof 1.2).
- D2 post-condition `git grep` gate added (proof 1.4).
- D2 `TestRunProjectUpdate_SingleFlagDoesNotClobberOthers` test-rewrite guidance (proof 2.1 + falsification 2.2).
- D3 mcp-rejection sub-test upgraded from "optional" to "REQUIRED" (falsification 2.3).

**Outcome:** ZERO planner-misses, ZERO reactive add-on droplets. The cross-package consumer audit refinement was implicit in the plan-QA pair's verification — proof asked "is every consumer accounted for?" before any code moved.

### E2E-8 — `till_auth_request` `wait_timeout` fix (recursive build-QA follow-up exercised)

**Shape:** 3-droplet plan (D1 + D3 + optional D3) → 4 droplets shipped (D2 absorbed by D1's builder; D4 added recursively from build-QA-falsification finding). HEAD `8e2acfe`. `mage ci` GREEN 3286/3286.

**Recursive cascade exercised cleanly:** D1's build-QA-falsification surfaced sibling-parity gap (sibling claim-side has `WakesOnDeny` + `WakesOnCancel` tests; new create-side tests only covered Approval). Rather than absorbing inline orchestrator-direct, the orchestrator spawned D4 with its own builder + build-QA pair — the cascade ran with proper recursion through the build-QA finding, not just the plan-QA finding.

**Orchestrator-direct QA-falsification fallback:** D3's QA-falsification subagent hit a rate limit mid-run. Per `feedback_orchestrator_no_build.md` mid-flight stabilization allowance, the orchestrator ran the 7-attack pass inline with `git grep` evidence and recorded the verdict directly. The cascade tolerated subagent-availability friction without losing the verification discipline.

### Phase 4.4 — `LoadDefaultTemplateForLanguage` retirement (plan-QA falsification forced a 4-droplet restructure)

**Shape:** Initial 1-droplet "delete the function" plan → 4 droplets after plan-QA falsification rejection (ADD / MIGRATE / MIGRATE / DELETE). HEAD `bb110c1`. `mage ci` GREEN 3294/3294 across 28 packages.

**Plan-QA falsification's load-bearing intervention:** the initial planner's single-droplet "delete `LoadDefaultTemplateForLanguage` + clean up callers in one commit" decomposition would have stranded 2 callers (`internal/app/auto_generate_steward.go:44` and `internal/adapters/mcp_rpc/extended_tools_test.go:3837`) mid-commit, breaking `mage ci` at HEAD. Plan-QA falsification rejected the decomposition with BLOCKER verdict and proposed the 4-droplet ADD/MIGRATE/MIGRATE/DELETE pattern:

- D1 `b156594` (ADD-only) — added `LoadBuiltinTemplate(name string) (Template, error)` + `ErrBuiltinNotFound` sentinel + 4 tests. Old API untouched. `mage ci` GREEN.
- D3 `ce9e36c` (MIGRATE) — migrated `mcp_rpc/extended_tools_test.go` fixture to the new API. Single-site change. `mage ci` GREEN.
- D2 `441c093` (MIGRATE) — migrated `loadStewardSeedTemplate` seam from `func(lang string)` to `func(project domain.Project)`. Production tries `loadProjectTierTemplateOnly(&project)` first, falls back to `LoadBuiltinTemplate("till-gen")`. 6 fixture sites migrated. `mage ci` GREEN.
- D4 `8f84693` (DELETE) — deleted `LoadDefaultTemplate` + `LoadDefaultTemplateForLanguage` + 5 language-axis tests + refreshed doc-comments across 5 files (`-372/+67` LOC delta). `mage ci` GREEN.

**Build-QA NITs all addressed inline:** D2 falsification flagged the docstring's "6 STEWARD anchor seeds" claim as misleading (fallback-only invariant) — fixed at `e55fc22`. D4 falsification flagged orphan `ErrLanguageNotSupported` + pre-existing `ListBuiltinTemplates` doc-comment drift (missing `till-fe`) — both fixed at `bb110c1`. Per `feedback_nits_are_first_class.md`, every NIT addressed in-chain; no NITs deferred to future cascades.

### E2E-5/6/10 — Heterogeneous E2E friction bundle (plan-QA falsification caught a load-bearing planner premise error)

**Shape:** 3 disjoint droplets (one per ticket). HEAD `8ea3822` (final ticket commit). `mage ci` GREEN 3298/3298.

**Plan-QA falsification's load-bearing intervention:** the planner's E2E-6 brief framed the bug as "W2.D7 fields not displayed/edited in TUI form." Plan-QA falsification ran the actual surface audit and surfaced a different reality: `startProjectForm` ALREADY pre-populates all 5 W2.D7 fields at `model.go:4766-4772`, `projectFormBodyLines` ALREADY renders them at `model.go:17417-17421`, and form submission ALREADY wires them at `model.go:11704-11781`. The planner's premise was wrong.

The REAL bug lived at `model.go:4763`: the form was reading `m.projectRoots[slug]` (TUI-local cache) instead of `project.RepoPrimaryWorktree`. For projects backfilled via `till project update --root-path` (the E2E-3 workaround), the canonical field was populated but the form displayed the stale cache value. Plan-QA falsification rescoped D2 from "add missing fields" to "read canonical over cache (1-line fix + TUI test)."

- E2E-5 D1 `edfa130` — extended `writeProjectReadiness` with 6 W2.D7 rows mirroring `writeProjectDetail` order. Build-QA proof PASS 7/7; falsification PASS 8/8 + 1 NIT (table-driven `Groups` edge-case fixture — transitively safe via `strings.Join` + `compactText` contracts).
- E2E-6 D2 `1a932e8` — fixed at `model.go:4763`: prefer `project.RepoPrimaryWorktree`, fall back to `m.projectRoots[slug]` when canonical empty. Both fields `TrimSpace`'d. Build-QA proof PASS 7/7; falsification PASS 8/8 + 1 refinement (cache-vs-canonical UX split between paths/roots modal + project-edit modal — two-cache design noted as non-obvious for maintainers).
- E2E-10 D3 `8ea3822` — Option A auto-clear guard at `service.go:1337` (BEFORE existing `if !=""` at 1340 — placement noted by plan-QA falsification). 3-subtest regression covering top-level / legitimate-parent / not-found branches. Build-QA proof PASS all 7 checks; falsification PASS (7 MITIGATE + 1 ACCEPT-AS-REFINEMENT: MCP wire-layer test for `parent==project` — service-layer coverage sufficient because the MCP adapter is a thin trim-and-forward shim).

**Methodology lesson:** plan-QA falsification is not "check the planner's math" — it is "verify the planner's premise holds against actual code." The E2E-6 catch demonstrates plan-QA's value beyond decomposition-shape verification: it can surface that the planner framed the wrong problem entirely.

### Methodology progression (5 cascades, 2026-05-15)

| Cascade        | Planner spawns | Plan-QA gating | Plan-QA intervention                                                                                       | Reactive droplets | Result                                |
|----------------|----------------|----------------|------------------------------------------------------------------------------------------------------------|-------------------|----------------------------------------|
| Phase 4.2      | 1              | post-hoc retro | none (gating not yet adopted)                                                                              | 3 (40% overhead)  | Caught at build-QA                    |
| Phase 4.3      | 1              | pre-builder    | 5 NITs folded into builder spawn briefs                                                                    | 0                 | Caught at plan-QA                     |
| E2E-8          | 1              | pre-builder    | none (D4 surfaced from build-QA on shipped code)                                                           | 1 (recursive)     | Caught at build-QA, recursed cleanly  |
| Phase 4.4      | 1              | pre-builder    | rejected 1-droplet decomp; forced 4-droplet ADD/MIGRATE/MIGRATE/DELETE                                     | 0                 | Caught at plan-QA, restructure landed |
| E2E-5/6/10     | 1              | pre-builder    | rejected E2E-6 planner premise (fields already wired); rescoped D2 to actual TUI cache-vs-canonical bug    | 0                 | Caught at plan-QA, premise corrected  |

The Phase 4.2 → 4.3 jump established the plan-QA discipline. Phase 4.4 and E2E-5/6/10 confirmed plan-QA earns its keep on every cascade shape — restructure forcing (Phase 4.4), premise correction (E2E-6), and routine NIT folding (Phase 4.3 + E2E-5 + E2E-10). Across 4 plan-QA-gated cascades, plan-QA caught 2 load-bearing decomposition errors (Phase 4.4 ADD/MIGRATE structure + E2E-6 premise) that would have manifested as failed `mage ci` commits or wrong-surface fixes if the gate didn't exist.

### Observations against methodology claims

- **Plan-down-build-up** holds in practice — every cascade decomposed top-down then built bottom-up with `blocked_by` serialization. No droplet had to be split mid-build for sizing.
- **Atomic-droplet sizing (1-2 code blocks)** is the current threshold (tightened from the earlier 1-4 ceiling on 2026-05-22 after observing under-decomposition). Phase 4.3 D2 was at the prior upper bound (5-6 atomic edits across 3 files); under the new threshold that would be a sub-plan, not a single droplet. The methodology's "by cohesion not by region count" framing still applies — sizing is approximate — but recursion is now the default for borderline cases.
- **Three orthogonal axes** (`kind` × `role` × `structural_type`) — orthogonality earned its keep on QA kinds. Every QA pair carried `role=qa-proof` or `role=qa-falsification` regardless of parent `kind` (`plan-qa-proof` on cascade roots + `build-qa-proof` on leaf builds), and the dispatch + reasoning differed based on parent context.
- **Asymmetric QA pair** caught complementary surfaces. Proof PASS verdicts confirmed acceptance criteria; falsification PASS verdicts surfaced 12+ NITs / REFINEMENTS across the 3 cascades that proof never flagged. Running them separately (not as one combined review) was load-bearing — the orchestrator-direct D3 QA-falsification fallback verified the asymmetry: even a single-orchestrator run hits different findings on the falsification axis than on the proof axis.
- **Build-QA-falsification → recursive droplet** is a real pattern. E2E-8's D4 demonstrates: build-QA can legitimately surface a follow-up scope that warrants its own droplet rather than orchestrator-direct absorption. The methodology framing handles this via "any new scope discovered mid-cascade gets its own droplet with full QA pair."

### Systematic benchmark axes (still pending)

Token cost per cascade, end-to-end wall-clock per cascade, subagent failure-and-retry rates, escalation rate to human review — still unmeasured. These need instrumentation in Tillsyn itself before they can be populated. The qualitative discipline progression above is the load-bearing finding for the methodology shape; the quantitative numbers come once Tillsyn measures itself.

<!-- TODO populate post-dogfood with measured benchmarks -->

---

## Provenance

This methodology is the synthesis of multiple threads:

- **Plan-Down-Build-Up spine** — derived from the rollout's `feedback_plan_down_build_up.md` memory entry; refined through Drop 4c.6 / 4c.7 / 4c.8 sketch-and-plan iteration.
- **Section 0 certificate shape** — Ugare & Chandra, *Agentic Code Reasoning* (arxiv 2603.01896, Meta, 4 Mar 2026), with Tillsyn's two extensions (Evidence + Unknowns as first-class fields, plus the adversarial QA Falsification pass).
- **Tillsyn-flavored Specify pass** — inspired by Spec-Driven Development frameworks (Specify, GitHub Spec Kit) and adapted to live in cascade-tree metadata rather than separate spec files.
- **Closed-12-kind enum + orthogonal-axes design** — Tillsyn-internal, landed in Drop 1.75 with the kind-collapse migration; documented canonically in `WIKI.md` § "Cascade Vocabulary."
- **5-value `structural_type` with cascade-as-level-1** — Dev directive 2026-05-21; Go enum work tracked at Tillsyn action_item `62569299-6522-401e-a15b-c6f61e2dc609`.
- **`--bare`-collapsed isolation enforcement** — Anthropic's documented Claude Code `--bare` flag behavior; shipped end-to-end in Drop 4c.6 W3.
- **Atomic-droplet sizing rules** — till-go + till-fe template values (1-2 code blocks, ≤80 LOC + tests as of 2026-05-22; previously 1-4 blocks / 80-120 LOC through Drop 4c iterations); adopters using other templates may differ. The methodology-level invariant is *atomic sizing exists* and *non-atomic sub-goals become sub-plans*, not the specific till-go numbers.
- **Side-by-side worked example + metrics catalog + blocker-failure re-QA invariant + failure-handling rules + audit-trail storage** — originally drafted in `AGENT_CASCADE_DESIGN.md` (2026-04-18 design doc, since retired); merged into this methodology doc 2026-05-21.

The methodology is intentionally template-customizable at the semantic edges (sizing numbers, model assignments, tool allowlists) and hardcoded-structural at the invariant edges (closed enums, parent-children-complete, isolation enforcement, separation-of-concerns between roles). The split follows Tillsyn's "templates define semantic behavior; Tillsyn enforces structural invariants" rule (`feedback_tillsyn_enforces_templates.md`).

The skeleton in this document is intentionally evergreen at the methodology-shape level — the rules that change with measurement (sizing thresholds, escalation N-counts, token caps) are deferred to template config and post-dogfood numbers, while the rules that anchor the methodology (closed enums, asymmetric QA, plan-down-build-up, isolation-by-default) ship as load-bearing skeleton text. Future revisions will populate the post-dogfood benchmark sections and refine the template-defined edges in lockstep with measured outcomes.

<!-- TODO populate post-dogfood with measured benchmarks -->

## Related Files

- `WIKI.md` § "Cascade Vocabulary" — single canonical source for the `kind` enum, `role` enum, `structural_type` enum, and the orthogonality table.
- `CLAUDE.md` — project rules, Hard Rules, agent bindings, build-QA-commit discipline.
- `AGENTS_CONFIG.md` — adopter-facing reference for `agents.toml` schema, override semantics, env_set vs env_from_shell, tools_allow vs tools_deny scope, frontmatter strip behavior, `claude_md_addons`, and worked Bedrock / Vertex / OpenRouter / Ollama Cloud examples.
- `GDD_METHODOLOGY.md` — Graph-Driven Development methodology (Hylla-flavored). Composes with Cascade Methodology: cascade describes *how work decomposes and verifies*; GDD describes *how knowledge is graph-indexed and traversed*. Substantive content lands post-Hylla-rev / post-dogfood per `project_methodology_docs_tracker.md`.
- `CLI_ADAPTER_AUTHORING.md` — guide for authoring new CLI adapters (today: Claude Code + Codex; future: Ollama-bridge, others). Inherits the `--bare`-collapsed isolation framing.
