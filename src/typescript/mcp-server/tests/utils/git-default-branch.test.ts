/**
 * `getDefaultBranch` — which branch is the trunk, asked of git ONCE per repo.
 *
 * Two defects in the first version, both fixed here:
 *  - precedence: it consulted `init.defaultBranch`, which names the branch git
 *    creates in NEW repositories (a user-wide preference), not this repo's trunk;
 *  - cost/caching: up to five synchronous git subprocesses (2 s timeout each) on
 *    the request path, and a FAILED lookup cached for the life of the process —
 *    one slow-filesystem moment downgraded the repo to the index heuristic
 *    until restart.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';

import {
  clearDefaultBranchCache,
  getDefaultBranchWith,
  parseDefaultBranch,
} from '../../src/utils/git-utils.js';

const line = (ref: string, symref = '') => `${ref}\t${symref}`;

describe('parseDefaultBranch', () => {
  it('takes what origin/HEAD points at', () => {
    const out = [
      line('refs/heads/main'),
      line('refs/heads/master'),
      line('refs/remotes/origin/HEAD', 'refs/remotes/origin/master'),
    ].join('\n');
    expect(parseDefaultBranch(out)).toBe('master');
  });

  it('keeps a slash inside the trunk name', () => {
    expect(
      parseDefaultBranch(line('refs/remotes/origin/HEAD', 'refs/remotes/origin/release/2.0'))
    ).toBe('release/2.0');
  });

  it('falls back to a local main, then a local master', () => {
    expect(parseDefaultBranch([line('refs/heads/master'), line('refs/heads/main')].join('\n'))).toBe(
      'main'
    );
    expect(parseDefaultBranch(line('refs/heads/master'))).toBe('master');
  });

  it('compares ref names exactly — main/experiment is not main', () => {
    // for-each-ref treats a pattern as a prefix, so this row can appear.
    expect(parseDefaultBranch(line('refs/heads/main/experiment'))).toBeNull();
  });

  it('ignores an origin/HEAD that is not a remote-tracking symref', () => {
    expect(parseDefaultBranch(line('refs/remotes/origin/HEAD', ''))).toBeNull();
    expect(parseDefaultBranch(line('refs/remotes/origin/HEAD', 'refs/remotes/origin/'))).toBeNull();
  });

  it('returns null when git listed none of the refs', () => {
    expect(parseDefaultBranch('')).toBeNull();
  });
});

describe('getDefaultBranchWith — caching', () => {
  beforeEach(() => clearDefaultBranchCache());

  it('asks git once and keeps a definitive answer', () => {
    const run = vi.fn(() => line('refs/heads/main'));
    let now = 0;
    expect(getDefaultBranchWith('/repo', run, () => now)).toBe('main');
    now = 10 * 60 * 60 * 1000; // ten hours later
    expect(getDefaultBranchWith('/repo', run, () => now)).toBe('main');
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('keeps a definitive "no trunk here" answer too', () => {
    const run = vi.fn(() => '');
    expect(getDefaultBranchWith('/repo', run, () => 0)).toBeNull();
    expect(getDefaultBranchWith('/repo', run, () => 999_999_999)).toBeNull();
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('remembers a FAILED lookup only briefly, then asks again', () => {
    const run = vi
      .fn<(repoRoot: string) => string | null>()
      .mockReturnValueOnce(null) // git timed out / unreadable
      .mockReturnValue(line('refs/heads/main'));
    let now = 0;
    expect(getDefaultBranchWith('/repo', run, () => now)).toBeNull();
    now = 30_000; // inside the failure TTL: no new subprocess
    expect(getDefaultBranchWith('/repo', run, () => now)).toBeNull();
    expect(run).toHaveBeenCalledTimes(1);
    now = 61_000; // past it: git is asked again and the real answer sticks
    expect(getDefaultBranchWith('/repo', run, () => now)).toBe('main');
    expect(run).toHaveBeenCalledTimes(2);
  });

  it('caches per repository', () => {
    const run = vi.fn((root: string) =>
      root === '/a' ? line('refs/heads/main') : line('refs/heads/master')
    );
    expect(getDefaultBranchWith('/a', run, () => 0)).toBe('main');
    expect(getDefaultBranchWith('/b', run, () => 0)).toBe('master');
    expect(run).toHaveBeenCalledTimes(2);
  });
});
