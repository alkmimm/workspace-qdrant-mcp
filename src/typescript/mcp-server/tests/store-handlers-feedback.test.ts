/**
 * Tests for storeFeedback (store type:"feedback").
 *
 * Feedback is a scratchpad-family note written to a dedicated synthetic tenant
 * (TENANT_FEEDBACK) so it aggregates in one bucket and stays isolated from every
 * project-scoped read surface. It requires a `category` and records the optional
 * `refTool`; both are mirrored into tags so /feedback-review can group without a
 * payload re-read. Unlike scratchpad it ignores projectId/cwd for tenanting.
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
  it('records a valid feedback note to the dedicated feedback tenant', async () => {
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
    expect(payload['source_type']).toBe('feedback');
    expect(payload['category']).toBe('win');
    expect(payload['ref_tool']).toBe('grep');
    expect(payload['tags']).toEqual(
      expect.arrayContaining(['feedback', 'category:win', 'tool:grep'])
    );
    expect(payload['content']).toContain('round trip');
  });

  it('ignores projectId — feedback always aggregates in the one bucket', async () => {
    const sm = mockStateManager();
    await storeFeedback({ content: 'x', category: 'friction', projectId: 'some-project' }, sm, session());
    expect(arg(sm, 2)).toBe(TENANT_FEEDBACK);
  });

  it('omits ref_tool / tool tag when refTool is not given', async () => {
    const sm = mockStateManager();
    await storeFeedback({ content: 'x', category: 'missing-rule' }, sm, session());
    const payload = arg(sm, 4) as Record<string, unknown>;
    expect(payload['ref_tool']).toBeUndefined();
    expect(payload['tags']).toEqual(['feedback', 'category:missing-rule']);
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

  it('accepts every declared category', async () => {
    for (const category of FEEDBACK_CATEGORIES) {
      const sm = mockStateManager();
      const res = await storeFeedback({ content: 'x', category }, sm, session());
      expect(res.success).toBe(true);
      expect((arg(sm, 4) as Record<string, unknown>)['category']).toBe(category);
    }
  });
});
