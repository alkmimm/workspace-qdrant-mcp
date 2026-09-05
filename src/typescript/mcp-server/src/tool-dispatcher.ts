/**
 * Tool dispatcher — executes a named MCP tool and returns its result.
 *
 * Extracted from WorkspaceQdrantMcpServer.handleToolCall to keep server.ts
 * within the 300-line file-size limit.
 */

import { randomUUID } from 'node:crypto';

import type { SessionState } from './server-types.js';
import { logToolCall } from './utils/logger.js';
import type { DaemonClient } from './clients/daemon-client.js';
import { logSearchEvent, updateSearchEvent } from './clients/search-event-queries.js';
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
import { storeUrl, storeScratchpad, storeFeedback } from './store-handlers.js';
import { handleEmbedding } from './tools/embedding.js';
import { handleHelp } from './tools/help.js';
import { handleWorkspaceIndex } from './tools/workspace-index.js';
import { handleGraph } from './tools/graph.js';
import { getQdrantClient } from './clients/qdrant-client-factory.js';
import { PROJECTS_COLLECTION } from './tools/retrieve-types.js';
import { runSearchEval } from './tools/search-eval.js';
import { resolveScopedTenant } from './tools/tenant-scope.js';
import {
  ensureClientProjectActive,
  ensureProjectFresh,
  registerProjectFromTool,
  sendHeartbeat,
} from './session-lifecycle.js';
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

/** Tools answered entirely from in-process constants. The dispatcher skips its
 *  session preamble (heartbeat RPC, git-state refresh, project activation) for
 *  these, so a static lookup can never block on the daemon, spawn git, or
 *  side-effect project registration — this set is what makes the "static,
 *  local" claim in their docs true, rather than merely asserted. Safe to skip:
 *  heartbeat cadence is already kept by the self-rescheduling timer in
 *  session-lifecycle.ts, and activation simply defers to the next non-static
 *  call. */
const STATIC_TOOLS = new Set(['help']);

// Derived from src/constants/mcp-public-config.json (single source of truth).
// publicTools = tools exposed in client `enabled_tools` lists.
// internalTools = tools the server accepts but not advertised to clients by default.
export const KNOWN_TOOLS = [
  ...mcpPublicConfig.publicTools,
  ...mcpPublicConfig.internalTools,
] as const;

/**
 * Tools instrumented at the DISPATCHER level with a `search_events` record
 * (`op` = tool name). The read tools (search / search_exact / grep / retrieve /
 * list) self-instrument inside their own implementations — richer records with
 * filters, topK, and the token-economy sidecar — and are deliberately NOT here.
 * Any op listed here must be accepted by the search_events op CHECK
 * (schema v49; see search_events_schema.rs).
 */
const OP_EVENT_TOOLS = new Set([
  'rules',
  'scratchpad',
  'graph',
  'store',
  'embedding',
  'workspace_index',
  'search_eval',
  // 'help' rides the same lane since schema v49 widened the op CHECK — its
  // adoption then shows in the dashboard's "by op & actor" panels. The event
  // write is fire-and-forget, so it does not undo the STATIC_TOOLS preamble
  // skip: a help RESPONSE still never waits on the daemon.
  'help',
]);

/**
 * Best-effort result_count for an op-event response: the length of the first
 * list-shaped field, else 1 for a successful call, else 0.
 */
export function countOpResults(result: unknown): number {
  if (result !== null && typeof result === 'object') {
    const r = result as Record<string, unknown>;
    for (const key of ['rules', 'entries', 'results', 'nodes', 'projects', 'branches']) {
      const v = r[key];
      if (Array.isArray(v)) return v.length;
    }
    if (r['success'] === true) return 1;
  }
  return 0;
}

/**
 * Instrument a tool call with the same start/finish `search_events` records the
 * read tools emit (`op` = tool name, `query_text` = the action/type arg).
 *
 * Field feedback 2026-07-15: reported "rules timeouts" were invisible in
 * telemetry because these tools logged NO events at all — the investigation
 * only closed thanks to the JSONL tool-call logs. Latency, outcome and
 * result_count now land in the same queryable store as search/grep/exact/list
 * (CLAUDE.md shared-behavior rule). Fire-and-forget on both sides; an
 * instrumentation failure never breaks the tool call.
 *
 * Deliberately NO token-economy sidecar: `token_savings` filters on
 * `bytes_in IS NOT NULL`, so writing bytes for ops that have no
 * "what-the-agent-would-have-paid" notion would inject savings_ratio=0 rows
 * and dilute the TCC dashboards. Op events carry latency/outcome/count only.
 */
