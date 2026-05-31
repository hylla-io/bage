#!/usr/bin/env python3
"""post_tooluse_agent_audit.py

PostToolUse hook for the built-in `Agent` tool. After every Agent dispatch,
this hook reads the subagent's JSONL transcript, categorizes every tool_use
into TOON-formatted buckets, checks out-of-scope against the persona's
`tools:` frontmatter allowlist, and emits the audit via
`hookSpecificOutput.additionalContext` so the parent orchestrator sees it
inline next to the Agent tool's result.

PROJECT-LEVEL HOOK. Byte-identical cp across all 5 sibling projects
(tillsyn / ta / valv / sand / hylla-poly). Lives at:
  <project>/.claude/hooks/post_tooluse_agent_audit.py

Wire-up in `<project>/.claude/settings.json` PostToolUse:
  {
    "matcher": "Agent",
    "hooks": [{
      "type": "command",
      "command": "python3 -B \"$CLAUDE_PROJECT_DIR/.claude/hooks/post_tooluse_agent_audit.py\""
    }]
  }

TOON format spec (real): https://github.com/toon-format/toon
  - Single object: yaml-like indented `key: value`.
  - Tabular array: `field[N]{col1,col2,...}:\n  v1,v2,...\n  v1,v2,...`
  - Empty array: `field[0]:`

Fail-safe: any exception is logged to
  <project>/.claude/hooks/agent_audit_errors.log
and the hook exits 0. NEVER blocks claude code's normal flow.

Sand + tillsyn Go-port reference: this is the bash/Python proof-of-concept.
Sand will translate to Go MCP returning `tool_calls` in response envelope;
tillsyn will translate to Go persisting tool_calls in
`action_item.metadata.tool_calls`. TOON schema stays the same across all
three implementations.
"""

from __future__ import annotations

import json
import os
import re
import sys
import traceback
from pathlib import Path


# --- Tool categorization buckets ---

FILE_OPS = frozenset({
    "Read", "Edit", "Write", "MultiEdit", "Grep", "Glob", "NotebookEdit",
})
BASH = frozenset({"Bash"})
WEB = frozenset({"WebFetch", "WebSearch"})
LSP = frozenset({"LSP"})
AGENT_RECURSIVE = frozenset({"Agent"})
SKILL = frozenset({"Skill"})
SCHEDULE = frozenset({"ScheduleWakeup", "CronCreate", "CronDelete", "CronList"})
WORKTREE = frozenset({"EnterWorktree", "ExitWorktree"})
# TaskCreate/Update/List etc. are USEFUL (sub-granular subagent work);
# they fall into other_calls — NOT a forbidden bucket.

MCP_TA_PREFIX = "mcp__ta__"
MCP_TILLSYN_PREFIX = "mcp__tillsyn"  # covers __tillsyn__ AND __tillsyn-dev__
MCP_HYLLA_PREFIX = "mcp__hylla__"
PLUGIN_CONTEXT7_PREFIX = "mcp__plugin_context7_"
PLUGIN_PLAYWRIGHT_PREFIX = "mcp__plugin_playwright_"
PLUGIN_GOPLS_PREFIX = "mcp__plugin_gopls"

BUCKETS = [
    "file_ops",
    "bash_calls",
    "mcp_ta_calls",
    "mcp_tillsyn_calls",
    "mcp_hylla_calls",
    "plugin_context7_calls",
    "plugin_playwright_calls",
    "plugin_gopls_calls",
    "web_calls",
    "lsp_calls",
    "agent_calls",
    "skill_calls",
    "schedule_calls",
    "worktree_calls",
    "other_calls",
]


def categorize(tool_name: str) -> str:
    if tool_name in FILE_OPS:
        return "file_ops"
    if tool_name in BASH:
        return "bash_calls"
    if tool_name in WEB:
        return "web_calls"
    if tool_name in LSP:
        return "lsp_calls"
    if tool_name in AGENT_RECURSIVE:
        return "agent_calls"
    if tool_name in SKILL:
        return "skill_calls"
    if tool_name in SCHEDULE:
        return "schedule_calls"
    if tool_name in WORKTREE:
        return "worktree_calls"
    if tool_name.startswith(MCP_TA_PREFIX):
        return "mcp_ta_calls"
    if tool_name.startswith(MCP_TILLSYN_PREFIX):
        return "mcp_tillsyn_calls"
    if tool_name.startswith(MCP_HYLLA_PREFIX):
        return "mcp_hylla_calls"
    if tool_name.startswith(PLUGIN_CONTEXT7_PREFIX):
        return "plugin_context7_calls"
    if tool_name.startswith(PLUGIN_PLAYWRIGHT_PREFIX):
        return "plugin_playwright_calls"
    if tool_name.startswith(PLUGIN_GOPLS_PREFIX):
        return "plugin_gopls_calls"
    return "other_calls"


