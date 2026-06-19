/**
 * MCP tool-definition annotation contract.
 *
 * Annotations are the only metadata that travels with a tool to EVERY context
 * (main session, subagents, and deferred/tool-search listings). `readOnlyHint`
 * in particular lets a client (e.g. Claude Code) treat discovery tools as
 * auto-approvable instead of prompting on every call — the friction that
 * otherwise pushes agents back onto native Grep/Read. These tests pin the
 * read-only/mutating split so it can never silently regress (a tool that gains
 * a mutating path must drop `readOnlyHint`, and vice-versa).
 */

import { describe, it, expect } from 'vitest';
import { getToolDefinitions } from '../src/tool-definitions/index.js';

// Tools whose every action only reads (telemetry side-effects don't count as
// environment mutation — the regular `search` logs events too).
const READ_ONLY = new Set([
  'search',
  'retrieve',
  'grep',
  'list',
  'embedding',
  'search_eval',
  'graph',
]);

// Tools with at least one persistent-state mutation — must NOT claim read-only.
const MUTATING = new Set(['rules', 'store', 'scratchpad', 'workspace_index']);

type Annotated = {
  name: string;
  annotations?: {
    title?: string;
    readOnlyHint?: boolean;
    openWorldHint?: boolean;
  };
};

const defs = getToolDefinitions() as unknown as ReadonlyArray<Annotated>;
const byName = new Map(defs.map((d) => [d.name, d]));

describe('tool definition annotations', () => {
  it('exposes exactly the expected tools', () => {
    expect(new Set(defs.map((d) => d.name))).toEqual(new Set([...READ_ONLY, ...MUTATING]));
  });

  it('gives every tool a human-readable title and a local (closed-world) hint', () => {
    for (const d of defs) {
      expect(d.annotations, `${d.name} must carry annotations`).toBeDefined();
      expect(typeof d.annotations?.title, `${d.name} needs a title`).toBe('string');
      expect((d.annotations?.title?.length ?? 0) > 0, `${d.name} title non-empty`).toBe(true);
      // The whole server answers from a local index/daemon, never the open web.
      expect(d.annotations?.openWorldHint, `${d.name} is local-only`).toBe(false);
    }
  });

  it('marks read-only discovery tools readOnlyHint:true', () => {
    for (const name of READ_ONLY) {
      expect(byName.get(name)?.annotations?.readOnlyHint, `${name} should be read-only`).toBe(true);
    }
  });

  it('never claims readOnlyHint on tools that can mutate persistent state', () => {
    for (const name of MUTATING) {
      expect(
        byName.get(name)?.annotations?.readOnlyHint,
        `${name} must not claim read-only`
      ).not.toBe(true);
    }
  });
});
