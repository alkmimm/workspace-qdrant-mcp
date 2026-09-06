/**
 * Qdrant request-timeout resolution.
 *
 * Every tool used to carry its own `?? 5000`. That default is under the floor
 * for a burst: with the daemon saturating Qdrant after a branch switch
 * (folder-scan storm + branch_reconcile), nine parallel MCP calls produced
 * four hard failures — search/list/retrieve aborting with the opaque "This
 * operation was aborted" while a direct scroll measured 0.27s (2026-09-05).
 * Resolution now lives in the factory so a deployment can raise it in ONE
 * place and every surface moves together.
 */

import { describe, it, expect, afterEach } from 'vitest';

import {
  DEFAULT_QDRANT_TIMEOUT_MS,
  QDRANT_TIMEOUT_ENV_VAR,
  resolveQdrantTimeoutMs,
} from '../../src/clients/qdrant-client-factory.js';

afterEach(() => {
  delete process.env[QDRANT_TIMEOUT_ENV_VAR];
});

describe('resolveQdrantTimeoutMs', () => {
  it('defaults well above the burst floor that produced the aborts', () => {
    // Pinned as a floor, not an exact value: the point is that 5s is gone.
    expect(resolveQdrantTimeoutMs()).toBe(DEFAULT_QDRANT_TIMEOUT_MS);
    expect(DEFAULT_QDRANT_TIMEOUT_MS).toBeGreaterThanOrEqual(15000);
  });

  it('honors the env override', () => {
    process.env[QDRANT_TIMEOUT_ENV_VAR] = '45000';
    expect(resolveQdrantTimeoutMs()).toBe(45000);
  });

  it('lets an explicit argument win over the env', () => {
    process.env[QDRANT_TIMEOUT_ENV_VAR] = '45000';
    expect(resolveQdrantTimeoutMs(1234)).toBe(1234);
  });

  it('falls through a non-numeric or non-positive env instead of disabling the timeout', () => {
    for (const bad of ['', 'soon', '0', '-1', 'NaN']) {
      process.env[QDRANT_TIMEOUT_ENV_VAR] = bad;
      expect(resolveQdrantTimeoutMs(), `env=${JSON.stringify(bad)}`).toBe(
        DEFAULT_QDRANT_TIMEOUT_MS
      );
    }
  });

  it('ignores a non-positive explicit value the same way', () => {
    expect(resolveQdrantTimeoutMs(0)).toBe(DEFAULT_QDRANT_TIMEOUT_MS);
    expect(resolveQdrantTimeoutMs(Number.NaN)).toBe(DEFAULT_QDRANT_TIMEOUT_MS);
  });
});
