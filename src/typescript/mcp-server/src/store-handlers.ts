/**
 * Store handler helpers for URL and scratchpad store types
 */

import { randomUUID } from 'node:crypto';

import type { SqliteStateManager } from './clients/sqlite-state-manager.js';
import type { ProjectDetector } from './utils/project-detector.js';
import type { SessionState } from './server-types.js';
import { COLLECTION_SCRATCHPAD, PRIORITY_HIGH } from './common/native-bridge.js';
import { TENANT_FEEDBACK } from './constants/tenants.js';
import { utcNow } from './utils/timestamps.js';
import { resolveScratchpadOrigin, type ScratchpadOrigin } from './tools/scratchpad-origin.js';
import { resolveScopedTenant, describeScope, type ScopedTenant } from './tools/tenant-scope.js';

type StoreResult = {
  success: boolean;
  message: string;
  queue_id?: string;
  collection: string;
  /**
   * Tenant the write was routed to, and that tenant's registered path when
   * known. Echoed on every project-scoped write so a caller who passed `cwd`
   * can see WHICH project the server resolved rather than trusting it — the
   * failure mode this guards against (a note silently landing in another
   * project) is otherwise indistinguishable from success.
   */
  project_id?: string;
  project_path?: string;
};

/** Attach the resolved-scope echo to a write result. */
function withScopeEcho(result: StoreResult, scoped: ScopedTenant): StoreResult {
  return {
    ...result,
    project_id: scoped.tenantId,
    ...(scoped.projectPath ? { project_path: scoped.projectPath } : {}),
  };
}

/**
 * Pre-enqueue URL validation. Rejects malformed input, non-http(s) schemes,
 * and obviously bad hostnames so the daemon does not waste a queue cycle on
 * URLs it would reject at fetch time. Full SSRF policy (private-network
 * denylist, DNS rebinding defense, redirect re-validation) is enforced
 * daemon-side; this is a fast-fail surface for user-facing error messages.
 */
export function validateUrlInput(raw: unknown): { ok: true } | { ok: false; message: string } {
  if (typeof raw !== 'string' || raw.trim().length === 0) {
    return { ok: false, message: 'url is required when type is "url"' };
  }
  const trimmed = raw.trim();
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return { ok: false, message: 'url is malformed (failed to parse)' };
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    return {
      ok: false,
      message: `url must use http:// or https:// (got ${parsed.protocol})`,
    };
  }
  const host = parsed.hostname;
  if (!host || host.length === 0) {
    return { ok: false, message: 'url has empty hostname' };
  }
  if (/^[.\s]+$/.test(host)) {
    return { ok: false, message: 'url has invalid hostname (dots/whitespace only)' };
  }
  return { ok: true };
}

/** Build the URL queue payload. */
function buildUrlPayload(
  url: string,
  libraryName: string | undefined,
  title: string | undefined
): Record<string, unknown> {
  const payload: Record<string, unknown> = {
    url: url.trim(),
    crawl: false,
    max_depth: 0,
    max_pages: 1,
  };
  if (libraryName) payload['library_name'] = libraryName.trim();
  if (title) payload['title'] = title;
  return payload;
}

/**
 * Store a URL for daemon-side fetch and ingestion.
 *
 * Queues the URL as item_type 'url' in the unified queue.
 * The daemon will fetch the page, extract text, generate embeddings,
 * and store in Qdrant.
 */
export async function storeUrl(
  args: Record<string, unknown> | undefined,
  stateManager: SqliteStateManager,
  projectDetector: ProjectDetector,
  sessionState: Pick<SessionState, 'projectId'>
): Promise<StoreResult> {
  const url = args?.['url'] as string;
  const validation = validateUrlInput(url);
  if (!validation.ok) {
    return { success: false, message: validation.message, collection: '' };
  }

  const libraryName = args?.['libraryName'] as string | undefined;
  const title = args?.['title'] as string | undefined;
  const collection = libraryName ? 'libraries' : COLLECTION_SCRATCHPAD;
  // A named library is its OWN tenant (the libraries collection is keyed by
  // library_name, not by project) — no project resolution applies. Without one
  // the capture is project-scoped and follows the shared write precedence.
  const libraryTenant = libraryName?.trim();
  let scoped: ScopedTenant | undefined;
  if (!libraryTenant) {
    scoped = await resolveScopedTenant({
      explicitProjectId: args?.['projectId'],
      projectDetector,
      sessionProjectId: sessionState.projectId,
      stateManager,
    });
  }
  const tenantId = scoped ? scoped.tenantId : (libraryTenant as string);
  const payload = buildUrlPayload(url, libraryName, title);

  try {
    const result = await stateManager.enqueueUnified(
      'url',
      'add',
      tenantId,
      collection,
      payload,
      PRIORITY_HIGH,
      'main',
      { source: 'mcp_store_url' }
    );
    if (result.status !== 'ok' || !result.data)
      return { success: false, message: result.message ?? 'Failed to enqueue URL', collection };
    const ok: StoreResult = {
      success: true,
      message: `URL queued for fetch and ingestion (${collection}/${scoped ? describeScope(scoped) : tenantId})`,
      queue_id: result.data.queueId,
      collection,
    };
    return scoped ? withScopeEcho(ok, scoped) : ok;
  } catch (error) {
    return {
      success: false,
      message: `Failed to queue URL: ${error instanceof Error ? error.message : String(error)}`,
      collection,
    };
  }
}

