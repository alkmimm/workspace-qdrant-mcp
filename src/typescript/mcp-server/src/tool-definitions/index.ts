/**
 * MCP tool schema definitions for ListTools response.
 * Re-exports all per-tool schemas and assembles the full tool list.
 */

export { searchToolDefinition } from './search.js';
export { retrieveToolDefinition } from './retrieve.js';
export { rulesToolDefinition } from './rules.js';
export { storeToolDefinition } from './store.js';
export { scratchpadToolDefinition } from './scratchpad.js';
export { grepToolDefinition } from './grep.js';
export { listToolDefinition } from './list.js';
export { embeddingToolDefinition } from './embedding.js';
export { workspaceIndexToolDefinition } from './workspace-index.js';
export { searchEvalToolDefinition } from './search-eval.js';
export { graphToolDefinition } from './graph.js';
export { helpToolDefinition } from './help.js';

import { searchToolDefinition } from './search.js';
import { retrieveToolDefinition } from './retrieve.js';
import { rulesToolDefinition } from './rules.js';
import { storeToolDefinition } from './store.js';
import { scratchpadToolDefinition } from './scratchpad.js';
import { grepToolDefinition } from './grep.js';
import { listToolDefinition } from './list.js';
import { embeddingToolDefinition } from './embedding.js';
import { workspaceIndexToolDefinition } from './workspace-index.js';
import { searchEvalToolDefinition } from './search-eval.js';
import { graphToolDefinition } from './graph.js';
import { helpToolDefinition } from './help.js';
import {
  SEARCH_OUTPUT_SCHEMA,
  GREP_OUTPUT_SCHEMA,
  LIST_OUTPUT_SCHEMA,
  RETRIEVE_OUTPUT_SCHEMA,
  GRAPH_OUTPUT_SCHEMA,
} from './output-schemas.js';

/** Shape shared by every MCP tool definition assembled below. `annotations`
 *  is optional in the MCP spec but every tool here carries one (read-only /
 *  closed-world hints + a display title) — see `tests/tool-definitions.test.ts`. */
export interface McpToolDefinition {
  name: string;
  description: string;
  annotations?: {
    title?: string;
    readOnlyHint?: boolean;
    openWorldHint?: boolean;
    idempotentHint?: boolean;
    destructiveHint?: boolean;
  };
  inputSchema: {
    type: 'object';
    properties: Record<string, unknown>;
    required?: readonly string[];
  };
  /** Optional MCP outputSchema (JSON Schema) so a client can validate/route the
   *  tool's `structuredContent`. Declared by read tools only (centrally in
   *  getToolDefinitions); kept permissive (additionalProperties) because
   *  responses also carry health/indexing/status fields. */
  outputSchema?: {
    type: 'object';
    properties?: Record<string, unknown>;
    required?: readonly string[];
    additionalProperties?: boolean;
  };
}

/**
 * Env var: comma-separated tool names this deployment EXPOSES. Unset/empty =
 * every tool. Names not in the catalog are ignored (with a warning), so a typo
 * narrows the surface rather than silently widening it back to everything.
 *
 * Why: the catalog is ~53 KB of JSON (~15k tokens) and its `instructions`
 * steer the agent toward `search` and `graph` by name. A deployment that
 * wants to offer ONLY a lexical surface (grep/list/retrieve), or a semantic
 * one without the graph, could not: a client-side allowlist blocks the CALLS
 * but the agent still sees, reads and reasons about every tool. Exposing a
 * subset at the server is what makes "condition B = lexical only" a property
 * of the deployment instead of a hope about agent behaviour.
 */
export const MCP_TOOLS_ENV = 'WQM_MCP_TOOLS';

/** Parse `WQM_MCP_TOOLS`; `undefined` means "expose everything". */
export function exposedToolNames(
  env: Record<string, string | undefined> = process.env
): Set<string> | undefined {
  const raw = (env[MCP_TOOLS_ENV] ?? '').trim();
  if (raw.length === 0) return undefined;
  return new Set(
    raw
      .split(',')
      .map((t) => t.trim())
      .filter((t) => t.length > 0)
  );
}

/**
 * The tool definitions this deployment advertises: the full catalog, narrowed
 * by `WQM_MCP_TOOLS` when set. Returns the unknown names too, so the caller can
 * log them once instead of every ListTools.
 */
export function getExposedToolDefinitions(env: Record<string, string | undefined> = process.env): {
  tools: McpToolDefinition[];
  unknown: string[];
} {
  const all = getToolDefinitions();
  const allow = exposedToolNames(env);
  if (!allow) return { tools: all, unknown: [] };
  const known = new Set(all.map((t) => t.name));
  return {
    tools: all.filter((t) => allow.has(t.name)),
    unknown: [...allow].filter((n) => !known.has(n)).sort(),
  };
}

/**
 * Returns the full list of tool definitions for the ListTools MCP response
 */
export function getToolDefinitions(): McpToolDefinition[] {
  // outputSchema is attached here (not in each tool-def file) so the read tools
  // gain a validated structuredContent contract with minimal churn; write tools
  // and eval/embedding stay output-schema-less.
  return [
    { ...searchToolDefinition, outputSchema: SEARCH_OUTPUT_SCHEMA },
    { ...retrieveToolDefinition, outputSchema: RETRIEVE_OUTPUT_SCHEMA },
    rulesToolDefinition,
    storeToolDefinition,
    scratchpadToolDefinition,
    { ...grepToolDefinition, outputSchema: GREP_OUTPUT_SCHEMA },
    { ...listToolDefinition, outputSchema: LIST_OUTPUT_SCHEMA },
    embeddingToolDefinition,
    workspaceIndexToolDefinition,
    searchEvalToolDefinition,
    { ...graphToolDefinition, outputSchema: GRAPH_OUTPUT_SCHEMA },
    helpToolDefinition,
  ];
}
