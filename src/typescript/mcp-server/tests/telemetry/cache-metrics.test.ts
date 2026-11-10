/**
 * wqm_mcp_cache_hits_total / wqm_mcp_cache_misses_total were declared and
 * never incremented, so Grafana's "Cache Hit Ratio" panel showed "No data"
 * for its whole life. These pin that every in-process cache is exported from
 * start-up and that real lookups count as hits and misses.
 */
import { describe, expect, it } from 'vitest';
import {
  CACHE_NAMES,
  cacheHits,
  cacheMisses,
  recordCacheLookup,
  type CacheName,
} from '../../src/telemetry/metrics.js';
import { getDefaultBranchWith } from '../../src/utils/git-utils.js';
import {
  fetchIndexStaleWarning,
  resetIndexStalenessCacheForTests,
} from '../../src/tools/index-staleness.js';

async function count(counter: typeof cacheHits, cache: CacheName): Promise<number | undefined> {
  const m = await counter.get();
  return m.values.find((v) => v.labels['cache'] === cache)?.value;
}

describe('cache metrics', () => {
  it('exports every cache from start-up, so an idle cache reads 0, not "No data"', async () => {
    for (const name of CACHE_NAMES) {
      expect(await count(cacheHits, name)).toBeTypeOf('number');
      expect(await count(cacheMisses, name)).toBeTypeOf('number');
    }
  });

  it('recordCacheLookup counts a hit or a miss', async () => {
    const h0 = (await count(cacheHits, 'daemon_check')) ?? 0;
    const m0 = (await count(cacheMisses, 'daemon_check')) ?? 0;
    recordCacheLookup('daemon_check', true);
    recordCacheLookup('daemon_check', false);
    recordCacheLookup('daemon_check', false);
    expect(await count(cacheHits, 'daemon_check')).toBe(h0 + 1);
    expect(await count(cacheMisses, 'daemon_check')).toBe(m0 + 2);
  });

  it('a real cache records its first lookup as a miss and the repeat as a hit', async () => {
    const h0 = (await count(cacheHits, 'default_branch')) ?? 0;
    const m0 = (await count(cacheMisses, 'default_branch')) ?? 0;
    const run = (): string => 'refs/heads/main\t\n';
    const repo = `/tmp/cache-metrics-${Date.now()}`;
    getDefaultBranchWith(repo, run, () => 1);
    getDefaultBranchWith(repo, run, () => 2);
    expect(await count(cacheMisses, 'default_branch')).toBe(m0 + 1);
    expect(await count(cacheHits, 'default_branch')).toBe(h0 + 1);
  });

  it('the stale-index cache counts the same way', async () => {
    resetIndexStalenessCacheForTests();
    const h0 = (await count(cacheHits, 'index_staleness')) ?? 0;
    const m0 = (await count(cacheMisses, 'index_staleness')) ?? 0;
    const probe = async (): Promise<null> => null;
    await fetchIndexStaleWarning('cache-metrics-tenant', probe);
    await fetchIndexStaleWarning('cache-metrics-tenant', probe);
    expect(await count(cacheMisses, 'index_staleness')).toBe(m0 + 1);
    expect(await count(cacheHits, 'index_staleness')).toBe(h0 + 1);
  });
});
