/**
 * Scratchpad management tool — list, update, and delete existing scratchpad
 * notes. (Creation stays in `store(type="scratchpad")`, which also wires the
 * project tenant + recall-lane behavior.)
 *
 * Identity is content-addressed: a note's Qdrant point id derives from
 * `hash(tenant_id, content)`, so an entry is identified by its current
 * `content` (obtained from a prior `search`/`scratchpad list`) — or by its
 * point `id`, which the tool resolves back to the content via a tenant-checked
 * Qdrant lookup before following the same content-addressed flow. Mutations
 * are enqueued to the unified queue (daemon-owned writes); the daemon removes
 * the point + its mirror row (delete) or upserts the new content and evicts
 * the superseded point (update). Reads (list) scroll Qdrant directly and are
 * shaped like every other read surface: summary entries by default (preview +
 * content_length), the shared search/grep response byte budget, and cursor
 * pagination over Qdrant's scroll offset.
 */

import type { QdrantClient } from '@qdrant/js-client-rest';
import { getQdrantClient } from '../clients/qdrant-client-factory.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import { randomUUID } from 'node:crypto';
import { utcNow } from '../utils/timestamps.js';
import {
  COLLECTION_SCRATCHPAD,
  PRIORITY_HIGH,
  FIELD_TENANT_ID,
  FIELD_CONTENT,
  FIELD_TITLE,
} from '../common/native-bridge.js';
import { resolveScopedTenant, type ScopedTenant } from './tenant-scope.js';
import { resolveScratchpadOrigin } from './scratchpad-origin.js';
import { applyByteBudget } from './response-budget.js';
import { scopedTenantEcho, type ProjectSource } from './project-echo.js';
import { DEFAULT_MAX_RESPONSE_BYTES } from './search-types.js';

/** Preview length (chars) for summary-mode list entries. */
const LIST_PREVIEW_CHARS = 200;

export type ScratchpadAction = 'list' | 'update' | 'delete';

export interface ScratchpadOptions {
  action: ScratchpadAction;
  /** Current text of the target note — its identity for update/delete. */
  content?: string;
  /**
   * Point id of the target note (from `scratchpad list` / a search hit) —
   * alternative identity for update/delete when `content` is omitted. Ids are
   * content-derived, so an update changes the note's id.
   */
  id?: string;
  /** Replacement text (update only). */
  newContent?: string;
  /** New title (update only). */
  title?: string;
  /** New tags (update only). */
  tags?: string[];
  /** Tenant the note belongs to (takes precedence over cwd). */
  projectId?: string;
  /** Max entries for list (default 50). */
  limit?: number;
  /**
   * For list: return summary entries (preview + content_length) instead of
   * full note bodies. Default true — pass false for full bodies (the response
   * byte budget still applies).
   */
  summary?: boolean;
  /**
   * For list: cap on total response chars (default: the shared search/grep
   * budget, ~24k). Trailing entries are dropped (>=1 kept), reported via
   * `budget_truncated`, and `next_cursor` resumes at the first dropped entry.
   * 0 disables.
   */
  maxResponseBytes?: number;
  /**
   * For list: opaque pagination cursor — pass the `next_cursor` from a
   * previous list response to fetch the next page.
   */
  cursor?: string;
}

export interface ScratchpadEntry {
  id: string;
  /** Full note text — present only with summary:false. */
  content?: string;
  /** Leading slice of the note (summary mode). */
  preview?: string;
  /** Total note length in chars (summary mode). */
  content_length?: number;
  title?: string;
  tags?: string[];
  created_at?: string;
  updated_at?: string;
  /**
   * Write-time provenance (stamped since the provenance work, absent on older
   * notes — absence means "unknown", not stale). Staleness signals for note
   * reviews: a deleted origin branch or a dead origin cwd usually marks a
   * note whose work has finished.
   */
  origin_branch?: string;
  origin_cwd?: string;
  origin_worktree?: boolean;
}

export interface ScratchpadResponse {
  success: boolean;
  action: ScratchpadAction;
  message?: string;
  entries?: ScratchpadEntry[];
  count?: number;
  /** Total notes for the tenant (best-effort; omitted if the count fails). */
  total?: number;
  /**
   * Pagination cursor — pass back as `cursor` for the next page. Present when
   * more entries remain (a further scroll page, or a budget-dropped tail).
   */
  next_cursor?: string;
  /** Entries dropped by the response byte budget (resume via next_cursor). */
  budget_truncated?: { dropped: number };
  hint?: string;
  queue_id?: string;
  tenant_id?: string;
  /** Read-side project echo on list (parity with search/grep/list/retrieve/
   *  graph): the tenant answered from and how it was resolved. */
  project_id?: string;
  project_path?: string;
  project_source?: ProjectSource;
}

