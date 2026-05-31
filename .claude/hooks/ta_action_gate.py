#!/usr/bin/env python3
"""ta_action_gate — PreToolUse gate for built-in Agent subagents.

Architecture (locked 2026-05-27 — no --bare):

  * For TOP-LEVEL sessions (orchestrator, `claude -p` headless subprocess): no
    `agent_id` in hook input. DEFER to claude code's normal permission flow.
    For `-p`, the dispatcher passes `--settings <persona-settings.json>` so
    claude code applies the per-persona permissions natively.

  * For BUILT-IN AGENT subagents: `agent_id` present. `--settings` does NOT
    apply per-dispatch on this path (Agent tool spawns subagents in-process).
    This hook reads `<project>/.claude/agents/<agent_type>/settings.json` and
    applies `permissions.deny` patterns against the current Bash command,
    plus the orch-independent baselines below.

Hardcoded baselines (always apply to subagents):
  * git mutation verbs (commit/push/add/etc.) — orchestrator is sole committer.
  * raw go verbs (test/build/vet/run/install/fmt/mod/tool/generate/get/work) —
    must use mage.
  * gofmt / gofumpt — must use mage format.

Non-Bash tool surface (Read / Edit / Write / MultiEdit / Grep / Glob / mcp__*) is
restricted by the persona's `tools:` frontmatter — claude code enforces it
natively for built-in Agent before this hook even runs. The hook focuses on
Bash gate, where commands are dynamic and need pattern-based matching.

Per-dispatch edit-path scope is DEFERRED — see
EDIT_PATH_SCOPE_GATING_DEFERRED.md.

PROJECT-LEVEL HOOK. Byte-identical cp across sibling projects (tillsyn / ta /
valv / sand / hylla-poly) once the architecture is proven in tillsyn.

Fails OPEN on any internal error (a hook bug must never brick a tool call).
"""

import json
import os
import re
import sys
from typing import NoReturn


# --- Hardcoded baselines (orch-independent — every subagent is blocked) ---

_GIT_GLOBAL_OPT_WITH_ARG = {
    "-C", "--git-dir", "--work-tree", "--namespace", "-c", "--exec-path",
    "--super-prefix",
}


def _git_subcommand(seg_tokens):
    """If a shell segment invokes git (possibly path-prefixed, behind env
    assignments, and behind git global options like -C / -c / --git-dir),
    return the git subcommand verb; else None. Defeats `git -C dir commit`,
    `/usr/bin/git commit`, `FOO=1 git commit`, `git --git-dir=x commit`."""
    n = len(seg_tokens)
    i = 0
    while i < n and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", seg_tokens[i]):
        i += 1  # skip leading VAR=val env assignments
    while i < n:
        if seg_tokens[i].rsplit("/", 1)[-1] == "git":
            j = i + 1
            while j < n:
                tk = seg_tokens[j]
                if tk in _GIT_GLOBAL_OPT_WITH_ARG:
                    j += 2  # global option consumes its argument
                    continue
                if tk.startswith("-"):
                    j += 1  # other global flag (incl. --git-dir=… inline)
                    continue
                return tk  # first non-flag token is the subcommand
            return None
        i += 1
    return None


_GIT_MUTATION_VERBS = frozenset({
    "commit", "push", "add", "reset", "rebase", "merge", "checkout", "branch",
    "tag", "stash", "restore", "cherry-pick", "am", "clean", "switch", "rm",
    "mv", "update-ref", "gc", "prune", "worktree", "submodule", "init", "clone",
    "fetch", "pull", "remote", "apply",
})


def _git_mutation(command):
    """Return the git-mutation verb (e.g. 'git commit') if any shell segment
    invokes one, else None. Read-only git (diff/status/log/show/blame/...) is
    NOT in _GIT_MUTATION_VERBS and stays allowed."""
    cmd = command or ""
    for seg in re.split(r"[;&|\n]+", cmd):
        sub = _git_subcommand(seg.split())
        if sub is not None and sub in _GIT_MUTATION_VERBS:
            return "git " + sub
    return None


