/**
 * `countOnly` argument handling for the grep tool.
 *
 * Field feedback (DOC-V2, 2026-08-13): a caller wanting the SIZE of a surface
 * (`GoRoute\(` → 172 matches) had to fetch match bodies it would immediately
 * discard and then read the number out of truncation metadata.
 *
 * The page cap matters here and is easy to get wrong: the small agent-sized
 * default exists to bound the RESPONSE, but a count-only response has no
 * bodies — so leaving it at 100 would report a truncation upper bound instead
 * of the real count, defeating the entire mode.
 */

import { describe, it, expect } from 'vitest';

import { buildGrepOptions } from '../src/tool-builders/grep.js';

describe('buildGrepOptions — countOnly', () => {
  it('is absent by default and keeps the agent-sized page cap', () => {
    const o = buildGrepOptions({ pattern: 'GoRoute' });
    expect(o.countOnly).toBeUndefined();
    expect(o.maxResults).toBe(100);
  });

  it('raises the page cap so the reported count is exact, not truncated', () => {
    const o = buildGrepOptions({ pattern: 'GoRoute', countOnly: true });
    expect(o.countOnly).toBe(true);
    expect(o.maxResults).toBe(10000);
  });

  it('never overrides an explicit maxResults', () => {
    const o = buildGrepOptions({ pattern: 'GoRoute', countOnly: true, maxResults: 5 });
    expect(o.countOnly).toBe(true);
    expect(o.maxResults).toBe(5);
  });

  it('countOnly:false behaves exactly like omitting it', () => {
    const o = buildGrepOptions({ pattern: 'GoRoute', countOnly: false });
    expect(o.countOnly).toBeUndefined();
    expect(o.maxResults).toBe(100);
  });

  it('carries the other filters through untouched', () => {
    const o = buildGrepOptions({
      pattern: 'GoRoute',
      countOnly: true,
      regex: true,
      pathGlob: 'lib/**/*.dart',
    });
    expect(o).toMatchObject({
      pattern: 'GoRoute',
      countOnly: true,
      regex: true,
      pathGlob: 'lib/**/*.dart',
    });
  });
});