export interface ScratchpadToolConfig {
  qdrantUrl: string;
  qdrantApiKey?: string;
  qdrantTimeout?: number;
}

export class ScratchpadTool {
  private readonly qdrantClient: QdrantClient;
  private readonly stateManager: SqliteStateManager;
  private readonly projectDetector: ProjectDetector;

  constructor(
    config: ScratchpadToolConfig,
    stateManager: SqliteStateManager,
    projectDetector: ProjectDetector
  ) {
    this.qdrantClient = getQdrantClient({
      url: config.qdrantUrl,
      apiKey: config.qdrantApiKey,
      timeout: config.qdrantTimeout ?? 5000,
    });
    this.stateManager = stateManager;
    this.projectDetector = projectDetector;
  }

  /**
   * Resolve the tenant whose notes the action targets, through the SAME shared
   * resolver every project-scoped write uses: explicit projectId → the project
   * detected from the effective cwd → global. There is no session project rung
   * here (the tool is stateless), which is exactly why this path never suffered
   * the store misroute — pass the tenant_id seen in a search/list result as
   * projectId to target a specific project's notes.
   */
  private async resolveTenant(projectId: string | undefined): Promise<ScopedTenant> {
    return resolveScopedTenant({
      explicitProjectId: projectId,
      projectDetector: this.projectDetector,
      stateManager: this.stateManager,
    });
  }

  async execute(options: ScratchpadOptions): Promise<ScratchpadResponse> {
    const scoped = await this.resolveTenant(options.projectId);
    const tenantId = scoped.tenantId;
    switch (options.action) {
      case 'list':
        return this.list(tenantId, options, scoped);
      case 'delete':
        return this.delete(tenantId, options);
      case 'update':
        return this.update(tenantId, options);
      default:
        return {
          success: false,
          action: options.action,
          message: `Unknown action: ${options.action}`,
        };
    }
  }

  private async list(
    tenantId: string,
    options: ScratchpadOptions,
    scoped?: ScopedTenant
  ): Promise<ScratchpadResponse> {
    const limit = options.limit ?? 50;
    const summary = options.summary ?? true;
    const budget = options.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES;
    try {
      const result = await this.qdrantClient.scroll(COLLECTION_SCRATCHPAD, {
        filter: { must: [{ key: FIELD_TENANT_ID, match: { value: tenantId } }] },
        limit,
        with_payload: true,
        ...(options.cursor ? { offset: options.cursor } : {}),
      });
      const entries: ScratchpadEntry[] = result.points.map((p) => {
        const payload = (p.payload ?? {}) as Record<string, unknown>;
        const content = (payload[FIELD_CONTENT] as string) ?? '';
        const entry: ScratchpadEntry = { id: String(p.id) };
        if (summary) {
          entry.preview = content.slice(0, LIST_PREVIEW_CHARS);
          entry.content_length = content.length;
        } else {
          entry.content = content;
        }
        const title = payload[FIELD_TITLE] as string | undefined;
        if (title) entry.title = title;
        const tags = payload['tags'] as string[] | undefined;
        if (Array.isArray(tags) && tags.length > 0) entry.tags = tags;
        const createdAt = payload['created_at'] as string | undefined;
        if (createdAt) entry.created_at = createdAt;
        const updatedAt = payload['updated_at'] as string | undefined;
        if (updatedAt) entry.updated_at = updatedAt;
        const originBranch = payload['origin_branch'] as string | undefined;
        if (originBranch) entry.origin_branch = originBranch;
        const originCwd = payload['origin_cwd'] as string | undefined;
        if (originCwd) entry.origin_cwd = originCwd;
        const originWorktree = payload['origin_worktree'];
        if (typeof originWorktree === 'boolean') entry.origin_worktree = originWorktree;
        return entry;
      });
      // Shared response budget (same semantics as search/grep): trailing
      // entries are dropped (>=1 kept). Qdrant scroll offsets are inclusive
      // point ids, so resuming at the first dropped entry loses nothing.
      const { kept, dropped } = applyByteBudget(entries, (e) => JSON.stringify(e).length, budget);
      const response: ScratchpadResponse = {
        success: true,
        action: 'list',
        entries: kept,
        count: kept.length,
        tenant_id: tenantId,
        ...(scoped ? scopedTenantEcho(scoped) : {}),
        message: `Found ${kept.length} scratchpad entr${kept.length === 1 ? 'y' : 'ies'} for ${tenantId}`,
      };
      const firstDropped = entries[kept.length];
      if (dropped > 0 && firstDropped) {
        response.budget_truncated = { dropped };
        response.next_cursor = firstDropped.id;
      } else if (result.next_page_offset !== null && result.next_page_offset !== undefined) {
        response.next_cursor = String(result.next_page_offset);
      }
      const total = await this.countNotes(tenantId);
      if (total !== undefined) response.total = total;
      if (summary) {
        response.hint =
          "Entries are summaries (preview + content_length). For one note's full " +
          'text use retrieve (collection:"scratchpad", documentId:<id>) or pass ' +
          'summary:false; to find notes by content use search (collection:"scratchpad").';
      }
      return response;
    } catch (error) {
      return {
        success: false,
        action: 'list',
        message: `Failed to list scratchpad entries: ${error instanceof Error ? error.message : 'unknown error'}`,
      };
    }
  }

