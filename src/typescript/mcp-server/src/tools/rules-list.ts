/**
 * Rules list operation — query rules by scope from Qdrant with mirror fallback.
 */

import type { QdrantClient } from '@qdrant/js-client-rest';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import type { RuleOptions, RuleResponse, Rule, RuleScope } from './rules-types.js';
import { RULES_COLLECTION } from './rules-types.js';
import { FIELD_PROJECT_ID, FIELD_CONTENT, FIELD_TITLE } from '../common/native-bridge.js';
import { TENANT_GLOBAL } from '../constants/tenants.js';
import { resolveProjectIdentity } from './branch-scope.js';
import { applyByteBudget } from './response-budget.js';

/** Preview length (chars) for summary-mode list entries. */
const RULES_LIST_PREVIEW_CHARS = 200;

/** Build Qdrant filter for list query based on scope. */
function buildListFilter(
  scope: RuleScope,
  projectId?: string
): Record<string, unknown> | undefined {
  const mustConditions: Record<string, unknown>[] = [];

  if (scope === TENANT_GLOBAL) {
    mustConditions.push({ key: 'scope', match: { value: TENANT_GLOBAL } });
  } else if (scope === 'project' && projectId) {
    mustConditions.push({ key: 'scope', match: { value: 'project' } });
    mustConditions.push({ key: FIELD_PROJECT_ID, match: { value: projectId } });
  }

  return mustConditions.length > 0 ? { must: mustConditions } : undefined;
}

/** Map a Qdrant point payload to a Rule object. */
function pointToRule(point: {
  id: string | number;
  payload?: Record<string, unknown> | null;
}): Rule {
  const rule: Rule = {
    id: String(point.id),
    content: (point.payload?.[FIELD_CONTENT] as string) ?? '',
    scope: (point.payload?.['scope'] as RuleScope) ?? TENANT_GLOBAL,
  };
  const label = point.payload?.['label'] as string | undefined;
  if (label) rule.label = label;
  const pid = point.payload?.[FIELD_PROJECT_ID] as string | undefined;
  if (pid) rule.projectId = pid;
  // Always surface the owner so a multi-tenant listing is unambiguous:
  // project rules → owning tenant_id; global rules → "global".
  rule.owner = rule.scope === 'project' ? (pid ?? 'unknown-project') : TENANT_GLOBAL;
  const title = point.payload?.[FIELD_TITLE] as string | undefined;
  if (title) rule.title = title;
  const tagsStr = point.payload?.['tags'] as string | undefined;
  if (tagsStr) rule.tags = tagsStr.split(',');
  const priorityRaw = point.payload?.['priority'];
  if (priorityRaw !== undefined && priorityRaw !== null) rule.priority = Number(priorityRaw);
  const createdAt = point.payload?.['created_at'] as string | undefined;
  if (createdAt) rule.createdAt = createdAt;
  const updatedAt = point.payload?.['updated_at'] as string | undefined;
  if (updatedAt) rule.updatedAt = updatedAt;
  return rule;
}

/** Build a scroll request for the rules collection. */
function buildScrollRequest(
  limit: number,
  filter: Record<string, unknown> | undefined,
  cursor?: string
): { limit: number; with_payload: boolean; filter?: Record<string, unknown>; offset?: string } {
  const req: {
    limit: number;
    with_payload: boolean;
    filter?: Record<string, unknown>;
    offset?: string;
  } = {
    limit,
    with_payload: true,
  };
  if (filter) req.filter = filter;
  if (cursor) req.offset = cursor;
  return req;
}

/** Attempt to read rules from the local mirror as fallback. Returns the raw
 *  rules (full content) or null; the caller applies list shaping so the mirror
 *  path is bounded identically to the Qdrant path. */