/** Write a scratchpad entry to the local mirror (fire-and-forget). */
function writeScratchpadMirror(
  stateManager: SqliteStateManager,
  content: string,
  title: string | undefined,
  tags: string[],
  tenantId: string
): void {
  const now = utcNow();
  stateManager.upsertScratchpadMirror({
    scratchpadId: randomUUID(),
    title: title?.trim() ?? null,
    content: content.trim(),
    tags: JSON.stringify(tags),
    tenantId,
    createdAt: now,
    updatedAt: now,
  });
}

/**
 * Store content to scratchpad collection.
 *
 * After successful enqueue, also writes to scratchpad_mirror for rebuild
 * recovery. The mirror write is fire-and-forget (advisory).
 */
/** Build the scratchpad queue payload. */
function buildScratchpadPayload(
  content: string,
  title: string | undefined,
  tags: string[],
  origin: ScratchpadOrigin
): Record<string, unknown> {
  const payload: Record<string, unknown> = { content: content.trim(), source_type: 'scratchpad' };
  if (title?.trim()) payload['title'] = title.trim();
  if (tags.length > 0) payload['tags'] = tags;
  return { ...payload, ...origin };
}

/**
 * Tenanting a scratchpad note is what makes it REACHABLE: the recall lane and
 * `scratchpad list` both filter strictly by tenant, so a note must carry the
 * right project's tenant_id or it is written and then never seen again.
 *
 * The order lives in {@link resolveScopedTenant}, shared with every other
 * project-scoped write and matched to the read surfaces — explicit `projectId`,
 * then the effective cwd, then the session's activated project, then global.
 */
export async function storeScratchpad(
  args: Record<string, unknown> | undefined,
  stateManager: SqliteStateManager,
  projectDetector: ProjectDetector,
  sessionState: Pick<SessionState, 'projectId' | 'currentBranch' | 'isWorktree'>
): Promise<StoreResult> {
  const content = args?.['content'] as string;
  if (!content?.trim())
    return {
      success: false,
      message: 'content is required when type is "scratchpad"',
      collection: COLLECTION_SCRATCHPAD,
    };

  const title = args?.['title'] as string | undefined;
  const tags = (args?.['tags'] as string[] | undefined) ?? [];
  const scoped = await resolveScopedTenant({
    explicitProjectId: args?.['projectId'],
    projectDetector,
    sessionProjectId: sessionState.projectId,
    stateManager,
  });
  const origin = await resolveScratchpadOrigin({
    explicitBranch: args?.['branch'] as string | undefined,
    sessionState,
    projectDetector,
    // Origin is WHERE THE WRITE CAME FROM: only a cwd-resolved tenant names
    // the writer's own repo. When the note targets another project (explicit
    // projectId / session rung) the writer's location is still the attribution
    // and the detector resolves it from the cwd.
    cwdProjectPath: scoped.source === 'cwd' ? scoped.projectPath : undefined,
  });
  const payload = buildScratchpadPayload(content, title, tags, origin);

  return enqueueScratchpadEntry(stateManager, payload, scoped, content, title, tags);
}

async function enqueueScratchpadEntry(
  stateManager: SqliteStateManager,
  payload: Record<string, unknown>,
  scoped: ScopedTenant,
  content: string,
  title: string | undefined,
  tags: string[]
): Promise<StoreResult> {
  const tenantId = scoped.tenantId;
  try {
    // branch stays "main" by design: the scratchpad point id derives from
    // (tenant, branch, document_id), so a real-branch value would fork a
    // note's identity per branch. Provenance travels in origin_* payload
    // fields instead (see scratchpad-origin.ts).
    const result = await stateManager.enqueueUnified(
      'text',
      'add',
      tenantId,
      COLLECTION_SCRATCHPAD,
      payload,
      PRIORITY_HIGH,
      'main',
      { source: 'mcp_store_scratchpad' }
    );
    if (result.status !== 'ok' || !result.data)
      return {
        success: false,
        message: result.message ?? 'Failed to enqueue scratchpad entry',
        collection: COLLECTION_SCRATCHPAD,
      };

    writeScratchpadMirror(stateManager, content, title, tags, tenantId);
    return withScopeEcho(
      {
        success: true,
        message: `Scratchpad entry queued for processing (${describeScope(scoped)})`,
        queue_id: result.data.queueId,
        collection: COLLECTION_SCRATCHPAD,
      },
      scoped
    );
  } catch (error) {
    return {
      success: false,
      message: `Failed to queue scratchpad entry: ${error instanceof Error ? error.message : String(error)}`,
      collection: COLLECTION_SCRATCHPAD,
    };
  }
}