  /**
   * Tenant note count via the Qdrant count API — best-effort so a count
   * failure never fails the list (the `total` field is simply omitted).
   */
  private async countNotes(tenantId: string): Promise<number | undefined> {
    try {
      const res = await this.qdrantClient.count(COLLECTION_SCRATCHPAD, {
        filter: { must: [{ key: FIELD_TENANT_ID, match: { value: tenantId } }] },
        exact: true,
      });
      return res.count;
    } catch {
      return undefined;
    }
  }

  /**
   * Does a scratchpad note with EXACTLY this content exist for the tenant?
   * update/delete are content-addressed (document_id = hash(tenant, content)),
   * so a near-miss (e.g. a truncated `search` hit) would otherwise silently
   * no-op. Fails OPEN on a Qdrant error — never blocks the mutation.
   */
  private async noteExists(tenantId: string, content: string): Promise<boolean> {
    try {
      const res = await this.qdrantClient.scroll(COLLECTION_SCRATCHPAD, {
        filter: {
          must: [
            { key: FIELD_TENANT_ID, match: { value: tenantId } },
            { key: FIELD_CONTENT, match: { value: content } },
          ],
        },
        limit: 1,
        with_payload: false,
      });
      return (res.points?.length ?? 0) > 0;
    } catch {
      return true; // fail open: a transient lookup error must not block the op
    }
  }

  /** Shared "exact content not found" message for update/delete. */
  private notFoundMessage(tenantId: string): string {
    return (
      `No scratchpad entry with that exact content was found for ${tenantId}. ` +
      'Entries are content-addressed, so the text must match the note VERBATIM. ' +
      'Prefer targeting by point `id` (from `scratchpad list`) instead; for the ' +
      'verbatim text, retrieve the single note by its point id (`retrieve` with ' +
      'collection:"scratchpad") or use `scratchpad list` with summary:false — ' +
      'a `search` hit body may be truncated.'
    );
  }

  /**
   * Resolve a note's current content from its Qdrant point id (the `id` from
   * `scratchpad list` / a search hit). Tenant-checked: a point belonging to a
   * different tenant resolves as not-found rather than leaking a cross-project
   * mutation. Returns undefined when the point is missing or unreadable.
   */
  private async contentById(tenantId: string, id: string): Promise<string | undefined> {
    try {
      const points = await this.qdrantClient.retrieve(COLLECTION_SCRATCHPAD, {
        ids: [id],
        with_payload: true,
      });
      const payload = points[0]?.payload as Record<string, unknown> | undefined;
      if (!payload) return undefined;
      if ((payload[FIELD_TENANT_ID] as string | undefined) !== tenantId) return undefined;
      const content = payload[FIELD_CONTENT] as string | undefined;
      return content?.trim() ? content.trim() : undefined;
    } catch {
      return undefined;
    }
  }

