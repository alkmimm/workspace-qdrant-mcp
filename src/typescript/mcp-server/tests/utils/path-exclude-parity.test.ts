/**
 * `pathExclude` parity between this server and the daemon.
 *
 * The daemon now applies the caller's exclude BEFORE its result cap
 * (`TextSearchRequest.path_exclude`). Applied only here, after the cap, the
 * excluded paths consumed the page: `grep "pub fn" pathExclude:"src/rust/**"
 * maxResults:3` came back EMPTY, `truncated: true`, with no continuation and a
 * total of 2021 for 13 real hits.
 *
 * Moving the filter is only safe if the two matchers agree exactly. The daemon's
 * existing glob matcher is lenient on purpose (its `*` crosses `/`, `[...]` is a
 * class) — fine for an include, wrong for an exclude, which would then delete
 * hits this server keeps. So the daemon carries a port of `matchesPathExclude`,
 * and this shared table — GENERATED from the TypeScript implementation — is
 * asserted by both suites (`text_search::path_exclude` tests on the Rust side).
 */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, it, expect } from 'vitest';

import { matchesPathExclude } from '../../src/utils/path-glob.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
// tests/utils/ → mcp-server/ → typescript/ → src/ → repo root
const REPO_ROOT = resolve(__dirname, '..', '..', '..', '..', '..');
const TABLE_PATH = resolve(REPO_ROOT, 'assets', 'path_exclude_parity.json');

interface ParityCase {
  path: string;
  exclude: string;
  excluded: boolean;
  why: string;
}

const table = JSON.parse(readFileSync(TABLE_PATH, 'utf8')) as { cases: ParityCase[] };

describe('pathExclude parity table', () => {
  it('is present and non-trivial', () => {
    expect(table.cases.length).toBeGreaterThanOrEqual(30);
  });

  it('pins the two cases where the daemon include matcher would over-exclude', () => {
    // If someone regenerates the table and these disappear, the parity check
    // stops guarding the exact divergence that made a port necessary.
    const single = table.cases.find((c) => c.exclude === 'src/*.rs' && c.path.includes('/deep/'));
    const bracket = table.cases.find((c) => c.exclude === 'src/[x].rs' && c.path === '/repo/src/x.rs');
    expect(single?.excluded).toBe(false);
    expect(bracket?.excluded).toBe(false);
  });

  it.each(table.cases.map((c) => [`${c.exclude} vs ${c.path}`, c] as const))(
    'server agrees with the table: %s',
    (_label, c) => {
      expect(matchesPathExclude(c.path, c.exclude), c.why).toBe(c.excluded);
    }
  );
});