async function withOpEvent(
  daemonClient: DaemonClient,
  op: string,
  args: Record<string, unknown> | undefined,
  run: () => Promise<unknown>
): Promise<unknown> {
  const eventId = randomUUID();
  const startTime = Date.now();
  // The op's "what was asked": action for the CRUD-style tools, type for
  // store, topic for help.
  const action = args?.['action'] ?? args?.['type'] ?? args?.['topic'];
  logSearchEvent(daemonClient, {
    id: eventId,
    actor: 'claude',
    tool: 'mcp_qdrant',
    op,
    queryText: typeof action === 'string' ? action : undefined,
    projectId: typeof args?.['projectId'] === 'string' ? (args['projectId'] as string) : undefined,
  });
  try {
    const result = await run();
    updateSearchEvent(daemonClient, eventId, {
      resultCount: countOpResults(result),
      latencyMs: Date.now() - startTime,
    });
    return result;
  } catch (error) {
    updateSearchEvent(daemonClient, eventId, {
      resultCount: 0,
      latencyMs: Date.now() - startTime,
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
  if (storeType === 'url')
    return storeUrl(args, components.stateManager, components.projectDetector, sessionState);
  if (storeType === 'scratchpad')
    return storeScratchpad(args, components.stateManager, components.projectDetector, sessionState);
  if (storeType === 'feedback')
    return storeFeedback(args, components.stateManager, sessionState, components.projectDetector);
  return storeLibrary(args, components, sessionState);
}

/**
 * `store(type:"library")`. A named library is its own tenant; `forProject:true`
 * instead scopes the entry to a PROJECT, so its tenant goes through the shared
 * write resolver ({@link resolveScopedTenant}) — same precedence as the read
 * surfaces and the scratchpad path, rather than the session project alone.
 * A `'fallback'` source means nothing resolved: leave `projectId` unset so
 * `StoreTool` raises its own "no active project" error instead of quietly
 * writing the entry to the global tenant.
 */
async function storeLibrary(
  args: Record<string, unknown> | undefined,
  components: ServerComponents,
  sessionState: SessionState
): Promise<unknown> {
  const options = buildStoreOptions(args);
  if (!options.forProject) return components.storeTool.store(options);

  const scoped = await resolveScopedTenant({
    explicitProjectId: args?.['projectId'],
    projectDetector: components.projectDetector,
    sessionProjectId: sessionState.projectId,
    stateManager: components.stateManager,
  });
  if (scoped.source !== 'fallback') options.projectId = scoped.tenantId;

  const result = await components.storeTool.store(options);
  if (!result.success || scoped.source === 'fallback') return result;
  // StoreTool's own message already carries `libraries/<tenant>`, so only the
  // project PATH is added here — repeating the tenant id would just be noise.
  return {
    ...result,
    ...(scoped.projectPath ? { message: `${result.message} — ${scoped.projectPath}` } : {}),
    project_id: scoped.tenantId,
    ...(scoped.projectPath ? { project_path: scoped.projectPath } : {}),
  };
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
  if (OP_EVENT_TOOLS.has(toolName)) {
    return withOpEvent(components.daemonClient, toolName, args, () =>
      routeToolInner(toolName, args, components, sessionState)
    );
  }
  return routeToolInner(toolName, args, components, sessionState);
}

/** The actual tool switch — see {@link routeTool} for the instrumented entry. */
async function routeToolInner(
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
      return rulesTool.execute(buildRuleOptions(args));
    case 'store':
      return dispatchStore(args, components, sessionState);
    case 'scratchpad':
      return components.scratchpadTool.execute(buildScratchpadOptions(args));
    case 'grep':
      return grepTool.grep(buildGrepOptions(args));
    case 'list':
      return listTool.list(buildListOptions(args));
    case 'embedding':
      return handleEmbedding(args, daemonClient);
    case 'help':
      // Static topical manual. The handler reads only in-process constants;
      // the dispatch-level preamble is skipped via STATIC_TOOLS above.
      return handleHelp(args);
    case 'workspace_index': {
      const { qdrantUrl, qdrantApiKey } = components.qdrantConfig;
      // Issue #299: let indexing_status/project_status cross-check the vector
      // lane. Count the canonical tenant's points in the `projects` collection —
      // the same lane semantic search reads — so a tenant re-key that emptied it
      // surfaces as `degraded` instead of a false `complete`. Only invoked when
      // the queue already looks drained-and-non-empty; any failure resolves null.
      const probeQdrantPointCount = async (tenantId: string): Promise<number | null> => {
        try {
          const client = getQdrantClient({ url: qdrantUrl, apiKey: qdrantApiKey });
          const res = await client.count(PROJECTS_COLLECTION, {
            filter: { must: [{ key: 'tenant_id', match: { value: tenantId } }] },
            exact: true,
          });
          return typeof res?.count === 'number' ? res.count : null;
        } catch {
          return null;
        }
      };
      return handleWorkspaceIndex(args, daemonClient, projectDetector, probeQdrantPointCount);
    }
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

  if (!STATIC_TOOLS.has(toolName)) {
    sendHeartbeat(sessionState, components.daemonClient);

    // Refresh cached git state (branch + worktree flag) if stale. Cheap inside
    // the TTL window; ~3ms `git` invocation outside it.
    ensureProjectFresh(sessionState);

    // Lazily activate the connecting client's project from THIS call's cwd (bound
    // into the request context above by handleToolCall). Fire-and-forget: the
    // resolve/register is off the tool's latency path, and the current call's
    // scoping already uses the cwd directly. This is what lets a non-wqm client
    // repo become `is_active` — see ensureClientProjectActive. `cwd` is captured
    // synchronously inside it, before any await unwinds the request context.
    void ensureClientProjectActive(
      sessionState,
      components.daemonClient,
      components.projectDetector
    );
  }

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
