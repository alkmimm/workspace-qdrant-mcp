/**
 * Tool dispatcher — executes a named MCP tool and returns its result.
 *
 * Extracted from WorkspaceQdrantMcpServer.handleToolCall to keep server.ts
 * within the 300-line file-size limit.
 */

import { randomUUID } from 'node:crypto';

import type { SessionState } from './server-types.js';
import { logToolCall } from './utils/logger.js';
import { SERVER_VERSION as MCP_SERVER_VERSION } from './server-types.js';
import type { DaemonClient } from './clients/daemon-client.js';
import { finishToolEvent, logSearchEvent } from './clients/search-event-queries.js';
import type { ServerComponents } from './server-factory.js';
import {
  buildSearchOptions,
  buildRetrieveOptions,
  buildRuleOptions,
  buildStoreOptions,
  buildScratchpadOptions,
  buildGrepOptions,
  buildListOptions,
} from './tool-builders/index.js';
import { storeUrl, storeScratchpad } from './store-handlers.js';
import { handleEmbedding } from './tools/embedding.js';
import { handleWorkspaceIndex } from './tools/workspace-index.js';
import { handleGraph } from './tools/graph.js';
import { runSearchEval } from './tools/search-eval.js';
import { ensureProjectFresh, registerProjectFromTool, sendHeartbeat } from './session-lifecycle.js';
import { withToolMetrics } from './telemetry/metrics.js';
import mcpPublicConfig from './constants/mcp-public-config.json' with { type: 'json' };

export type ToolResult = {
  content: Array<{ type: string; text: string }>;
  /** Machine-validatable mirror of the (object) result for read tools that
   *  declare an outputSchema. The same JSON is ALSO serialized into the
   *  TextContent above, so clients that don't consume structuredContent are
   *  unaffected (MCP spec: provide both). */
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
};

/** Read tools whose result is a single JSON object safe to mirror into
 *  structuredContent — kept in sync with the outputSchema-bearing definitions
 *  in tool-definitions/index.ts. */
const STRUCTURED_OUTPUT_TOOLS = new Set(['search', 'grep', 'list', 'retrieve', 'graph']);

// Derived from src/constants/mcp-public-config.json (single source of truth).
// publicTools = tools exposed in client `enabled_tools` lists.
// internalTools = tools the server accepts but not advertised to clients by default.
export const KNOWN_TOOLS = [
  ...mcpPublicConfig.publicTools,
  ...mcpPublicConfig.internalTools,
] as const;

/**
 * Best-effort result_count for a rules/scratchpad response: the length of the
 * first list-shaped field, else 1 for a successful mutation, else 0.
 */
export function countOpResults(result: unknown): number {
  if (result !== null && typeof result === 'object') {
    const r = result as Record<string, unknown>;
    for (const key of ['rules', 'entries', 'results']) {
      const v = r[key];
      if (Array.isArray(v)) return v.length;
    }
    if (r['success'] === true) return 1;
  }
  return 0;
}

/**
 * Instrument a rules/scratchpad call with the same `search_events` records the
 * read tools emit (`op` = tool name, `query_text` = the action).
 *
 * Field feedback 2026-07-15: reported "rules timeouts" were invisible in
 * telemetry because these tools logged NO events at all — the investigation
 * only closed thanks to the JSONL tool-call logs. Latency, outcome and
 * result_count now land in the same queryable store as search/grep/exact/list
 * (CLAUDE.md shared-behavior rule). Fire-and-forget on both sides; an
 * instrumentation failure never breaks the tool call.
 */
async function withOpEvent(
  daemonClient: DaemonClient,
  op: 'rules' | 'scratchpad',
  args: Record<string, unknown> | undefined,
  run: () => Promise<unknown>
): Promise<unknown> {
  const eventId = randomUUID();
  const startTime = Date.now();
  logSearchEvent(daemonClient, {
    id: eventId,
    actor: 'claude',
    tool: 'mcp_qdrant',
    op,
    queryText: typeof args?.['action'] === 'string' ? (args['action'] as string) : undefined,
    projectId: typeof args?.['projectId'] === 'string' ? (args['projectId'] as string) : undefined,
  });
  try {
    const result = await run();
    const bytesOut = JSON.stringify(result)?.length ?? 0;
    finishToolEvent(daemonClient, eventId, {
      resultCount: countOpResults(result),
      latencyMs: Date.now() - startTime,
      // No "what the agent would have paid instead" notion for these ops —
      // floor bytesIn at bytesOut so the token_savings view never counts
      // fabricated savings (same floor rule as computeGrepEconomy).
      bytesIn: bytesOut,
      bytesOut,
      toolVersion: MCP_SERVER_VERSION,
    });
    return result;
  } catch (error) {
    finishToolEvent(daemonClient, eventId, {
      resultCount: 0,
      latencyMs: Date.now() - startTime,
      bytesIn: 0,
      bytesOut: 0,
      toolVersion: MCP_SERVER_VERSION,
      outcome: 'error',
    });
    throw error;
  }
}