function readRulesFromMirror(
  stateManager: SqliteStateManager,
  scope: RuleScope,
  resolvedProjectId: string | undefined,
  limit: number
): Rule[] | null {
  try {
    const mirrorRows = stateManager.listRulesMirror(scope, resolvedProjectId, limit);
    if (mirrorRows.length === 0) return null;
    return mirrorRows.map((row) => {
      const scope = (row.scope as RuleScope) ?? TENANT_GLOBAL;
      const rule: Rule = {
        id: row.ruleId,
        content: row.ruleText,
        scope,
        owner: scope === 'project' ? (row.tenantId ?? 'unknown-project') : TENANT_GLOBAL,
        createdAt: row.createdAt,
        updatedAt: row.updatedAt,
      };
      if (row.tenantId) rule.projectId = row.tenantId;
      return rule;
    });
  } catch {
    return null;
  }
}

/**
 * Cold-start guard: a freshly (re)started MCP container pays connection warmup
 * on its FIRST Qdrant call, which can exceed the MCP client's request timeout
 * (surfacing as -32001) even though a retry — hitting a warm connection —
 * succeeds. Bound the scroll so a slow cold call degrades to the local mirror
 * fast instead of hanging. Warm scrolls of the small rules collection finish in
 * milliseconds, so this deadline is never hit on the normal path.
 */
const RULES_SCROLL_DEADLINE_MS = 2500;

/**
 * Resolve `p`, or `null` if it has not settled within `ms`. A rejection that
 * arrives AFTER the deadline is swallowed so it does not surface as an unhandled
 * rejection; a rejection BEFORE the deadline still rejects the race, so the
 * caller's catch (its existing error→mirror path) handles a genuine failure.
 */
function withDeadline<T>(p: Promise<T>, ms: number): Promise<T | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), ms);
  });
  p.catch(() => undefined);
  return Promise.race([p, deadline]).finally(() => {
    if (timer) clearTimeout(timer);
  });
}

/** Project a full Rule to its summary form (preview + content_length, no body). */
function shapeRuleForList(rule: Rule, summary: boolean): Rule {
  if (!summary) return rule;
  const content = rule.content ?? '';
  const shaped: Rule = {
    id: rule.id,
    scope: rule.scope,
    preview: content.slice(0, RULES_LIST_PREVIEW_CHARS),
    content_length: content.length,
  };
  if (rule.label) shaped.label = rule.label;
  if (rule.projectId) shaped.projectId = rule.projectId;
  if (rule.owner) shaped.owner = rule.owner;
  if (rule.title) shaped.title = rule.title;
  if (rule.tags) shaped.tags = rule.tags;
  if (rule.priority !== undefined) shaped.priority = rule.priority;
  if (rule.createdAt) shaped.createdAt = rule.createdAt;
  if (rule.updatedAt) shaped.updatedAt = rule.updatedAt;
  return shaped;
}

/**
 * Shape a raw rules listing like every other read surface (scratchpad/search/
 * grep): optional summary projection + the shared response byte budget + cursor
 * pagination. Shaping is OFF by default (summary=false, budget=0) so internal
 * callers — agent-rules' system-prompt injection, the seeder's content dedup —
 * keep full, untruncated content; the MCP tool surface (buildRuleOptions) turns
 * it on. The caller sets `message`.
 */
function buildListResponse(
  rules: Rule[],
  options: RuleOptions,
  nextPageOffset?: unknown
): RuleResponse {
  const summary = options.summary ?? false;
  const budget = options.maxResponseBytes ?? 0;
  const shaped = rules.map((r) => shapeRuleForList(r, summary));
  // Shared response budget (same semantics as search/grep/scratchpad): trailing
  // rules are dropped (>=1 kept). Qdrant scroll offsets are inclusive point ids,
  // so resuming at the first dropped rule loses nothing.
  const { kept, dropped } = applyByteBudget(shaped, (r) => JSON.stringify(r).length, budget);
  const response: RuleResponse = {
    success: true,
    action: 'list',
    rules: kept,
    count: kept.length,
  };
  const firstDropped = shaped[kept.length];
  if (dropped > 0 && firstDropped) {
    response.budget_truncated = { dropped };
    response.next_cursor = firstDropped.id;
  } else if (nextPageOffset !== null && nextPageOffset !== undefined) {
    response.next_cursor = String(nextPageOffset);
  }
  if (summary) {
    response.hint =
      'Rules are summaries (preview + content_length). For a rule\'s full text pass ' +
      'summary:false, or read one by id with retrieve (collection:"rules", documentId:<id>).';
  }
  return response;
}