_RAW_GO_FORBIDDEN_VERBS = frozenset({
    "test", "build", "vet", "run", "install", "fmt", "mod", "tool",
    "generate", "get", "work",
})


def _raw_go(command):
    """Return the forbidden `go <verb>` string if any shell segment invokes
    one. Path-prefixed (`/usr/local/go/bin/go test`) and env-prefixed
    (`GOFLAGS=... go test`) variants are caught. Read-only verbs (`go doc`,
    `go list`, `go env`, `go version`, `go help`) are NOT in this set."""
    cmd = command or ""
    for seg in re.split(r"[;&|\n]+", cmd):
        tokens = seg.split()
        n = len(tokens)
        i = 0
        while i < n and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", tokens[i]):
            i += 1
        if i < n and tokens[i].rsplit("/", 1)[-1] == "go" and i + 1 < n:
            verb = tokens[i + 1]
            if verb in _RAW_GO_FORBIDDEN_VERBS:
                return "go " + verb
    return None


_RAW_FMT_BINS = frozenset({"gofmt", "gofumpt"})


def _raw_fmt(command):
    """Return the forbidden fmt-binary name (gofmt/gofumpt) if any shell
    segment invokes one. All formatting MUST go through mage format /
    mage format-file."""
    cmd = command or ""
    for seg in re.split(r"[;&|\n]+", cmd):
        tokens = seg.split()
        n = len(tokens)
        i = 0
        while i < n and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", tokens[i]):
            i += 1
        if i < n:
            bin_name = tokens[i].rsplit("/", 1)[-1]
            if bin_name in _RAW_FMT_BINS:
                return bin_name
    return None


# --- Per-persona settings.json ---

_BASH_PATTERN_RE = re.compile(r"^Bash\((.+?)(:\*)?\)$")


def _bash_pattern_from(p):
    """Extract bash-command pattern from 'Bash(<pattern>:*)' or 'Bash(<pattern>)'.
    Returns the inner pattern (e.g. 'git commit' from 'Bash(git commit:*)') or
    None if not a Bash() pattern."""
    if not isinstance(p, str):
        return None
    m = _BASH_PATTERN_RE.match(p.strip())
    return m.group(1) if m else None


def _bash_pattern_matches(cmd, bash_pat):
    """Return True if a bash-command pattern matches the command. Handles
    git-verb-aware matching (catches `git -C dir commit`) AND generic word-
    boundary matching."""
    # Git verb-aware: extract 'commit' from 'git commit' and check via
    # _git_subcommand (defeats `git -C dir commit`).
    git_verb_m = re.match(r"^git\s+(\S+)$", bash_pat.strip())
    if git_verb_m:
        verb = git_verb_m.group(1)
        for seg in re.split(r"[;&|\n]+", cmd):
            sub = _git_subcommand(seg.split())
            if sub is not None and sub == verb:
                return True
        return False
    # Generic word-boundary match for 'mage ci', 'go get', 'rm -rf', etc.
    return bool(re.search(r"(?<![\w-])" + re.escape(bash_pat) + r"(?![\w-])", cmd))


def _read_persona_settings(agent_type, project_dir):
    """Read .claude/agents/<agent_type>/settings.json from the project. Return
    parsed dict or None if missing / unreadable."""
    if not agent_type or not project_dir:
        return None
    path = os.path.join(project_dir, ".claude", "agents", agent_type, "settings.json")
    if not os.path.exists(path):
        return None
    try:
        with open(path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else None
    except Exception:
        return None


def _persona_deny_for_bash(cmd, deny_patterns):
    """Return the first Bash-style deny pattern (e.g. 'Bash(git commit:*)')
    that matches the command, else None."""
    if not isinstance(deny_patterns, list):
        return None
    for p in deny_patterns:
        bash_pat = _bash_pattern_from(p)
        if bash_pat is None:
            continue
        if _bash_pattern_matches(cmd, bash_pat):
            return p
    return None


# --- Result emitters ---

def _defer_and_exit() -> NoReturn:
    """exit 0 with no output == defer to claude code's normal permission flow."""
    sys.exit(0)


def _deny(reason) -> NoReturn:
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }))
    sys.exit(0)


