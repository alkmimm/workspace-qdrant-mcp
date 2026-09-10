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
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import { resolveProjectIdentity, type ProjectIdentity } from './branch-scope.js';
import { projectEcho } from './project-echo.js';
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

async function resolveTenantIdentity(
  args: JsonObject,
  projectDetector: ProjectDetector,
  stateManager: SqliteStateManager | undefined
): Promise<ProjectIdentity & { projectId: string }> {
  const explicit = str(args, 'projectId') ?? str(args, 'tenantId');
  // The SAME shared resolver search/grep/list/retrieve use: an explicit id is
  // completed with its registered path (so the echo carries project_path here
  // too); otherwise the caller's cwd resolves (`getEffectiveCwd()` honours the
  // `cwd` arg / X-MCP-Host-Cwd header) with the sole-project convenience
  // fallback. This is what keeps `graph` on the same project as the other tools.
  const detected = await resolveProjectIdentity(projectDetector, explicit, true, stateManager);
  if (detected.projectId) {
    return { ...detected, projectId: detected.projectId };
  }
  // Deliberately NO "first active project" fallback: with multiple projects and
  // an unresolvable cwd it picked an arbitrary (wrong) project and returned its
  // graph silently. Fail loudly instead.
  throw new Error(
    'Could not resolve a project for `graph`. Pass `projectId` (the tenant_id), ' +
      'or pass `cwd` (your absolute working directory) so the project can be ' +
      'auto-detected. (graph no longer guesses the first active project.)'
  );
}

/**
 * Caveat for an EMPTY `usages`/`impact`/`relations` result. The graph is built
 * from edges extracted at ingest — CALLS, USES_TYPE, IMPORTS, inheritance — over
 * CALLABLE symbols, so a 0 means "no such edge was extracted", NOT "unused".
 * Field feedback (multiple worktree sessions): `usages`/`impact` on a Dart class
 * field / Riverpod provider returned 0 while a plain grep found 17 real sites —
 * the graph does not index field/property access, dynamic dispatch, string-keyed
 * DI, or generated-code references. Leading a caller to "nobody uses X" from a
 * graph 0 is the exact failure this prevents.
 */
function graphNoEdgesHint(kind: 'usages' | 'impact' | 'relations'): string {
  const what =
    kind === 'relations'
      ? 'no outgoing dependency edges'
      : kind === 'usages'
        ? 'no direct references'
        : 'no dependents';
  return (
    `Graph shows ${what} here, but it only has CALL / USES_TYPE / IMPORTS / inheritance edges over ` +
    `callable symbols. Field/property/provider access, dynamic dispatch, string-keyed DI, and ` +
    `generated-code references are NOT edges — this 0 does NOT prove the symbol is unused. Confirm ` +
    `with the grep tool before concluding "no callers/usages".`
  );
}

/**
 * Strip proto-loader's synthetic oneof markers from a decoded daemon response.
 *
 * The client loads the proto with `oneofs: true` (connection.ts), so every
 * proto3 `optional` field arrives with a sibling `_<field>` whose value is the
 * field's own name — e.g. `reliability_warning` comes with
 * `_reliability_warning: "reliability_warning"`. That is decoder bookkeeping,
 * not data, and the graph actions spread daemon responses wholesale, so it
 * lands straight in the agent's context.
 *
 * Applied once at the tool boundary rather than at the one action that has an
 * `optional` field today: any action that gains one later is covered, and the
 * recursion catches nested messages too.
 */
function stripOneofMarkers<T>(value: T): T {
  if (Array.isArray(value)) {
    return value.map((v) => stripOneofMarkers(v)) as unknown as T;
  }
  if (value === null || typeof value !== 'object') return value;
  const out: Record<string, unknown> = {};
  for (const [key, v] of Object.entries(value as Record<string, unknown>)) {
    // A marker is `_<field>` whose value is exactly `<field>`; a real payload
    // key starting with `_` (e.g. search's `_search_type`) never matches that.
    if (key.startsWith('_') && v === key.slice(1)) continue;
    out[key] = stripOneofMarkers(v);
  }
  return out as T;
}

/**
 * How many times the symbol appears in the text index, or `undefined` when the
 * probe could not run.
 *
 * Called only on the `usages` zero-result path, which by definition has nothing
 * to show — so one extra query buys the difference between "no edge models this"
 * and "nothing references this". `textSearchCount` returns a count without
 * transferring any match bodies.
 *
 * Every failure mode is swallowed on purpose. This is a courtesy on an already
 * empty answer; it must never turn a valid empty result into an error.
 */
async function countTextOccurrences(
  daemonClient: DaemonClient | undefined,
  symbol: string,
  tenantId: string | undefined
): Promise<number | undefined> {
  if (!daemonClient) return undefined;
  try {
    const response = await daemonClient.textSearchCount({
      pattern: symbol,
      regex: false,
      case_sensitive: true,
      context_lines: 0,
      max_results: 1,
      ...(tenantId ? { tenant_id: tenantId } : {}),
    });
    return response?.count;
  } catch {
    return undefined;
  }
}

