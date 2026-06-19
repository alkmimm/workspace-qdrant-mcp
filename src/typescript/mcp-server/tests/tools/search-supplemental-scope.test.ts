/**
 * Guardrail for the supplemental symbol-candidate scope filter.
 *
 * Supplemental candidate point ids come from the SQLite `qdrant_chunks` mirror,
 * which is filtered only by watch_folder + needle and is known to drift from
 * Qdrant. `filterSupplementalPointsToScope` re-checks each retrieved point's
 * payload against the caller's full scope (tenant + concrete branch + active
 * base_points) so a symbol match can never surface a chunk from another branch
 * or another clone of the project. Without it, #115's recall feature leaked
 * cross-branch results — the exact contract #112 had just established.
 */
import { describe, it, expect } from 'vitest';
import { filterSupplementalPointsToScope } from '../../src/tools/search-helpers.js';
import { FIELD_BASE_POINT, FIELD_BRANCH, FIELD_TENANT_ID } from '../../src/common/native-bridge.js';

const TENANT = 'tenant-1';

function pt(
  id: string,
  payload: Record<string, unknown>
): { id: string; payload: Record<string, unknown> } {
  return { id, payload };
}
const onMain = (id: string, extra: Record<string, unknown> = {}) =>
  pt(id, { [FIELD_TENANT_ID]: TENANT, [FIELD_BRANCH]: 'main', ...extra });
const onFeature = (id: string, extra: Record<string, unknown> = {}) =>
  pt(id, { [FIELD_TENANT_ID]: TENANT, [FIELD_BRANCH]: 'feature/x', ...extra });

describe('filterSupplementalPointsToScope', () => {
  it('drops points from a different branch when the branch is concrete', () => {
    const kept = filterSupplementalPointsToScope([onMain('a'), onFeature('b'), onMain('c')], {
      tenantId: TENANT,
      branch: 'main',
      basePoints: undefined,
    });
    expect(kept.map((p) => p.id)).toEqual(['a', 'c']);
  });

  it("keeps every branch when branch is '*' (explicit cross-branch opt-out)", () => {
    const kept = filterSupplementalPointsToScope([onMain('a'), onFeature('b')], {
      tenantId: TENANT,
      branch: '*',
      basePoints: undefined,
    });
    expect(kept.map((p) => p.id)).toEqual(['a', 'b']);
  });

  it('drops points from a foreign tenant (mirror-drift defense)', () => {
    const foreign = pt('x', { [FIELD_TENANT_ID]: 'other-tenant', [FIELD_BRANCH]: 'main' });
    const kept = filterSupplementalPointsToScope([onMain('a'), foreign], {
      tenantId: TENANT,
      branch: 'main',
      basePoints: undefined,
    });
    expect(kept.map((p) => p.id)).toEqual(['a']);
  });

  it('restricts to active base_points for multi-clone disambiguation', () => {
    const points = [
      onMain('a', { [FIELD_BASE_POINT]: 'clone-1' }),
      onMain('b', { [FIELD_BASE_POINT]: 'clone-2' }),
      onMain('c'), // no base_point at all
    ];
    const kept = filterSupplementalPointsToScope(points, {
      tenantId: TENANT,
      branch: 'main',
      basePoints: ['clone-1'],
    });
    expect(kept.map((p) => p.id)).toEqual(['a']);
  });

  it('applies no branch/tenant guard when the scope fields are unset', () => {
    const kept = filterSupplementalPointsToScope(
      [pt('a', {}), pt('b', { [FIELD_BRANCH]: 'whatever' })],
      { tenantId: undefined, branch: undefined, basePoints: undefined }
    );
    expect(kept.map((p) => p.id)).toEqual(['a', 'b']);
  });

  it('covers the post-#124 array branch payload (point shared across branches)', () => {
    // Post-#124 the `branch` payload is an ARRAY (one Qdrant point shared across
    // branches). A scalar `!==` against the effective branch is ALWAYS true and
    // would silently drop EVERY supplemental hit on a branch-scoped search.
    const arr = (id: string, branches: string[]) =>
      pt(id, { [FIELD_TENANT_ID]: TENANT, [FIELD_BRANCH]: branches });
    const kept = filterSupplementalPointsToScope(
      [arr('a', ['main']), arr('b', ['feature/x']), arr('c', ['main', 'feature/x'])],
      { tenantId: TENANT, branch: 'main', basePoints: undefined }
    );
    // 'a' is on main; 'c' is shared across main+feature so it still covers main;
    // 'b' is feature-only → dropped. Pre-fix the scalar compare dropped all three.
    expect(kept.map((p) => p.id)).toEqual(['a', 'c']);
  });
});