# --- Persona tools allowlist parsing ---

def load_persona_allowlist(cwd: str, agent_type: str):
    """Read `<cwd>/.claude/agents/<agent_type>.md`, return the comma-split
    `tools:` frontmatter list as a set of tool names. None if file missing
    or frontmatter has no `tools:` line.
    """
    persona_path = Path(cwd) / ".claude" / "agents" / f"{agent_type}.md"
    if not persona_path.is_file():
        return None
    try:
        text = persona_path.read_text(encoding="utf-8")
    except Exception:
        return None
    m = re.match(r"---\s*\n(.*?)\n---\s*\n", text, re.DOTALL)
    if not m:
        return None
    frontmatter = m.group(1)
    tools_match = re.search(r"^tools:\s*(.+)$", frontmatter, re.MULTILINE)
    if not tools_match:
        return None
    raw = tools_match.group(1).strip()
    return {t.strip() for t in raw.split(",") if t.strip()}


# --- Subagent JSONL discovery ---

def find_subagent_jsonl(cwd: str, parent_session_id: str, agent_id: str):
    """Locate the subagent JSONL at:
      ~/.claude/projects/<project-flat>/<parent_session_id>/subagents/agent-<agent_id>.jsonl
    """
    project_flat = cwd.replace("/", "-")
    candidate = (
        Path.home() / ".claude" / "projects" / project_flat /
        parent_session_id / "subagents" / f"agent-{agent_id}.jsonl"
    )
    return candidate if candidate.is_file() else None


def find_subagent_meta(jsonl_path: Path):
    meta_path = jsonl_path.parent / (jsonl_path.stem + ".meta.json")
    if not meta_path.is_file():
        return None
    try:
        return json.loads(meta_path.read_text(encoding="utf-8"))
    except Exception:
        return None


def resolve_agent_id(cwd: str, parent_session_id: str, tool_use_id: str,
                    tool_response):
    """Try to get agent_id from tool_response first; fall back to scanning
    the subagents/ dir for the meta.json whose toolUseId matches.
    """
    if isinstance(tool_response, dict):
        for key in ("agentId", "agent_id", "id"):
            v = tool_response.get(key)
            if isinstance(v, str) and v:
                return v
    # Scan-by-tool-use-id fallback.
    if not (tool_use_id and parent_session_id):
        return ""
    project_flat = cwd.replace("/", "-")
    sub_dir = (
        Path.home() / ".claude" / "projects" / project_flat /
        parent_session_id / "subagents"
    )
    if not sub_dir.is_dir():
        return ""
    for meta_file in sub_dir.glob("agent-*.meta.json"):
        try:
            meta = json.loads(meta_file.read_text(encoding="utf-8"))
        except Exception:
            continue
        if meta.get("toolUseId") == tool_use_id:
            # meta_file name: agent-<id>.meta.json
            name = meta_file.name
            if name.startswith("agent-") and name.endswith(".meta.json"):
                return name[len("agent-"):-len(".meta.json")]
    return ""


# --- Tool-use extraction ---

def brief_input(inp) -> str:
    """Compact one-line summary of a tool_use input dict (first 3 keys)."""
    if not isinstance(inp, dict):
        return ""
    parts = []
    for k, v in list(inp.items())[:3]:
        if isinstance(v, (str, int, float, bool)):
            sv = str(v)
            if len(sv) > 60:
                sv = sv[:57] + "..."
            parts.append(f"{k}={sv}")
        else:
            parts.append(f"{k}=<{type(v).__name__}>")
    return " ".join(parts)