/** Best-effort total rule count for the scope (omitted on any failure). */
async function countRules(
  qdrantClient: QdrantClient,
  filter: Record<string, unknown> | undefined
): Promise<number | undefined> {
  try {
    const res = await qdrantClient.count(RULES_COLLECTION, {
      ...(filter ? { filter } : {}),
      exact: true,
    });
    return res.count;
  } catch {
    return undefined;
  }
}

/** List rules by scope from Qdrant, with rules_mirror fallback. */
export async function listRules(
  qdrantClient: QdrantClient,
  stateManager: SqliteStateManager,
  projectDetector: ProjectDetector,
  options: RuleOptions
): Promise<RuleResponse> {
  const { scope = 'project', projectId, limit = 50 } = options;

  let resolvedProjectId = projectId;
  if (scope === 'project' && !resolvedProjectId) {
    resolvedProjectId = (await resolveProjectIdentity(projectDetector, undefined)).projectId;
  }

  // When a project-scoped list can't resolve a tenant, buildListFilter yields
  // no filter and the scroll spans every project's rules. Surface that so the
  // agent knows the listing is multi-tenant and reads each rule's `owner`.
  const unresolvedProjectScope = scope === 'project' && !resolvedProjectId;

  // Shape the mirror fallback identically to the Qdrant path so a degraded
  // response is bounded too (never a raw full-content dump).
  const mirrorResponse = (): RuleResponse | null => {
    const mirrorRules = readRulesFromMirror(stateManager, scope, resolvedProjectId, limit);
    if (!mirrorRules) return null;
    const response = buildListResponse(mirrorRules, options);
    response.message = `Found ${response.count} rule(s) from local mirror (Qdrant unavailable)`;
    return response;
  };

  try {
    const filter = buildListFilter(scope, resolvedProjectId);
    const scrollResult = await withDeadline(
      qdrantClient.scroll(RULES_COLLECTION, buildScrollRequest(limit, filter, options.cursor)),
      RULES_SCROLL_DEADLINE_MS
    );
    if (scrollResult) {
      const rules: Rule[] = scrollResult.points.map(pointToRule);
      const response = buildListResponse(rules, options, scrollResult.next_page_offset);
      response.message = unresolvedProjectScope
        ? `Found ${response.count} rule(s) across ALL projects — the current project could not be detected, so this listing is not scoped. Each rule's "owner" field identifies its project (or "global"). Pass cwd or projectId to scope to one project.`
        : `Found ${response.count} rule(s)`;
      const total = await countRules(qdrantClient, filter);
      if (total !== undefined) response.total = total;
      return response;
    }
    // Deadline hit — a slow COLD Qdrant scroll. Serve the local mirror fast so
    // the caller gets its rules instead of an MCP client timeout (-32001).
    const mirror = mirrorResponse();
    if (mirror) return mirror;
    return {
      success: false,
      action: 'list',
      rules: [],
      message:
        'Rules backend (Qdrant) was slow to respond (cold start) and no local mirror was available; retry shortly.',
    };
  } catch (error) {
    const mirror = mirrorResponse();
    if (mirror) return mirror;
    return {
      success: false,
      action: 'list',
      rules: [],
      message: `Failed to list rules: ${error instanceof Error ? error.message : 'unknown error'}`,
    };
  }
}
