/**
 * `graph` MCP tool — code-relationship graph navigation.
 *
 * Surfaces the daemon's GraphService (edges built from tree-sitter symbol
 * relations during ingestion) to MCP clients. Read-only. Actions:
 *   - stats     : node/edge counts by type (GetGraphStats)
 *   - relations : a symbol's dependencies N hops out (QueryRelated). Defaults to
 *                 dependency edges (excludes CONTAINS membership; pass
 *                 edgeTypes:["CONTAINS"] to list members instead).
 *   - impact    : what transitively depends on a symbol (ImpactAnalysis)
 *   - hotspots  : most central symbols by PageRank (ComputePageRank)
 *   - modules   : code communities/clusters (DetectCommunities)
 *
 * Tenant resolution mirrors `search`/`grep`/`list`: an explicit `projectId`
 * wins; otherwise the caller's `cwd` is resolved to its project (so `graph`
 * operates on the SAME project as the other tools). It does NOT fall back to
 * "first active project" — that silently returned a different project's graph
 * when the cwd didn't match — it errors instead, asking for `projectId`/`cwd`.
 */

import { createHash } from 'node:crypto';

import type { DaemonClient } from '../clients/daemon-client.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import { resolveProjectIdentity } from './branch-scope.js';
import type {
  ImpactAnalysisRequest,
  PageRankRequest,
  CommunityRequest,
  BetweennessRequest,
  CycleRequest,
  TestGapsRequest,
  QueryRelatedRequest,
} from '../clients/grpc-types.js';

type JsonObject = Record<string, unknown>;

function str(args: JsonObject, key: string): string | undefined {
  const v = args[key];
  return typeof v === 'string' && v.trim().length > 0 ? v : undefined;
}

function num(args: JsonObject, key: string): number | undefined {
  const v = args[key];
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined;
}

/**
 * Extract and validate `minConfidence` (shared by relations/impact/usages).
 * Confidence is a best-path edge-weight product in [0,1] — NOT a percentage; a
 * threshold above 1.0 would silently filter out every node (all confidences are
 * <= 1.0), indistinguishable from "no relations exist", so out-of-range values
 * are rejected loudly here before any daemon call.
 */
function minConfidenceArg(args: JsonObject): number | undefined {
  const v = num(args, 'minConfidence');
  if (v !== undefined && (v < 0 || v > 1)) {
    throw new Error(
      `\`minConfidence\` must be within [0, 1], got ${v} — confidence is a best-path ` +
        'edge-weight product (e.g. 0.5), not a percentage.'
    );
  }
  return v;
}

function strArray(args: JsonObject, key: string): string[] | undefined {
  const v = args[key];
  if (Array.isArray(v)) {
    const out = v.filter((x): x is string => typeof x === 'string');
    return out.length > 0 ? out : undefined;
  }
  return undefined;
}

/**
 * Default edge types for `relations` — every dependency edge EXCEPT `CONTAINS`.
 * Traversing CONTAINS from a class/struct returns its own members, so an
 * unfiltered `relations` on a large class is an internal MEMBER INVENTORY, not a
 * dependency map. Excluding CONTAINS by default makes `relations` answer "what
 * does this symbol depend on" (calls / type uses / imports / inheritance). Pass
 * `edgeTypes` explicitly (e.g. `["CONTAINS"]`) to override — the membership
 * escape hatch.
 */
const RELATION_DEPENDENCY_EDGES = ['CALLS', 'IMPORTS', 'USES_TYPE', 'EXTENDS', 'IMPLEMENTS'];

/**
 * SHA256(tenant_id|file_path|symbol_name|symbol_type)[..32 hex chars].
 * Must match Rust's `compute_node_id` so QueryRelated finds the node.
 */
function computeNodeId(
  tenantId: string,
  filePath: string,
  symbolName: string,
  symbolType: string
): string {
  return createHash('sha256')
    .update(`${tenantId}|${filePath}|${symbolName}|${symbolType}`)
    .digest('hex')
    .slice(0, 32);
}

async function resolveTenant(args: JsonObject, projectDetector: ProjectDetector): Promise<string> {
  const explicit = str(args, 'projectId') ?? str(args, 'tenantId');
  if (explicit) return explicit;
  // Resolve the caller's cwd to its project exactly like `search`/`grep`/`list`
  // (`getEffectiveCwd()` honours the `cwd` arg / X-MCP-Host-Cwd header). This is
  // what keeps `graph` on the same project as the rest of the tools.
  // `fallbackToSoleProject` covers the single-project convenience case.
  const detected = await resolveProjectIdentity(projectDetector, undefined);
  if (detected.projectId) return detected.projectId;
  // Deliberately NO "first active project" fallback: with multiple projects and
  // an unresolvable cwd it picked an arbitrary (wrong) project and returned its
  // graph silently. Fail loudly instead.
  throw new Error(
    'Could not resolve a project for `graph`. Pass `projectId` (the tenant_id), ' +
      'or pass `cwd` (your absolute working directory) so the project can be ' +
      'auto-detected. (graph no longer guesses the first active project.)'
  );
}