export async function handleGraph(
  rawArgs: Record<string, unknown> | undefined,
  daemonClient: DaemonClient | undefined,
  projectDetector: ProjectDetector,
  stateManager?: SqliteStateManager
): Promise<unknown> {
  return stripOneofMarkers(
    await runGraphAction(rawArgs, daemonClient, projectDetector, stateManager)
  );
}

async function runGraphAction(
  rawArgs: Record<string, unknown> | undefined,
  daemonClient: DaemonClient | undefined,
  projectDetector: ProjectDetector,
  stateManager: SqliteStateManager | undefined
): Promise<unknown> {
  if (!daemonClient) {
    throw new Error('graph requires a connected daemon client (gRPC unavailable)');
  }
  const args = rawArgs ?? {};
  const action = str(args, 'action') ?? 'stats';
  const identity = await resolveTenantIdentity(args, projectDetector, stateManager);
  const edgeTypes = strArray(args, 'edgeTypes');
  const result = await dispatchGraphAction(
    action,
    args,
    identity.projectId,
    edgeTypes,
    daemonClient
  );
  // Read-side project echo (shared with search/grep/list/retrieve): which
  // tenant the graph answered for and how it was resolved.
  const explicit = str(args, 'projectId') ?? str(args, 'tenantId');
  return { ...result, ...projectEcho(identity, explicit) };
}