def extract_tool_uses(jsonl_path: Path):
    """Walk the subagent JSONL line-by-line, collect every tool_use entry."""
    calls = []
    try:
        with jsonl_path.open("r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if rec.get("type") != "assistant":
                    continue
                content = rec.get("message", {}).get("content", [])
                if not isinstance(content, list):
                    continue
                for c in content:
                    if not isinstance(c, dict):
                        continue
                    if c.get("type") != "tool_use":
                        continue
                    name = c.get("name", "?")
                    inp = c.get("input", {}) or {}
                    calls.append({
                        "name": name,
                        "input_brief": brief_input(inp),
                    })
    except Exception:
        pass
    return calls


# --- TOON rendering ---

def render_toon(audit: dict, calls: list, allowlist) -> str:
    """Render the audit as real TOON-format text (per /toon-format/toon)."""
    lines = []

    # Single-object header.
    lines.append("agent_audit:")
    lines.append(f"  agent_id: {audit.get('agent_id', '?')}")
    lines.append(f"  agent_type: {audit.get('agent_type', '?')}")
    desc = audit.get("description", "").replace("\n", " ").replace(",", ";")[:120]
    lines.append(f"  description: {desc}")
    lines.append(f"  tool_uses_total: {len(calls)}")
    lines.append("")

    # Categorize.
    by_bucket = {b: [] for b in BUCKETS}
    out_of_scope = []
    for idx, c in enumerate(calls, start=1):
        name = c["name"]
        bucket = categorize(name)
        by_bucket[bucket].append({"idx": idx, "name": name,
                                  "input_brief": c["input_brief"]})
        if allowlist is not None and name not in allowlist:
            out_of_scope.append({"idx": idx, "name": name,
                                 "reason": "not_in_persona_tools"})

    # Per-bucket TOON output.
    for bucket in BUCKETS:
        items = by_bucket[bucket]
        n = len(items)
        if n == 0:
            lines.append(f"{bucket}[0]:")
        else:
            lines.append(f"{bucket}[{n}]{{idx,name,input_brief}}:")
            for it in items:
                ib = it["input_brief"].replace(",", ";").replace("\n", " ")
                if len(ib) > 120:
                    ib = ib[:117] + "..."
                lines.append(f"  {it['idx']},{it['name']},{ib}")
        lines.append("")

    # forbidden_calls — empty placeholder for v2 per-persona policy.
    lines.append("forbidden_calls[0]:")
    lines.append("  (per-persona/per-project policy populates this; empty in MVP)")
    lines.append("")

    # out_of_scope vs persona tools allowlist.
    if not out_of_scope:
        lines.append("out_of_scope[0]:")
        if allowlist is None:
            lines.append("  (persona file not found; out-of-scope not checked)")
    else:
        lines.append(f"out_of_scope[{len(out_of_scope)}]{{idx,name,reason}}:")
        for it in out_of_scope:
            lines.append(f"  {it['idx']},{it['name']},{it['reason']}")
    lines.append("")

    # Totals.
    lines.append("totals:")
    for bucket in BUCKETS:
        short = bucket[:-len("_calls")] if bucket.endswith("_calls") else bucket
        lines.append(f"  {short}: {len(by_bucket[bucket])}")
    lines.append("  forbidden: 0")
    lines.append(f"  out_of_scope: {len(out_of_scope)}")

    return "\n".join(lines)


# --- Error logging ---

def log_error(cwd: str, msg: str) -> None:
    try:
        log_path = Path(cwd) / ".claude" / "hooks" / "agent_audit_errors.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("a", encoding="utf-8") as f:
            f.write(msg.rstrip() + "\n")
    except Exception:
        pass


# --- Entry point ---

def main() -> int:
    try:
        raw = sys.stdin.read()
        if not raw.strip():
            return 0
        try:
            hook_input = json.loads(raw)
        except json.JSONDecodeError as e:
            log_error(os.getcwd(), f"hook_input_parse_error: {e}")
            return 0

        if hook_input.get("tool_name") != "Agent":
            return 0  # only audit Agent dispatches

        cwd = hook_input.get("cwd") or os.getcwd()
        parent_session_id = hook_input.get("session_id", "")
        tool_use_id = hook_input.get("tool_use_id", "")
        tool_response = hook_input.get("tool_response", {}) or {}

        agent_id = resolve_agent_id(cwd, parent_session_id, tool_use_id,
                                    tool_response)
        if not agent_id:
            log_error(cwd, f"no_agent_id_found tool_use_id={tool_use_id}")
            return 0

        jsonl_path = find_subagent_jsonl(cwd, parent_session_id, agent_id)
        if jsonl_path is None:
            log_error(cwd, f"jsonl_not_found agent_id={agent_id} "
                          f"parent_session={parent_session_id}")
            return 0

        meta = find_subagent_meta(jsonl_path) or {}
        agent_type = meta.get("agentType", "unknown")
        description = meta.get("description", "")

        allowlist = load_persona_allowlist(cwd, agent_type)
        calls = extract_tool_uses(jsonl_path)

        audit = {
            "agent_id": agent_id,
            "agent_type": agent_type,
            "description": description,
        }
        toon = render_toon(audit, calls, allowlist)

        output = {
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": toon,
            }
        }
        print(json.dumps(output))
        return 0
    except Exception:
        try:
            log_error(os.getcwd(), traceback.format_exc())
        except Exception:
            pass
        return 0


if __name__ == "__main__":
    sys.exit(main())
