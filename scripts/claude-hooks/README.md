# Claude Code hooks — graph-first nudge (prototype)

`graph-first-nudge.mjs` is a **PreToolUse** hook that steers the agent toward the
`workspace-qdrant` MCP tools before it runs a native `Grep`/`Glob`.

**Why.** Field telemetry shows agents (especially subagents) skip the MCP and
fall back to native search — "0 MCP calls across 59 transcripts". Documentation
alone (the CLAUDE.md subagent preamble) doesn't fix it. This is the lever
[graphify](https://github.com/Graphify-Labs/graphify) uses: a PreToolUse hook
that makes the assistant consult the graph/index *before* grepping raw files.
The harness enforces a hook; a doc only hopes.

## What it does

Fires before `Grep`/`Glob`. Reads the PreToolUse JSON on stdin
(`session_id`, `tool_name`, `tool_input`, …) and, depending on the mode, either
injects a one-time reminder or blocks the first raw search of the session,
pointing at `mcp__workspace-qdrant__{search,grep,graph}`.

It is **fail-open**: any error → no output → normal permission flow. Exit code is
always 0; it can never crash the agent.

### Modes — `WQM_NUDGE_MODE`

| Mode | Behaviour |
|------|-----------|
| `once` (default) | Soft nudge (`allow` + `additionalContext`) on the **first** `Grep`/`Glob` of the session, then silent. |
| `always` | Soft nudge on every `Grep`/`Glob`. |
| `strict` | **Deny** (`permissionDecision: deny`) the first `Grep`/`Glob` of the session — forces an MCP-first pass — then silent. |
| `off` | No-op. |

First-occurrence is tracked with a per-session marker file
(`<tmp>/wqm-graph-nudge/<session_id>.seen`); `session_id` is stable per session.

## Install

Add to `~/.claude/settings.json` (global) or `.claude/settings.json` (project),
using Claude Code's nested hook format (same shape the project's `SessionStart`
hook uses):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Grep|Glob",
        "hooks": [
          {
            "type": "command",
            "command": "node \"<abs path>/scripts/claude-hooks/graph-first-nudge.mjs\"",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

`matcher` is a regex over the tool name; `Grep|Glob` covers both. Use the
host-visible absolute path to the script (on Windows + WSL, the `\\wsl.localhost\…`
UNC path or a copy on a local drive). To try strict mode, set
`WQM_NUDGE_MODE=strict` in the environment Claude Code launches hooks in.

## Tested

`spawnSync` harness feeding synthetic PreToolUse payloads (see the session that
introduced this): first-Grep → `allow`+context; second-Grep same session →
defer; first-Glob → nudge; `strict` first-Grep → `deny`+reason; `off` → defer;
non-search tool → defer. All exit 0.

## Production path (recommended)

This prototype is a standalone Node script. For shipping, fold the logic into the
`wqm` binary as `wqm hooks nudge` (read stdin, same modes) so it:

- matches the project's existing CLI-based hook pattern (`wqm session start/end`);
- is a single cross-platform binary already in PATH — no Node dependency;
- can cheaply gate the nudge on "**cwd is an indexed tenant**" via the daemon
  (skip the nudge outside indexed repos), which this prototype does not do;
- installs via `wqm hooks install` alongside the SessionStart/End hooks.