async function dispatchGraphAction(
  action: string,
  args: JsonObject,
  tenant: string,
  edgeTypes: ReturnType<typeof strArray>,
  daemonClient: DaemonClient
): Promise<Record<string, unknown>> {
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
      const topK = num(args, 'topK') ?? 50;
      const req: ImpactAnalysisRequest = {
        tenant_id: tenant,
        symbol_name: symbol,
        top_k: topK,
        ...(filePath ? { file_path: filePath } : {}),
        ...(minConfidence !== undefined ? { min_confidence: minConfidence } : {}),
      };
      const r = await daemonClient.impactAnalysis(req);
      // The daemon caps at top_k AFTER ordering nearest-first, so a page that came
      // back full may have cut nodes that did not fit. Only an unsaturated page
      // proves the list is complete. Presenting a capped page as the whole answer
      // is what let `usages` return a different arbitrary subset on every call and
      // still read as authoritative (issue #367).
      const returnedCount = (r.impacted_nodes ?? []).length;
      const truncated = topK > 0 && returnedCount >= topK;
      const raiseTopK =
        `Truncated at topK=${topK}: more nodes exist beyond the cap. Re-run with a ` +
        `larger topK (topK:0 removes the cap) before treating this list as complete.`;
      if (action === 'usages') {
        // Keep only direct references (1-hop). The daemon tags each node's
        // distance; distance===1 is a direct caller / reference / type-use. This
        // is what makes `usages` distinct from the transitive `impact`.
        const direct = (r.impacted_nodes ?? []).filter((n) => n.distance === 1);
        // Both caveats can hold at once — a full page of distance>1 nodes yields
        // zero direct references AND hides the rest — so collect them instead of
        // letting one `hint` key silently overwrite the other.
        const hints: string[] = [];
        // A threshold that removed nothing produces a response byte-identical to
        // omitting it, so the caller cannot tell "my filter ran and kept
        // everything" from "my filter did nothing" (#383). On a real result the
        // confidences cluster at 1.00 / 0.95 / 0.85, which makes 0.5 — the
        // natural "medium confidence" choice — a no-op. Say so, and name where
        // the cut points actually are.
        if (minConfidence !== undefined && (r.filtered_by_min_confidence ?? 0) === 0) {
          hints.push(
            `minConfidence:${minConfidence} removed nothing — this result is identical to ` +
              `omitting it. Reported confidences are the best-path edge-weight product and ` +
              `in practice cluster at 1.00 (precise), 0.95, 0.85, with 0.7 for a ` +
              `tenant-unique name; a same-name fan-out scores ~1/N. Useful cut points start ` +
              `around 0.85, not 0.5.`
          );
        }
        // `usages` needs its OWN wording. The cap applies to the TRANSITIVE page
        // the daemon returns, and only afterwards does the distance===1 filter
        // run here — so a reader sees a count well below topK next to
        // `truncated: true` and reasonably concludes the flag is wrong. Field
        // report: 22 nodes returned with topK 40, flagged truncated, no
        // explanation of the gap. Name both numbers so the arithmetic is visible.
        if (truncated) {
          hints.push(
            `Truncated: the daemon returned the first ${topK} impacted nodes across ALL ` +
              `depths (nearest first), and ${direct.length} of those are DIRECT references. ` +
              `Further direct references may sit beyond that cap — re-run with a larger ` +
              `topK (topK:0 removes it) before treating this list as complete.`
          );
        }
        // A zero here is the trap this tool is repeatedly reported for. The
        // graph only models CALLS / USES_TYPE / IMPORTS, so an idiom that
        // REFERENCES a symbol without invoking it produces no edge at all:
        // `ref.watch(someProvider)` passes the symbol as an argument, and
        // Flutter's `find.byType(Widget)` asserts on a type without
        // constructing it. Measured on DOC-V2: `activeContextProvider` existed
        // as a node with ZERO incoming edges against 71 references on disk, and
        // 505 of 506 Dart constants were in the same position.
        //
        // Telling the caller "0" and leaving them to run grep is how "not used"
        // gets concluded from "not modelled". Counting the text index here costs
        // one extra query on a path that already has nothing to show, and turns
        // an ambiguous zero into a specific statement.
        let textOccurrences: number | undefined;
        if (direct.length === 0) {
          textOccurrences = await countTextOccurrences(daemonClient, symbol, tenant);
          hints.push(graphNoEdgesHint('usages'));
          if (textOccurrences !== undefined && textOccurrences > 0) {
            hints.push(
              `The text index holds ${textOccurrences} occurrence(s) of "${symbol}", so this 0 ` +
                `means NOT MODELLED rather than unused. A symbol passed by reference is modelled ` +
                `for Dart lower-camel identifiers (REFERENCES edges), but NOT for other ` +
                `languages, and not for type-shaped names such as find.byType(X) in any ` +
                `language. Existing data also needs a graph rebuild before those edges appear. ` +
                `Use grep for the sites.`
            );
          }
        }
        return {
          success: true,
          action,
          tenant_id: tenant,
          symbol,
          ...r,
          impacted_nodes: direct,
          // The count of what is actually returned. When the page was truncated
          // this is a FLOOR, not a total; `truncated` says so rather than letting
          // the number pass for a complete answer.
          total_impacted: direct.length,
          truncated,
          ...(truncated ? { total_impacted_all_depths: r.total_impacted } : {}),
          ...(textOccurrences !== undefined ? { text_occurrences: textOccurrences } : {}),
          ...(hints.length > 0 ? { hint: hints.join(' ') } : {}),
        };
      }
      const impactHints: string[] = [];
      if (truncated) impactHints.push(raiseTopK);
      if ((r.total_impacted ?? returnedCount) === 0) {
        impactHints.push(graphNoEdgesHint('impact'));
      }
      return {
        success: true,
        action,
        tenant_id: tenant,
        symbol,
        ...r,
        truncated,
        ...(impactHints.length > 0 ? { hint: impactHints.join(' ') } : {}),
      };
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
      // A node the ubiquity filter removed cannot appear in ANY cycle, so a
      // short list is not by itself evidence of a clean codebase. Say what was
      // dropped, and lead with it — same channel and same ordering as the
      // test_gaps reliability warning, for the same reason: an agent reading
      // top-down must see "this was filtered" before the list it would act on.
      // `?? 0` is not redundant against the type: a daemon deployed before this
      // field existed omits it on the wire, and NaN in a hint reads as a bug.
      const suppressed = r.suppressed_ubiquitous ?? 0;
      const hint =
        suppressed > 0
          ? `${suppressed} symbol(s) were excluded before cycle detection because too many callers resolved to them — the shape of a name that collides with a language SDK method (List.add, Iterable.map), which the confidence gate cannot catch because a tenant-unique name scores 0.7. They cannot appear in any cycle below. Genuine high-traffic utilities are dropped by the same rule.`
          : undefined;
      return {
        success: true,
        action,
        tenant_id: tenant,
        ...(hint !== undefined ? { hint } : {}),
        ...r,
      };
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
      // The daemon flags an implausible coverage ratio (tests indexed, yet
      // almost nothing reachable = unresolved test→production edges, not
      // untested code). Surface it as `hint` — same channel the empty
      // usages/impact/relations caveat uses — and place it BEFORE `gaps` so an
      // agent reading top-down sees "this is noise" ahead of the ranking it
      // would otherwise act on. Field feedback: a 0.6% report was taken as a
      // finding and its top entries were demonstrably tested functions.
      const { reliability_warning: warning, ...rest } = r;
      return {
        success: true,
        action,
        tenant_id: tenant,
        ...(warning !== undefined && warning !== '' ? { hint: warning } : {}),
        ...rest,
      };
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
      return {
        success: true,
        action,
        tenant_id: tenant,
        symbol,
        node_id: nodeId,
        ...r,
        ...((r.total ?? (r.nodes ?? []).length) === 0
          ? { hint: graphNoEdgesHint('relations') }
          : {}),
      };
    }

    default:
      throw new Error(
        `Unknown graph action: '${action}'. Use one of: stats, relations, impact, usages, hotspots, bridges, modules, cycles.`
      );
  }
}