  /**
   * Resolve the target note's current content for update/delete: verbatim
   * `content` wins; otherwise the point `id` is looked up (tenant-checked).
   * Returns a ready error response when neither identifies a note.
   */
  private async resolveTarget(
    tenantId: string,
    options: ScratchpadOptions,
    action: 'update' | 'delete'
  ): Promise<{ content: string } | { response: ScratchpadResponse }> {
    const verbatim = options.content?.trim();
    if (verbatim) return { content: verbatim };
    const id = options.id?.trim();
    if (id) {
      const content = await this.contentById(tenantId, id);
      if (content !== undefined) return { content };
      return {
        response: {
          success: false,
          action,
          message:
            `No scratchpad entry with point id "${id}" was found for ${tenantId}. ` +
            'Get the id from `scratchpad list` — note that ids are content-derived, ' +
            "so a prior update changes the note's id.",
          tenant_id: tenantId,
        },
      };
    }
    return {
      response: {
        success: false,
        action,
        message:
          `content or id is required for ${action} — the current text of the note ` +
          'or its point id (both from `scratchpad list`).',
      },
    };
  }

  private async delete(tenantId: string, options: ScratchpadOptions): Promise<ScratchpadResponse> {
    const target = await this.resolveTarget(tenantId, options, 'delete');
    if ('response' in target) return target.response;
    const content = target.content;
    if (!(await this.noteExists(tenantId, content))) {
      return {
        success: false,
        action: 'delete',
        message: this.notFoundMessage(tenantId),
        tenant_id: tenantId,
      };
    }
    const result = await this.stateManager.enqueueUnified(
      'text',
      'delete',
      tenantId,
      COLLECTION_SCRATCHPAD,
      { content, source_type: 'scratchpad' },
      PRIORITY_HIGH,
      'main',
      { source: 'mcp_scratchpad_tool' }
    );
    if (result.status !== 'ok' || !result.data) {
      return {
        success: false,
        action: 'delete',
        message: result.message ?? 'Failed to enqueue scratchpad delete',
      };
    }
    return {
      success: true,
      action: 'delete',
      message: `Scratchpad entry deletion queued for processing (${tenantId})`,
      queue_id: result.data.queueId,
      tenant_id: tenantId,
    };
  }

  private async update(tenantId: string, options: ScratchpadOptions): Promise<ScratchpadResponse> {
    const newContent = options.newContent;
    if (!newContent?.trim()) {
      return {
        success: false,
        action: 'update',
        message: 'newContent is required for update — the replacement text.',
      };
    }
    const target = await this.resolveTarget(tenantId, options, 'update');
    if ('response' in target) return target.response;
    const oldContent = target.content;
    if (!(await this.noteExists(tenantId, oldContent))) {
      return {
        success: false,
        action: 'update',
        message: this.notFoundMessage(tenantId),
        tenant_id: tenantId,
      };
    }

    // Re-stamp provenance on update: origin_* records the last write's
    // context (this tool is stateless, so detection runs from the cwd).
    const origin = await resolveScratchpadOrigin({ projectDetector: this.projectDetector });
    const payload: Record<string, unknown> = {
      content: newContent.trim(),
      old_content: oldContent,
      source_type: 'scratchpad',
      ...origin,
    };
    if (options.title?.trim()) payload['title'] = options.title.trim();
    if (options.tags && options.tags.length > 0) payload['tags'] = options.tags;

    const result = await this.stateManager.enqueueUnified(
      'text',
      'update',
      tenantId,
      COLLECTION_SCRATCHPAD,
      payload,
      PRIORITY_HIGH,
      'main',
      { source: 'mcp_scratchpad_tool' }
    );
    if (result.status !== 'ok' || !result.data) {
      return {
        success: false,
        action: 'update',
        message: result.message ?? 'Failed to enqueue scratchpad update',
      };
    }

    // Refresh the advisory mirror with the new content (best-effort). The daemon
    // evicts the old mirror row by old content; this writes the new one so the
    // mirror stays usable for the Qdrant-down fallback before the next rebuild.
    const now = utcNow();
    this.stateManager.upsertScratchpadMirror({
      scratchpadId: randomUUID(),
      title: options.title?.trim() ?? null,
      content: newContent.trim(),
      tags: JSON.stringify(options.tags ?? []),
      tenantId,
      createdAt: now,
      updatedAt: now,
    });

    return {
      success: true,
      action: 'update',
      message: `Scratchpad entry update queued for processing (${tenantId})`,
      queue_id: result.data.queueId,
      tenant_id: tenantId,
    };
  }
}