export async function handleGraph(
  rawArgs: Record<string, unknown> | undefined,
  daemonClient: DaemonClient | undefined,
  projectDetector: ProjectDetector
): Promise<unknown> {
  if (!daemonClient) {
    throw new Error('graph requires a connected daemon client (gRPC unavailable)');
  }
  const args = rawArgs ?? {};
  const action = str(args, 'action') ?? 'stats';
  const tenant = await resolveTenant(args, projectDetector);
  const edgeTypes = strArray(args, 'edgeTypes');

  switch (action) {
    case 'stats': {
      const r = await daemonClient.getGraphStats({ tenant_id: tenant });
      return { success: true, action, tenant_id: tenant, ...r };
    }

    case 'impact':
    case 'usages': {
      // Both wrap ImpactAnalysis (reverse reachability over the graph), but differ
      // in DEPTH:
      //   impact → transitive blast-radius (depth<3): all that breaks if you
      //            change X, direct AND indirect.
      //   usages → DIRECT references only (distance===1): "who references X" (the
      //            IDE find-references), filtered from the same response below.
      // Precision improves once the LSP call-hierarchy pass resolves CALLS edges.
      const symbol = str(args, 'symbol');
      if (!symbol) throw new Error(`graph action '${action}' requires \`symbol\``);
      const filePath = str(args, 'filePath');
      // Precision filter: drop nodes below this best-path confidence at the
      // daemon (before top_k + total_impacted). Omitted = all. See tool desc.
      const minConfidence = minConfidenceArg(args);
      // Bound the impacted-node list: the daemon caps to top_k (nearest-by-depth
      // first) and still returns the true total_impacted. topK<=0 = all.
      const req: ImpactAnalysisRequest = {
        tenant_id: tenant,
        symbol_name: symbol,
        top_k: num(args, 'topK') ?? 50,
        ...(filePath ? { file_path: filePath } : {}),
        ...(minConfidence !== undefined ? { min_confidence: minConfidence } : {}),
      };
      const r = await daemonClient.impactAnalysis(req);
      if (action === 'usages') {
        // Keep only direct references (1-hop). The daemon tags each node's
        // distance; distance===1 is a direct caller / reference / type-use. This
        // is what makes `usages` distinct from the transitive `impact`.
        // total_impacted becomes the direct-reference count.
        const direct = (r.impacted_nodes ?? []).filter((n) => n.distance === 1);
        return {
          success: true,
          action,
          tenant_id: tenant,
          symbol,
          ...r,
          impacted_nodes: direct,
          total_impacted: direct.length,
        };
      }
      return { success: true, action, tenant_id: tenant, symbol, ...r };
    }

    case 'hotspots': {
      const req: PageRankRequest = {
        tenant_id: tenant,
        top_k: num(args, 'topK') ?? 20,
        ...(edgeTypes ? { edge_types: edgeTypes } : {}),
      };
      const r = await daemonClient.computePageRank(req);
      return { success: true, action, tenant_id: tenant, ...r };
    }

    case 'bridges': {
      // Betweenness centrality — symbols that sit on many shortest paths
      // ("bridges"/bottlenecks connecting otherwise-separate clusters).
      const maxSamples = num(args, 'maxSamples');
      const req: BetweennessRequest = {
        tenant_id: tenant,
        top_k: num(args, 'topK') ?? 20,
        ...(maxSamples !== undefined ? { max_samples: maxSamples } : {}),
        ...(edgeTypes ? { edge_types: edgeTypes } : {}),
      };
      const r = await daemonClient.computeBetweenness(req);
      return { success: true, action, tenant_id: tenant, ...r };
    }

    case 'cycles': {
      // Dependency cycles (Tarjan SCC): circular CALLS/IMPORTS between symbols.
      // cross_file cycles are returned FIRST (layering smells worth flagging);
      // same-file cycles are usually benign mutual recursion. `minSize` maps to
      // the minimum SCC size (daemon default 2, which skips self-loops).
      const minSize = num(args, 'minSize');
      const req: CycleRequest = {
        tenant_id: tenant,
        top_k: num(args, 'topK') ?? 20,
        ...(minSize !== undefined ? { min_cycle_size: minSize } : {}),
        ...(edgeTypes ? { edge_types: edgeTypes } : {}),
      };
      const r = await daemonClient.detectCycles(req);
      return { success: true, action, tenant_id: tenant, ...r };
    }

    case 'test_gaps': {
      // Production symbols no test reaches over the call graph (test → CALLS /
      // USES_TYPE → production). `gaps` are ranked by production_dependents
      // (most-relied-on untested code first); `topK` bounds the list while
      // `gap_count`/`covered`/`total_production` stay exact. NOTE: this is
      // call-graph REACHABILITY from test code — an approximation of coverage,
      // NOT execution coverage; it complements, not replaces, coverage tools.
      const req: TestGapsRequest = {
        tenant_id: tenant,
        top_k: num(args, 'topK') ?? 20,
        ...(edgeTypes ? { edge_types: edgeTypes } : {}),
      };
      const r = await daemonClient.detectTestGaps(req);
      return { success: true, action, tenant_id: tenant, ...r };
    }

    case 'modules': {
      const minSize = num(args, 'minSize');
      // Member sample per community. `top_k` bounds the community COUNT, but the
      // largest communities each hold thousands of members — a top-20 dump is
      // still ~1.5M chars and overflows the response. The daemon now caps members
      // to `member_limit` at the SOURCE (so the gRPC message is bounded too, not
      // just this response) and reports each cluster's true `member_count`.
      // memberLimit<=0 means "all members" (escape hatch) — sent as 0, which the
      // daemon treats as no cap.
      const memberLimitRaw = num(args, 'memberLimit');
      const memberLimit = memberLimitRaw === undefined ? 10 : memberLimitRaw;
      const memberLimitWire = memberLimit > 0 ? Math.floor(memberLimit) : 0;
      const req: CommunityRequest = {
        tenant_id: tenant,
        top_k: num(args, 'topK') ?? 20,
        member_limit: memberLimitWire,
        ...(minSize !== undefined ? { min_community_size: minSize } : {}),
        ...(edgeTypes ? { edge_types: edgeTypes } : {}),
      };
      const r = await daemonClient.detectCommunities(req);
      const communities = (r.communities ?? []).map((c) => {
        const members = c.members ?? [];
        return {
          community_id: c.community_id,
          // Daemon caps members and reports the true size in member_count; fall
          // back to the received length if an older daemon omits it. The slice is
          // then a defensive no-op (members already <= memberLimit).
          member_count: c.member_count ?? members.length,
          members: memberLimit > 0 ? members.slice(0, memberLimit) : members,
        };
      });
      return {
        success: true,
        action,
        tenant_id: tenant,
        total_communities: r.total_communities,
        query_time_ms: r.query_time_ms,
        member_limit: memberLimit,
        communities,
      };
    }

    case 'relations': {
      const symbol = str(args, 'symbol');
      const filePath = str(args, 'filePath');
      if (!symbol || !filePath) {
        throw new Error(
          "graph action 'relations' requires `symbol` and `filePath` " +
            "(plus optional `symbolType`, default 'function'). Get these from a `search` result's metadata."
        );
      }
      const symbolType = str(args, 'symbolType') ?? 'function';
      const nodeId = computeNodeId(tenant, filePath, symbol, symbolType);
      const minConfidence = minConfidenceArg(args);
      const req: QueryRelatedRequest = {
        tenant_id: tenant,
        node_id: nodeId,
        max_hops: num(args, 'maxHops') ?? 1,
        // Daemon caps the traversal list to top_k (nearest-by-depth first) and
        // returns the true total. topK<=0 = all.
        top_k: num(args, 'topK') ?? 50,
        // Precision filter: drop nodes below this best-path confidence at the
        // daemon (before top_k + total). Omitted = all. See tool desc.
        ...(minConfidence !== undefined ? { min_confidence: minConfidence } : {}),
        // Default to dependency edges (exclude CONTAINS membership) so relations
        // is a DEPENDENCY MAP, not an internal member inventory. Explicit
        // `edgeTypes` (e.g. ["CONTAINS"]) overrides — the membership escape hatch.
        edge_types: edgeTypes ?? RELATION_DEPENDENCY_EDGES,
        // Fallback identity: the daemon resolves the node by NAME if the
        // computed node_id misses (the symbolType/filePath must otherwise match
        // the extractor EXACTLY — e.g. an async fn is "async_function"). Without
        // this, a wrong symbolType silently returned 0.
        symbol_name: symbol,
        file_path: filePath,
      };
      const r = await daemonClient.queryRelated(req);
      return { success: true, action, tenant_id: tenant, symbol, node_id: nodeId, ...r };
    }

    default:
      throw new Error(
        `Unknown graph action: '${action}'. Use one of: stats, relations, impact, usages, hotspots, bridges, modules, cycles.`
      );
  }
}
