/**
 * Stale-checkout warning (checkout-staleness.ts): on 2026-11-12 DOC-V2's main
 * checkout sat 412 commits behind origin/main while agents branched off
 * origin, and search/grep on the trunk answered from that old code with no
 * sign of it. These pin the threshold, the wording, the int64 normalization,
 * and that search and grep attach it from the status they already fetch.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { GetProjectStatusResponse } from '../../src/clients/grpc-types.js';
import { normalizeProjectStatusCounts } from '../../src/clients/daemon-client/system-methods.js';
import {
  DEFAULT_CHECKOUT_BEHIND_WARN_COMMITS,
  checkoutBehindWarnCommits,
  checkoutLagOf,
  checkoutStaleWarning,
} from '../../src/tools/checkout-staleness.js';
import {
  fetchReadWarnings,
  resetIndexStalenessCacheForTests,
} from '../../src/tools/index-staleness.js';
import { attachIndexingProgress } from '../../src/tools/search-helpers.js';
import type { IndexingProgress, SearchResponse } from '../../src/tools/search-types.js';

function progress(behind?: number, ahead = 0): IndexingProgress {
  const p: IndexingProgress = { pending: 0, in_progress: 0, failed: 0, done: 10, total: 10, percent: 100 };
  if (behind !== undefined) p.checkout_lag = { trunk: 'main', behind, ahead };
  return p;
}

describe('checkoutBehindWarnCommits', () => {
  it('defaults to 20 and ignores unusable values; 0 switches it off', () => {
    expect(DEFAULT_CHECKOUT_BEHIND_WARN_COMMITS).toBe(20);
    expect(checkoutBehindWarnCommits({})).toBe(20);
    expect(checkoutBehindWarnCommits({ WQM_CHECKOUT_BEHIND_WARN_COMMITS: 'lots' })).toBe(20);
    expect(checkoutBehindWarnCommits({ WQM_CHECKOUT_BEHIND_WARN_COMMITS: '-1' })).toBe(20);
    expect(checkoutBehindWarnCommits({ WQM_CHECKOUT_BEHIND_WARN_COMMITS: ' 5 ' })).toBe(5);
    expect(checkoutBehindWarnCommits({ WQM_CHECKOUT_BEHIND_WARN_COMMITS: '0' })).toBe(
      Number.POSITIVE_INFINITY
    );
  });
});

describe('checkoutStaleWarning', () => {
  it('warns from the threshold on, naming the trunk and the remedy', () => {
    const w = checkoutStaleWarning(progress(412), 20);
    expect(w).toMatchObject({ trunk: 'main', behind: 412, ahead: 0 });
    expect(w?.message).toContain('412 commit(s) behind origin/main');
    expect(w?.message).toContain('git pull --ff-only');
    expect(w?.message).not.toContain('local commit');
    expect(checkoutStaleWarning(progress(20), 20)).toBeDefined();
  });

  it('says a diverged checkout needs more than a fast-forward', () => {
    expect(checkoutStaleWarning(progress(30, 2), 20)?.message).toContain('2 local commit(s)');
  });

  it('stays quiet below the threshold, without a lag, or switched off', () => {
    expect(checkoutStaleWarning(progress(19), 20)).toBeUndefined();
    expect(checkoutStaleWarning(progress(), 20)).toBeUndefined();
    expect(checkoutStaleWarning(null, 20)).toBeUndefined();
    expect(checkoutStaleWarning(progress(412), Number.POSITIVE_INFINITY)).toBeUndefined();
  });
});

describe('daemon status → lag', () => {
  it('normalizes the int64 counts the gRPC layer delivers as strings', () => {
    const raw = {
      found: true,
      checkout_trunk: 'main',
      checkout_behind_commits: '412',
      checkout_ahead_commits: '0',
    } as unknown as GetProjectStatusResponse;
    const status = normalizeProjectStatusCounts(raw);
    expect(checkoutLagOf(status)).toEqual({ trunk: 'main', behind: 412, ahead: 0 });
    const none = normalizeProjectStatusCounts({ found: true } as GetProjectStatusResponse);
    expect('checkout_behind_commits' in none).toBe(false);
    expect(checkoutLagOf(none)).toBeUndefined();
  });
});

describe('read responses', () => {
  beforeEach(() => resetIndexStalenessCacheForTests());

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

  it('search attaches checkout_stale from the status it already fetched', async () => {
    const response = emptyResponse();
    await attachIndexingProgress(
      response,
      daemonWith({ checkout_trunk: 'main', checkout_behind_commits: 412, checkout_ahead_commits: 0 }),
      'project',
      'lagging-tenant-1'
    );
    expect(response.checkout_stale).toMatchObject({ trunk: 'main', behind: 412, ahead: 0 });
    // Carried by the warning; the indexing block does not repeat it.
    expect(response.indexing).toBeDefined();
    expect(response.indexing?.checkout_lag).toBeUndefined();
  });

  it('a current checkout ships neither the lag nor the warning', async () => {
    const response = emptyResponse();
    await attachIndexingProgress(
      response,
      daemonWith({ checkout_trunk: 'main', checkout_behind_commits: 0, checkout_ahead_commits: 0 }),
      'project',
      'current-tenant-1'
    );
    expect(response.indexing).toBeDefined();
    expect(response.indexing?.checkout_lag).toBeUndefined();
    expect(response.checkout_stale).toBeUndefined();
  });

  it('grep gets both verdicts from one cached probe', async () => {
    const probe = vi.fn().mockResolvedValue(progress(412));
    const first = await fetchReadWarnings('tenant-lag', probe);
    const second = await fetchReadWarnings('tenant-lag', probe);
    expect(first.checkoutStale?.behind).toBe(412);
    expect(first.indexStale).toBeUndefined();
    expect(second).toBe(first);
    expect(probe).toHaveBeenCalledTimes(1);
  });
});
