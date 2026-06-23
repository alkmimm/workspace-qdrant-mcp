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
    destructiveHint?: boolean;
    idempotentHint?: boolean;
  };
  inputSchema?: { required?: string[] };
  outputSchema?: { type?: string };
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

describe('mutating tool action hints', () => {
  it('declares destructiveHint & idempotentHint on every mutating tool', () => {
    for (const name of MUTATING) {
      const a = byName.get(name)?.annotations;
      expect(typeof a?.destructiveHint, `${name} needs a destructiveHint`).toBe('boolean');
      expect(typeof a?.idempotentHint, `${name} needs an idempotentHint`).toBe('boolean');
    }
  });

  it('marks delete-capable tools destructive and the additive store non-destructive', () => {
    // store has no delete path → non-destructive (avoids a spurious "destructive"
    // prompt); rules/scratchpad/workspace_index each own a delete/cleanup action.
    expect(byName.get('store')?.annotations?.destructiveHint).toBe(false);
    expect(byName.get('scratchpad')?.annotations?.destructiveHint).toBe(true);
    expect(byName.get('rules')?.annotations?.destructiveHint).toBe(true);
    expect(byName.get('workspace_index')?.annotations?.destructiveHint).toBe(true);
  });

  it('requires an explicit `type` on store (no silent library default)', () => {
    expect(byName.get('store')?.inputSchema?.required).toContain('type');
  });
});

// Read tools declare a (permissive) outputSchema so clients can validate the
// structuredContent the dispatcher mirrors; write/eval/embedding tools do not.
const STRUCTURED_OUTPUT = new Set(['search', 'grep', 'list', 'retrieve', 'graph']);

describe('tool output schemas', () => {
  it('declares outputSchema on exactly the structured-output read tools', () => {
    for (const d of defs) {
      if (STRUCTURED_OUTPUT.has(d.name)) {
        expect(d.outputSchema, `${d.name} should declare outputSchema`).toBeDefined();
        expect(d.outputSchema?.type, `${d.name} outputSchema.type`).toBe('object');
      } else {
        expect(d.outputSchema, `${d.name} must not declare outputSchema`).toBeUndefined();
      }
    }
  });
});