/** Dispatch the 'store' tool subtypes. */
async function dispatchStore(
  args: Record<string, unknown> | undefined,
  components: ServerComponents,
  sessionState: SessionState
): Promise<unknown> {
  // Resolve the target type. Explicit `type` wins; otherwise INFER it from an
  // unambiguous signal so callers that clearly mean a library/project/url don't
  // have to restate `type` (libraryName → library, path → project, url → url).
  // Only a bare {content} with no signal is genuinely ambiguous — there we error
  // rather than silently defaulting to "library" (the trap #136 removed).
  let storeType = args?.['type'] as string | undefined;
  if (!storeType) {
    if (args?.['libraryName'] !== undefined || args?.['forProject'] === true) storeType = 'library';
    else if (args?.['path'] !== undefined) storeType = 'project';
    else if (args?.['url'] !== undefined) storeType = 'url';
  }
  if (!storeType) {
    throw new Error(
      "store needs a `type` (or an unambiguous arg): 'scratchpad' for a note (content), " +
        "'library' (libraryName), 'url' (url), or 'project' (path)."
    );
  }
  if (storeType === 'project')
    return registerProjectFromTool(args, sessionState, components.daemonClient);
  if (storeType === 'url') return storeUrl(args, components.stateManager, sessionState);
  if (storeType === 'scratchpad')
    return storeScratchpad(args, components.stateManager, components.projectDetector, sessionState);
  return components.storeTool.store(buildStoreOptions(args, sessionState));
}

/**
 * Dispatch a tool call to the appropriate handler.
 *
 * Fires an implicit heartbeat (fire-and-forget) before dispatching so that
 * active sessions keep their daemon connection alive without adding latency.
 */
/**
 * Route a validated tool name to its handler and return the raw result.
 *
 * Exported so the admin "tools playground" can invoke a tool through the exact
 * same path an MCP client uses (no parallel mock), getting the raw result
 * object back to render. Callers outside the MCP transport must validate the
 * tool name against {@link KNOWN_TOOLS} first — this throws on an unknown name.
 */
export async function routeTool(
  toolName: string,
  args: Record<string, unknown> | undefined,
  components: ServerComponents,
  sessionState: SessionState
): Promise<unknown> {
  const {
    searchTool,
    retrieveTool,
    rulesTool,
    grepTool,
    listTool,
    healthMonitor,
    daemonClient,
    projectDetector,
  } = components;
  switch (toolName) {
    case 'search': {
      const searchResult = await searchTool.search(buildSearchOptions(args));
      return healthMonitor.augmentSearchResults({ success: true, ...searchResult });
    }
    case 'retrieve':
      return retrieveTool.retrieve(buildRetrieveOptions(args));
    case 'rules':
      return withOpEvent(daemonClient, 'rules', args, () =>
        rulesTool.execute(buildRuleOptions(args))
      );
    case 'store':
      return dispatchStore(args, components, sessionState);
    case 'scratchpad':
      return withOpEvent(daemonClient, 'scratchpad', args, () =>
        components.scratchpadTool.execute(buildScratchpadOptions(args))
      );
    case 'grep':
      return grepTool.grep(buildGrepOptions(args));
    case 'list':
      return listTool.list(buildListOptions(args));
    case 'embedding':
      return handleEmbedding(args, daemonClient);
    case 'workspace_index':
      return handleWorkspaceIndex(args, daemonClient, projectDetector);
    case 'graph':
      return handleGraph(args, daemonClient, projectDetector);
    case 'search_eval':
      return runSearchEval(searchTool, projectDetector, args);
    default:
      throw new Error(`Unexpected tool: ${toolName}`);
  }
}

export async function dispatchToolCall(
  toolName: string,
  args: Record<string, unknown> | undefined,
  components: ServerComponents,
  sessionState: SessionState
): Promise<ToolResult> {
  const startTime = Date.now();

  sendHeartbeat(sessionState, components.daemonClient);

  // Refresh cached git state (branch + worktree flag) if stale. Cheap inside
  // the TTL window; ~3ms `git` invocation outside it.
  ensureProjectFresh(sessionState);

  if (!KNOWN_TOOLS.includes(toolName as (typeof KNOWN_TOOLS)[number])) {
    logToolCall(toolName, Date.now() - startTime, false, { error: 'Unknown tool' });
    return { content: [{ type: 'text', text: `Unknown tool: ${toolName}` }], isError: true };
  }

  try {
    const result = await withToolMetrics(toolName, () =>
      routeTool(toolName, args, components, sessionState)
    );
    logToolCall(toolName, Date.now() - startTime, true);
    const text = JSON.stringify(result, null, 2);
    // Read tools mirror their object result into structuredContent (validated
    // against the declared outputSchema) while TextContent stays the universal
    // fallback. Non-object / array results keep content-only.
    if (
      STRUCTURED_OUTPUT_TOOLS.has(toolName) &&
      result !== null &&
      typeof result === 'object' &&
      !Array.isArray(result)
    ) {
      return {
        content: [{ type: 'text', text }],
        structuredContent: result as Record<string, unknown>,
      };
    }
    return { content: [{ type: 'text', text }] };
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    logToolCall(toolName, Date.now() - startTime, false, { error: errorMessage });
    return { content: [{ type: 'text', text: `Error: ${errorMessage}` }], isError: true };
  }
}
