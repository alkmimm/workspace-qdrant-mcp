/**
 * Tests for storeFeedback (store type:"feedback").
 *
 * Feedback is a scratchpad-family note written to a dedicated synthetic tenant
 * (TENANT_FEEDBACK) so it aggregates in one bucket and stays isolated from every
 * project-scoped read surface. `category` and `refTool` are recorded as TAGS
 * (`category:<c>` / `tool:<t>`) — NOT dedicated payload fields — because the
 * daemon's scratchpad write drops unknown fields and hardcodes source_type; the
 * tags are what /feedback-review groups on. Feedback ignores projectId/cwd for
 * tenanting and writes the advisory scratchpad mirror like storeScratchpad.
 */

import { describe, it, expect, vi } from 'vitest';
import { storeFeedback, FEEDBACK_CATEGORIES } from '../src/store-handlers.js';
import type { SqliteStateManager } from '../src/clients/sqlite-state-manager.js';
import { TENANT_FEEDBACK } from '../src/constants/tenants.js';

function mockStateManager(): SqliteStateManager {
  return {
    enqueueUnified: vi.fn().mockResolvedValue({ status: 'ok', data: { queueId: 'q-1' } }),
    upsertScratchpadMirror: vi.fn(),
  } as unknown as SqliteStateManager;
}

/** Minimal session slice storeFeedback consumes (git provenance only). */
function session() {
  return { projectId: null, currentBranch: null, isWorktree: false };
}

/** Positional arg `i` of the (single) enqueueUnified call. */
function arg(sm: SqliteStateManager, i: number): unknown {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock.calls[0][i];
}

describe('storeFeedback', () => {
  it('records feedback to the dedicated tenant, with category/tool in TAGS (not payload fields)', async () => {
    const sm = mockStateManager();

    const res = await storeFeedback(
      { content: 'the grep truncation warning saved me a round trip', category: 'win', refTool: 'grep' },
      sm,
      session()
    );

    expect(res.success).toBe(true);
    expect(arg(sm, 2)).toBe(TENANT_FEEDBACK); // tenant_id (3rd positional)
    expect(res.collection).toBe(arg(sm, 3)); // enqueued to the scratchpad collection
    const payload = arg(sm, 4) as Record<string, unknown>;
    // category / refTool / source_type live in TAGS, not as payload fields — the
    // daemon drops unknown fields and hardcodes source_type, so persisting them as
    // fields would be dead weight.
    expect(payload['category']).toBeUndefined();
    expect(payload['ref_tool']).toBeUndefined();
    expect(payload['source_type']).toBeUndefined();
    expect(payload['tags']).toEqual(['feedback', 'category:win', 'tool:grep']);
    expect(payload['content']).toContain('round trip');
    // Advisory mirror is written to the same feedback tenant (rebuild recovery).
    expect(sm.upsertScratchpadMirror).toHaveBeenCalledWith(
      expect.objectContaining({ tenantId: TENANT_FEEDBACK })
    );
  });

  it('ignores projectId — feedback always aggregates in the one bucket', async () => {
    const sm = mockStateManager();
    await storeFeedback({ content: 'x', category: 'friction', projectId: 'some-project' }, sm, session());
    expect(arg(sm, 2)).toBe(TENANT_FEEDBACK);
  });

  it('omits the tool tag when refTool is not given', async () => {
    const sm = mockStateManager();
    await storeFeedback({ content: 'x', category: 'missing-rule' }, sm, session());
    expect((arg(sm, 4) as Record<string, unknown>)['tags']).toEqual([
      'feedback',
      'category:missing-rule',
    ]);
  });

  it('rejects missing content without enqueuing', async () => {
    const sm = mockStateManager();
    const res = await storeFeedback({ category: 'win' }, sm, session());
    expect(res.success).toBe(false);
    expect(res.message).toMatch(/content is required/);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('rejects a missing or invalid category without enqueuing', async () => {
    const sm = mockStateManager();
    const missing = await storeFeedback({ content: 'x' }, sm, session());
    expect(missing.success).toBe(false);
    expect(missing.message).toMatch(/category is required/);
    const invalid = await storeFeedback({ content: 'x', category: 'bogus' }, sm, session());
    expect(invalid.success).toBe(false);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('accepts every declared category (recorded in the category tag)', async () => {
    for (const category of FEEDBACK_CATEGORIES) {
      const sm = mockStateManager();
      const res = await storeFeedback({ content: 'x', category }, sm, session());
      expect(res.success).toBe(true);
      expect((arg(sm, 4) as Record<string, unknown>)['tags']).toContain(`category:${category}`);
    }
  });
});
