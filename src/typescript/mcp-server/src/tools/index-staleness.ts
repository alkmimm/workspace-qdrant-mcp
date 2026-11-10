/**
 * Stale-index warning for project-scoped read responses.
 *
 * On 2026-11-10 the daemon's queue had been stuck for 4 days: tecsul's oldest
 * queued change had waited 4d 3h, 663k items were pending, and every search
 * and grep kept answering from an index that no longer matched the code. The
 * Prometheus `QueueStuck` alert fired, but this stack has no alert dispatcher,
 * so nobody saw it. The agents calling these tools are the one channel that
 * always reaches the user, so the warning rides on their responses and asks
 * them to pass it on.
 *
 * The signal is the AGE of the project's oldest in-flight queue item, not the
 * pending count: a large backlog that is moving is normal (an initial index, a
 * branch cleanup), while an item that has waited for hours means changes are
 * not reaching the index.
 */

import type { IndexingProgress } from './search-types.js';

/** Default age past which the index is reported stale: 6 hours. Long enough
 *  that a healthy initial index of a large project (tecsul took 5.7 h) or a
 *  mass branch cleanup does not trip it; well under the 4 days it took a
 *  human to notice the 2026-11 stall. */
export const DEFAULT_INDEX_STALE_AFTER_SECONDS = 6 * 3600;

/** How long a grep reuses one staleness verdict. Staleness moves on the scale
 *  of hours, so one daemon probe per project per minute is plenty. */
const STALENESS_CACHE_TTL_MS = 60_000;

/** Attached as `index_stale` to project-scoped responses when the project's
 *  oldest queued change has waited past the threshold. */
export interface IndexStaleWarning {
  oldest_pending_age_seconds: number;
  pending: number;
  message: string;
}

/**
 * Threshold from `WQM_INDEX_STALE_WARN_SECONDS`: a positive number of seconds,
 * `0` to switch the warning off. Unset or unparsable keeps the default, so a
 * typo can never silence it.
 */
export function indexStaleAfterSeconds(
  env: Record<string, string | undefined> = process.env
): number {
  const raw = env['WQM_INDEX_STALE_WARN_SECONDS']?.trim();
  if (raw === undefined || raw === '') return DEFAULT_INDEX_STALE_AFTER_SECONDS;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0) return DEFAULT_INDEX_STALE_AFTER_SECONDS;
  return n === 0 ? Number.POSITIVE_INFINITY : n;
}

/** Compact age for humans: "4d 3h", "7h 12m", "45m". */
export function formatAge(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const d = Math.floor(s / 86_400);
  const h = Math.floor((s % 86_400) / 3_600);
  const m = Math.floor((s % 3_600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

/** The warning for `progress`, or `undefined` when the index is current enough. */
export function indexStaleWarning(
  progress: IndexingProgress | null | undefined,
  thresholdSeconds: number = indexStaleAfterSeconds()
): IndexStaleWarning | undefined {
  const age = progress?.oldest_pending_age_seconds;
  if (progress == null || typeof age !== 'number' || age < thresholdSeconds) return undefined;
  const pending = progress.pending + progress.in_progress;
  return {
    oldest_pending_age_seconds: age,
    pending,
    message:
      `The index of this project is stale: its oldest queued change has waited ` +
      `${formatAge(age)} and ${pending} item(s) are still queued, so files changed since ` +
      `then may be missing or outdated in these results. Tell the user: the indexing ` +
      `daemon's queue may be stuck (\`wqm queue stats\` shows it).`,
  };
}

interface StalenessCacheEntry {
  value: IndexStaleWarning | undefined;
  fetchedAt: number;
}

/** Keyed by tenant_id; one entry per active project. */
const stalenessCache = new Map<string, StalenessCacheEntry>();

/**
 * The project's stale-index warning for a tool that does not already fetch
 * indexing progress (grep). `probe` is the indexing-progress fetch
 * (`fetchIndexingProgress` bound to the daemon client), injected to keep this
 * module free of the search helpers. Cached per project for a minute; a failed
 * probe (`null`) yields no warning rather than a guess.
 */
export async function fetchIndexStaleWarning(
  tenantId: string,
  probe: (tenantId: string) => Promise<IndexingProgress | null>
): Promise<IndexStaleWarning | undefined> {
  const now = Date.now();
  const cached = stalenessCache.get(tenantId);
  if (cached && now - cached.fetchedAt < STALENESS_CACHE_TTL_MS) return cached.value;
  const value = indexStaleWarning(await probe(tenantId));
  stalenessCache.set(tenantId, { value, fetchedAt: now });
  return value;
}

/** Test hook: forget cached verdicts. */
export function resetIndexStalenessCacheForTests(): void {
  stalenessCache.clear();
}
