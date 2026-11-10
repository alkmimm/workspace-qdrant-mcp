/**
 * Daemon availability check with TTL-based caching.
 *
 * Verifies daemon connectivity before write operations, with TTL-based caching.
 * Positive results cached for 5s, negative results cached for 1s to allow fast recovery.
 */

import type { DaemonClient } from '../clients/daemon-client.js';
import { recordCacheLookup } from '../telemetry/metrics.js';

let lastCheck: { available: boolean; timestamp: number } | null = null;
const POSITIVE_TTL_MS = 5000;
const NEGATIVE_TTL_MS = 1000;

/**
 * Ensure the daemon is available for write operations.
 *
 * Returns the validated DaemonClient or throws if unavailable.
 * Caches positive results for 5s, negative for 1s to reduce health check overhead.
 */
export async function ensureDaemonAvailable(client: DaemonClient | null): Promise<DaemonClient> {
  if (!client) {
    throw new Error('Daemon unavailable. Cannot process write operation.');
  }
  const now = Date.now();
  const ttl = lastCheck?.available === true ? POSITIVE_TTL_MS : NEGATIVE_TTL_MS;
  const hit = lastCheck !== null && now - lastCheck.timestamp < ttl;
  recordCacheLookup('daemon_check', hit);
  if (lastCheck && hit) {
    if (!lastCheck.available) {
      throw new Error('Daemon unavailable. Cannot process write operation.');
    }
    return client;
  }
  try {
    await client.healthCheck();
    lastCheck = { available: true, timestamp: now };
    return client;
  } catch {
    lastCheck = { available: false, timestamp: now };
    throw new Error('Daemon unavailable. Cannot process write operation.');
  }
}

/** Reset the cached check (useful for testing). */
export function resetDaemonCheck(): void {
  lastCheck = null;
}
