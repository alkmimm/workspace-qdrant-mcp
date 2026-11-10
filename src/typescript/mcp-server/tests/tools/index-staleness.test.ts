/**
 * Stale-index warning (index-staleness.ts): the 2026-11 stall left the daemon
 * queue stuck for 4 days while every search and grep kept answering from an
 * outdated index, and the only alert went nowhere. These pin the threshold,
 * the wording the agent relays, and that search attaches it from the progress
 * it already fetches.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { GetProjectStatusResponse } from '../../src/clients/grpc-types.js';
import { normalizeProjectStatusCounts } from '../../src/clients/daemon-client/system-methods.js';
import {
  DEFAULT_INDEX_STALE_AFTER_SECONDS,
  fetchIndexStaleWarning,
  formatAge,
  indexStaleAfterSeconds,
  indexStaleWarning,
  resetIndexStalenessCacheForTests,
} from '../../src/tools/index-staleness.js';
import { attachIndexingProgress } from '../../src/tools/search-helpers.js';
import type { IndexingProgress, SearchResponse } from '../../src/tools/search-types.js';

const HOUR = 3600;
const STALL = 4 * 86_400 + 3 * HOUR; // the observed tecsul stall

function progress(age: number | undefined, pending = 353_706): IndexingProgress {
  const p: IndexingProgress = {
    pending,
    in_progress: 1,
    failed: 0,
    done: 10,
    total: pending + 11,
    percent: 1,
  };
  if (age !== undefined) p.oldest_pending_age_seconds = age;
  return p;
}

describe('indexStaleAfterSeconds', () => {
  it('defaults to 6 hours and ignores unusable values', () => {
    expect(DEFAULT_INDEX_STALE_AFTER_SECONDS).toBe(6 * HOUR);
    expect(indexStaleAfterSeconds({})).toBe(6 * HOUR);
    expect(indexStaleAfterSeconds({ WQM_INDEX_STALE_WARN_SECONDS: '' })).toBe(6 * HOUR);
    expect(indexStaleAfterSeconds({ WQM_INDEX_STALE_WARN_SECONDS: 'soon' })).toBe(6 * HOUR);
    expect(indexStaleAfterSeconds({ WQM_INDEX_STALE_WARN_SECONDS: '-5' })).toBe(6 * HOUR);
  });

  it('takes a positive number of seconds, and 0 switches the warning off', () => {
    expect(indexStaleAfterSeconds({ WQM_INDEX_STALE_WARN_SECONDS: ' 3600 ' })).toBe(3600);
    expect(indexStaleAfterSeconds({ WQM_INDEX_STALE_WARN_SECONDS: '0' })).toBe(
      Number.POSITIVE_INFINITY
    );
    expect(indexStaleWarning(progress(STALL), Number.POSITIVE_INFINITY)).toBeUndefined();
  });
});

describe('formatAge', () => {
  it('renders the two largest units', () => {
    expect(formatAge(STALL)).toBe('4d 3h');
    expect(formatAge(7 * HOUR + 12 * 60 + 59)).toBe('7h 12m');
    expect(formatAge(45 * 60)).toBe('45m');
    expect(formatAge(-3)).toBe('0m');
  });
});

describe('indexStaleWarning', () => {
  it('warns once the oldest queued change has waited past the threshold', () => {
    const w = indexStaleWarning(progress(STALL), 6 * HOUR);
    expect(w).toBeDefined();
    expect(w?.oldest_pending_age_seconds).toBe(STALL);
    expect(w?.pending).toBe(353_707); // pending + in_progress
    expect(w?.message).toContain('stale');
    expect(w?.message).toContain('4d 3h');
    expect(w?.message).toContain('353707 item(s)');
    expect(w?.message).toContain('Tell the user');
    expect(indexStaleWarning(progress(6 * HOUR), 6 * HOUR)).toBeDefined();
  });

  it('stays quiet for a young backlog, an empty queue, or no signal', () => {
    // A big backlog that is moving is normal (initial index, branch cleanup).
    expect(indexStaleWarning(progress(6 * HOUR - 1), 6 * HOUR)).toBeUndefined();
    expect(indexStaleWarning(progress(undefined), 6 * HOUR)).toBeUndefined();
    expect(indexStaleWarning(null, 6 * HOUR)).toBeUndefined();
    expect(indexStaleWarning(undefined, 6 * HOUR)).toBeUndefined();
  });
});

describe('fetchIndexStaleWarning', () => {
  beforeEach(() => resetIndexStalenessCacheForTests());

  it('probes once per project per minute and caches the verdict', async () => {
    const probe = vi.fn().mockResolvedValue(progress(STALL));
    const first = await fetchIndexStaleWarning('tenant-a', probe);
    const second = await fetchIndexStaleWarning('tenant-a', probe);
    expect(first?.oldest_pending_age_seconds).toBe(STALL);
    expect(second).toBe(first);
    expect(probe).toHaveBeenCalledTimes(1);
    await fetchIndexStaleWarning('tenant-b', probe);
    expect(probe).toHaveBeenCalledTimes(2);
  });

  it('gives no warning when the probe fails', async () => {
    expect(await fetchIndexStaleWarning('tenant-c', async () => null)).toBeUndefined();
  });
});

describe('daemon status → search response', () => {
  function daemonWith(status: Partial<GetProjectStatusResponse>): DaemonClient {
    return {
      getProjectStatus: vi.fn().mockResolvedValue({ found: true, ...status }),
    } as unknown as DaemonClient;
  }

  function emptyResponse(): SearchResponse {
    return {
      results: [],
      total: 0,
      query: 'q',
      mode: 'semantic',
      scope: 'project',
      collections_searched: ['projects'],
    } as unknown as SearchResponse;
  }

  it('normalizes the int64 age the gRPC layer delivers as a string', () => {
    const raw = {
      found: true,
      oldest_pending_age_seconds: String(STALL),
    } as unknown as GetProjectStatusResponse;
    expect(normalizeProjectStatusCounts(raw).oldest_pending_age_seconds).toBe(STALL);
    const none = normalizeProjectStatusCounts({ found: true } as GetProjectStatusResponse);
    expect('oldest_pending_age_seconds' in none).toBe(false);
  });

  it('search attaches index_stale from the progress it already fetched', async () => {
    const response = emptyResponse();
    await attachIndexingProgress(
      response,
      daemonWith({
        pending_count: 353_706,
        in_progress_count: 1,
        oldest_pending_age_seconds: STALL,
      }),
      'project',
      'stale-tenant-1'
    );
    expect(response.indexing?.oldest_pending_age_seconds).toBe(STALL);
    expect(response.index_stale?.message).toContain('4d 3h');
  });

  it('search leaves index_stale out while the queue is moving', async () => {
    const response = emptyResponse();
    await attachIndexingProgress(
      response,
      daemonWith({ pending_count: 120, oldest_pending_age_seconds: 300 }),
      'project',
      'fresh-tenant-1'
    );
    expect(response.indexing?.pending).toBe(120);
    expect(response.index_stale).toBeUndefined();
  });
});