// ── Feedback (store type:"feedback") ──────────────────────────────────────────

/** Feedback categories — the kind of tool-usage feedback being recorded. */
export const FEEDBACK_CATEGORIES = ['win', 'friction', 'trap', 'missing-rule', 'other'] as const;
export type FeedbackCategory = (typeof FEEDBACK_CATEGORIES)[number];

function isFeedbackCategory(value: unknown): value is FeedbackCategory {
  return typeof value === 'string' && (FEEDBACK_CATEGORIES as readonly string[]).includes(value);
}

/**
 * Store agent feedback ABOUT the workspace-qdrant tooling itself (store
 * type:"feedback").
 *
 * A feedback note is a scratchpad-family entry written to the dedicated synthetic
 * `TENANT_FEEDBACK` bucket, NOT a new collection — so it respects the
 * 4-canonical-collection invariant (ADR-001) while staying isolated from every
 * project-scoped read surface (the recall lane and `scratchpad list` are
 * tenant-strict). `category`/`refTool` are recorded as tags (the daemon drops extra
 * payload fields — see the tags comment below). It reuses the scratchpad provenance stamp
 * (origin_branch/cwd/worktree) so a note records where the friction was hit, and is
 * reviewed/triaged via the `/feedback-review` skill.
 *
 * Unlike scratchpad, feedback ignores `projectId`/`cwd` for tenanting — it ALWAYS
 * aggregates in the one bucket (that is the whole point; scattering it per project
 * defeats the aggregation the review step relies on).
 */
export async function storeFeedback(
  args: Record<string, unknown> | undefined,
  stateManager: SqliteStateManager,
  sessionState: Pick<SessionState, 'projectId' | 'currentBranch' | 'isWorktree'>,
  projectDetector?: ProjectDetector
): Promise<StoreResult> {
  const content = args?.['content'] as string;
  if (!content?.trim())
    return {
      success: false,
      message: 'content is required when type is "feedback" — the feedback itself.',
      collection: COLLECTION_SCRATCHPAD,
    };

  const category = args?.['category'];
  if (!isFeedbackCategory(category))
    return {
      success: false,
      message: `category is required when type is "feedback" — one of: ${FEEDBACK_CATEGORIES.join(', ')}.`,
      collection: COLLECTION_SCRATCHPAD,
    };

  const refTool = (args?.['refTool'] as string | undefined)?.trim();
  const title = args?.['title'] as string | undefined;
  const origin = await resolveScratchpadOrigin({
    explicitBranch: args?.['branch'] as string | undefined,
    sessionState,
    projectDetector,
  });

  // category + refTool are recorded as TAGS (`category:<c>` / `tool:<t>`), NOT as
  // dedicated payload fields: the daemon's scratchpad write (ScratchpadPayload +
  // strategies/processing/text.rs) only persists content/title/tags/provenance and
  // hardcodes source_type="scratchpad", so any extra payload field is silently
  // dropped. The tags survive and are exactly what /feedback-review groups on;
  // isolation is by the dedicated TENANT_FEEDBACK, not a payload discriminator.
  const tags = ['feedback', `category:${category}`];
  if (refTool) tags.push(`tool:${refTool}`);

  const payload: Record<string, unknown> = { content: content.trim(), tags, ...origin };
  if (title?.trim()) payload['title'] = title.trim();

  try {
    // branch stays "main" (like scratchpad): the point id derives from
    // (tenant, branch, document_id); provenance travels in origin_* instead.
    const result = await stateManager.enqueueUnified(
      'text',
      'add',
      TENANT_FEEDBACK,
      COLLECTION_SCRATCHPAD,
      payload,
      PRIORITY_HIGH,
      'main',
      { source: 'mcp_store_feedback' }
    );
    if (result.status !== 'ok' || !result.data)
      return {
        success: false,
        message: result.message ?? 'Failed to enqueue feedback',
        collection: COLLECTION_SCRATCHPAD,
      };
    // Advisory mirror for Qdrant-down rebuild recovery, same as storeScratchpad.
    writeScratchpadMirror(stateManager, content, title, tags, TENANT_FEEDBACK);
    return {
      success: true,
      message: `Feedback recorded (category=${category}${refTool ? `, tool=${refTool}` : ''}). Triage it with the /feedback-review skill.`,
      queue_id: result.data.queueId,
      collection: COLLECTION_SCRATCHPAD,
    };
  } catch (error) {
    return {
      success: false,
      message: `Failed to queue feedback: ${error instanceof Error ? error.message : String(error)}`,
      collection: COLLECTION_SCRATCHPAD,
    };
  }
}
