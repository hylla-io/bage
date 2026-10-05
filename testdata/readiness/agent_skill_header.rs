//! The Hs1 agent-protocol skill body (DL-41: "agent-protocol skill seeded into every spawned
//! RunDir"), rendered from a TOOL SLICE so the tool table is generated rather than restated.
//!
//! Two production callers render the same body from different views of the same inventory:
//! `hylla-mcp` over the static registry filtered by a [`ScopeBinding`](), and `hylla-runtime`
//! over a spawn's PROBED `tools/list`. They cannot share a crate — DL-286 bars `hylla-runtime`
//! an `hylla-mcp` edge — so the body lives below both, depending on neither.
//!
//! SPAWN-FILTERED, fail-closed: a prose SECTION is emitted only when every tool it names is in
//! `tools`, so a rendered skill can never hand an agent a wire name its spawn cannot call. The
//! tool table between [`GEN_OPEN`] and [`GEN_CLOSE`] is machine-rendered and must not be
//! hand-edited.
//!
//! STARVED BY CONSTRUCTION: the body carries the protocol an agent needs to USE its tools and
//! nothing else — no corpus prose, no roadmap, no tool the spawn was denied.

#![forbid(unsafe_code)]

use std::fmt::Write as _;

/// Open marker of the protected GENERATED tool block. Bytes between this and [`GEN_CLOSE`] are
/// machine-rendered from the caller's tool slice.
pub const GEN_OPEN: &str = "<!-- HYLLA:GENERATED:tools -->";
/// Close marker of the protected GENERATED tool block (see [`GEN_OPEN`]).
pub const GEN_CLOSE: &str = "<!-- HYLLA:/GENERATED:tools -->";

/// One tool as the skill names it: the wire name an agent calls, and the one-line summary the
/// generated block prints. Borrowed so neither caller has to allocate a parallel inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillTool<'a> {
    /// The wire name an agent calls.
    pub wire_name: &'a str,
    /// One-line agent-facing summary.
    pub summary: &'a str,
}

/// Which harness a rendered skill is destined for.
///
/// Drives two things: codex's lazy MCP loading (DL-83 gate 2 — codex agents MUST be told to
/// `tool_search` because tools are not auto-injected), and the RunDir destination the harness
/// reaches the body through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillHarness {
    /// Claude Code (`claude -p`) — tools auto-injected; the protocol is DELIVERED by flag.
    Claude,
    /// OpenAI Codex CLI (`codex exec`) — lazy MCP loading; reads `AGENTS.md` from cwd.
    Codex,
    /// omp / oh-my-pi — tools auto-injected; reads `AGENTS.md` from cwd.
    Omp,
}

impl SkillHarness {
    /// RunDir-RELATIVE destination the rendered skill is seeded to.
    ///
    /// Under `workspace/` because that is the spawn's cwd: `spawn_one` forces `profile.cwd =
    /// None` and every adapter then resolves cwd to the seeded workspace. A path outside the
    /// agent's cwd would seed a file nothing reaches — the same unreached-capability defect one
    /// layer down.
    ///
    /// Codex and omp auto-read `AGENTS.md` from cwd. Claude's `CLAUDE.md` auto-discovery is
    /// switchable OFF by the same flag that keeps the PARENT's config out, so the claude adapter
    /// delivers this path explicitly instead — and the name stays off `CLAUDE.md` so that a
    /// `CLAUDE.md` in the spawn cwd remains purely a parent-config surface, provably unread.
    pub fn rundir_dest(self) -> &'static str {
        match self {
            Self::Claude => "workspace/.hylla/agent-protocol.md",
            Self::Codex | Self::Omp => "workspace/AGENTS.md",
        }
    }
}

/// Render the agent-protocol skill for `tools` + `harness`.
///
/// Every section naming `hylla_*` tools is gated on ALL of them being present in `tools`; the
/// generated block lists exactly `tools`, in the caller's order.
pub fn render_agent_protocol_skill(tools: &[SkillTool<'_>], harness: SkillHarness) -> String {
    let has = |name: &str| tools.iter().any(|t| t.wire_name == name);

    // Header + provider-not-decider: names no tool → always present.
    let mut out = String::from(
        "# Hylla agent protocol (generated)\n\n\
         You are working through Hylla, a grounded knowledge-graph workbench. Follow this\n\
         protocol; it is the contract, not advice.\n\n\
         ## Provider, not decider\n\
         Hylla returns grounded graph context, findings, `suggested_next`, and UNKNOWNS —\n\
         never verdicts. YOU decide; cite the graph nodes you relied on.\n\n",
    );
    out
}
