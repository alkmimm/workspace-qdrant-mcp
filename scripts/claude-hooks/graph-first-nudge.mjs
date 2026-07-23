#!/usr/bin/env node
/**
 * PreToolUse hook — "graph-first" nudge for workspace-qdrant.
 *
 * Learned from graphify (github.com/Graphify-Labs/graphify): the strongest
 * lever for MCP adoption is a PreToolUse hook that steers the agent to the
 * knowledge graph / index BEFORE it greps raw files. This addresses our
 * documented pain ("subagents skip the MCP: 0 MCP calls across 59 transcripts").
 *
 * Fires before native Grep/Glob. Depending on WQM_NUDGE_MODE it either injects
 * a one-time reminder (soft) or blocks the first raw search of the session
 * (strict), pointing the agent at mcp__workspace-qdrant__{search,grep,graph}.
 *
 * Contract (Claude Code PreToolUse, code.claude.com/docs/en/hooks):
 *   stdin  : { session_id, hook_event_name, tool_name, tool_input, cwd, ... }
 *   stdout : { hookSpecificOutput: { hookEventName, permissionDecision,
 *              permissionDecisionReason?, additionalContext? } }
 *   allow + additionalContext  -> tool runs, model gets the reminder
 *   deny  + permissionDecisionReason -> tool blocked, model reads the reason
 *   (no output / exit 0)       -> defer to normal permission flow
 *
 * Modes (env WQM_NUDGE_MODE): off | once (default) | always | strict
 *   once   : soft nudge on the FIRST Grep/Glob of the session, then silent
 *   always : soft nudge on every Grep/Glob
 *   strict : DENY the first Grep/Glob of the session (force MCP-first), then silent
 *   off    : do nothing
 *
 * Fail-open by design: any error -> emit nothing, exit 0. A hook must never
 * crash the agent.
 */

import { readFileSync, mkdirSync, existsSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/** Exit without output => Claude Code uses its normal permission flow. */
function defer() {
  process.exit(0);
}

function emit(hookSpecificOutput) {
  process.stdout.write(JSON.stringify({ hookSpecificOutput }));
  process.exit(0);
}

try {
  const mode = (process.env.WQM_NUDGE_MODE || 'once').toLowerCase();
  if (mode === 'off') defer();

  let input;
  try {
    input = JSON.parse(readFileSync(0, 'utf8') || '{}');
  } catch {
    defer();
  }

  const tool = input.tool_name || '';
  // Matcher should already scope this to Grep|Glob, but never assume.
  if (tool !== 'Grep' && tool !== 'Glob') defer();

  const sessionId = String(input.session_id || 'no-session');
  const ti = input.tool_input || {};
  const pattern = ti.pattern || ti.query || '';

  // Per-session first-occurrence marker (session_id is stable per session).
  const stateDir = join(tmpdir(), 'wqm-graph-nudge');
  const safeId = sessionId.replace(/[^A-Za-z0-9_-]/g, '_');
  const marker = join(stateDir, `${safeId}.seen`);
  let firstThisSession = true;
  try {
    if (existsSync(marker)) {
      firstThisSession = false;
    } else {
      mkdirSync(stateDir, { recursive: true });
      writeFileSync(marker, String(Date.now()));
    }
  } catch {
    /* fail-open: if the marker can't be written, behave as "first" — worst case
       is one extra soft nudge, never a crash. */
  }

  // 'once' and 'strict' act only on the first Grep/Glob of the session.
  if ((mode === 'once' || mode === 'strict') && !firstThisSession) defer();

  const forPattern = pattern ? ` for \`${pattern}\`` : '';
  // Layered-retrieval discipline (adapted from graphify's "graph -> notes -> raw
  // files" rule): a native search is the LAST layer, not the first.
  const reason =
    `Indexed workspace-qdrant project — a native ${tool}${forPattern} only sees files ` +
    `already on disk in THIS worktree. Search in layers (widest & cheapest first); a native ` +
    `${tool} is the LAST layer, not the first:\n` +
    `1. mcp__workspace-qdrant__search — locate by meaning/concept (queries in English; add ` +
    `fileType:"code" or a pathGlob for implementation) — or mcp__workspace-qdrant__graph ` +
    `(action:"impact"/"usages") for callers, change-impact, relationships.\n` +
    `2. mcp__workspace-qdrant__grep — exact / regex substring over the whole FTS index ` +
    `(branch-aware; covers files you haven't opened).\n` +
    `3. prior context — scratchpad & rules: search collection:"scratchpad", or the rules tool.\n` +
    `Only THEN native ${tool}/Read, to pull exact bytes once a layer above located the file.`;

  if (mode === 'strict') {
    emit({
      hookEventName: 'PreToolUse',
      permissionDecision: 'deny',
      permissionDecisionReason: reason,
    });
  }

  emit({
    hookEventName: 'PreToolUse',
    permissionDecision: 'allow',
    additionalContext: reason,
  });
} catch {
  // Absolute backstop — never block the agent on an unexpected error.
  defer();
}