def _log(rec):
    try:
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ta_gate_debug.log"), "a", encoding="utf-8") as fh:
            fh.write(json.dumps(rec) + "\n")
    except Exception:
        pass


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        _defer_and_exit()

    tool = data.get("tool_name", "")
    tinput = data.get("tool_input", {}) or {}
    agent_id = data.get("agent_id", "")
    agent_type = data.get("agent_type", "")

    # Identify the persona for this tool call:
    #   1. Built-in Agent subagent: agent_id + agent_type are in hook input
    #      (claude code passes them automatically for subagent tool calls).
    #   2. `claude -p` subprocess dispatched via bin/agent-dispatch.sh: the
    #      dispatcher exports TILL_PERSONA=<role> env var for the subprocess.
    #      The hook inherits the env. Empirically verified 2026-05-27:
    #      `--settings <file>` permissions.deny is NOT enforced by claude
    #      code in headless `-p` mode without `--bare`. The hook is the only
    #      universal enforcement layer for per-persona deny on `-p`.
    #   3. Orchestrator / main session: neither present -> defer.
    persona = agent_type or os.environ.get("TILL_PERSONA", "")
    if not persona:
        _defer_and_exit()

    # Non-Bash tool surface: defer. For built-in Agent, the persona's `tools:`
    # frontmatter restricts the surface natively. For `-p` subprocess, claude
    # code's tool-availability logic restricts based on the persona body /
    # --append-system-prompt. The hook focuses on Bash where commands are
    # dynamic and need pattern-based deny.
    if tool != "Bash":
        _defer_and_exit()

    cmd = tinput.get("command", "") or ""

    # Hardcoded baselines (always apply to scoped tool calls — orch-independent).
    baseline_hit = _git_mutation(cmd) or _raw_go(cmd) or _raw_fmt(cmd)
    if baseline_hit:
        _log({
            "persona": persona, "agent_id": agent_id, "tool": tool,
            "decision": "deny", "reason": "baseline", "hit": baseline_hit,
            "command": cmd[:200],
        })
        _deny(
            f"ta-action-gate baseline: '{baseline_hit}' is orchestrator-only "
            f"for dispatched agents. Use mage targets (test-func / format / "
            f"vet-pkg / build) instead of raw `go`. Git mutation routes "
            f"through the orchestrator. STOP and report to the orchestrator "
            f"if the spawn prompt directed this."
        )

    # Per-persona deny patterns from settings.json. SAME source of truth for
    # built-in Agent AND `-p` subprocess paths.
    project_dir = os.environ.get("CLAUDE_PROJECT_DIR", "")
    if not project_dir:
        # Fall back to walking up from hook's own location:
        # .claude/hooks/ta_action_gate.py -> .claude -> <project_dir>
        project_dir = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

    settings = _read_persona_settings(persona, project_dir)
    if settings is not None:
        deny = settings.get("permissions", {}).get("deny", [])
        persona_hit = _persona_deny_for_bash(cmd, deny)
        if persona_hit:
            _log({
                "persona": persona, "agent_id": agent_id, "tool": tool,
                "decision": "deny", "reason": "persona", "hit": persona_hit,
                "command": cmd[:200],
            })
            _deny(
                f"ta-action-gate persona deny: '{persona_hit}' is not "
                f"permitted for {persona} (per its settings.json). STOP "
                f"and report to the orchestrator if the spawn prompt directed "
                f"this."
            )

    _log({
        "persona": persona, "agent_id": agent_id, "tool": tool,
        "decision": "defer", "command": cmd[:200],
    })
    _defer_and_exit()


if __name__ == "__main__":
    try:
        main()
    except Exception:
        sys.exit(0)
