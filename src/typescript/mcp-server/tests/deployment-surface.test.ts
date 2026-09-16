/**
 * Deployment-level surface knobs: which tools a server ADVERTISES and which
 * instructions it sends.
 *
 * The catalog is ~15k tokens and its instructions name `search`/`graph` as the
 * first thing to call. A deployment that wants to offer a narrower surface —
 * a lexical-only server, a semantic one without the graph — needs the server
 * to expose the subset and to send a matching prompt; a client-side allowlist
 * only blocks calls the agent has already read about and reasoned over. These
 * pin the two knobs so a misconfigured experiment fails loudly (unknown names
 * are reported, a missing instructions file throws) instead of silently
 * running with the full surface.
 */

import { describe, it, expect } from 'vitest';

import {
  exposedToolNames,
  getExposedToolDefinitions,
  getToolDefinitions,
} from '../src/tool-definitions/index.js';
import { resolveServerInstructions, SERVER_INSTRUCTIONS } from '../src/server-instructions.js';
import { scratchpadLaneEnabled } from '../src/tools/search-helpers.js';
import { qdrantSearchParams } from '../src/tools/search-qdrant.js';

describe('WQM_MCP_TOOLS — advertised tool subset', () => {
  it('exposes everything when unset or blank', () => {
    expect(exposedToolNames({})).toBeUndefined();
    expect(exposedToolNames({ WQM_MCP_TOOLS: '  ' })).toBeUndefined();
    const all = getToolDefinitions().map((t) => t.name);
    expect(getExposedToolDefinitions({}).tools.map((t) => t.name)).toEqual(all);
  });

  it('narrows the catalog to the named tools, in catalog order', () => {
    const { tools, unknown } = getExposedToolDefinitions({
      WQM_MCP_TOOLS: 'graph, grep,list ,retrieve',
    });
    expect(tools.map((t) => t.name)).toEqual(['retrieve', 'grep', 'list', 'graph']);
    expect(unknown).toEqual([]);
  });

  it('reports unknown names and never widens back to the full catalog', () => {
    const { tools, unknown } = getExposedToolDefinitions({
      WQM_MCP_TOOLS: 'grep,serach,grpah',
    });
    expect(tools.map((t) => t.name)).toEqual(['grep']);
    expect(unknown).toEqual(['grpah', 'serach']);
    // All-unknown ⇒ an EMPTY surface (a typo must be noticed, not papered over).
    expect(getExposedToolDefinitions({ WQM_MCP_TOOLS: 'nope' }).tools).toEqual([]);
  });
});

describe('WQM_MCP_INSTRUCTIONS_FILE — instructions override', () => {
  it('sends the built-in kernel when unset', () => {
    expect(resolveServerInstructions({})).toBe(SERVER_INSTRUCTIONS);
  });

  it('replaces the kernel with the file contents, trimmed', () => {
    const read = (path: string): string => {
      expect(path).toBe('/etc/wqm/instructions.txt');
      return '  Use grep for identifiers. Nothing else.\n';
    };
    expect(
      resolveServerInstructions({ WQM_MCP_INSTRUCTIONS_FILE: '/etc/wqm/instructions.txt' }, read)
    ).toBe('Use grep for identifiers. Nothing else.');
  });

  it('an empty file means NO instructions (undefined), not the default', () => {
    expect(
      resolveServerInstructions({ WQM_MCP_INSTRUCTIONS_FILE: '/dev/null' }, () => '\n')
    ).toBeUndefined();
  });

  it('a file that cannot be read is a startup error, not a silent fallback', () => {
    expect(() =>
      resolveServerInstructions({ WQM_MCP_INSTRUCTIONS_FILE: '/missing.txt' }, () => {
        throw new Error('ENOENT');
      })
    ).toThrow(/WQM_MCP_INSTRUCTIONS_FILE=\/missing\.txt could not be read: ENOENT/);
  });
});

describe('deployment knobs that freeze a measurement', () => {
  it('scratchpad lane: explicit per-call value wins, env sets the default', () => {
    expect(scratchpadLaneEnabled(undefined, {})).toBe(true);
    expect(scratchpadLaneEnabled(undefined, { WQM_SEARCH_SCRATCHPAD_LANE: '0' })).toBe(false);
    expect(scratchpadLaneEnabled(undefined, { WQM_SEARCH_SCRATCHPAD_LANE: '1' })).toBe(true);
    // An explicit request overrides the deployment default either way.
    expect(scratchpadLaneEnabled(true, { WQM_SEARCH_SCRATCHPAD_LANE: '0' })).toBe(true);
    expect(scratchpadLaneEnabled(false, {})).toBe(false);
  });

  it('exact vector search: only WQM_QDRANT_EXACT_SEARCH=1 turns HNSW off', () => {
    expect(qdrantSearchParams({})).toBeUndefined();
    expect(qdrantSearchParams({ WQM_QDRANT_EXACT_SEARCH: '0' })).toBeUndefined();
    expect(qdrantSearchParams({ WQM_QDRANT_EXACT_SEARCH: 'true' })).toBeUndefined();
    expect(qdrantSearchParams({ WQM_QDRANT_EXACT_SEARCH: '1' })).toEqual({ exact: true });
  });
});
