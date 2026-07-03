/**
 * Heartbeat self-healing (agent↔MCP session robustness).
 *
 * Guards the three fixes that keep the daemon's session-liveness reaper from
 * deactivating a still-live agent session:
 *  - (A) re-register when the daemon reports the session is gone
 *        (`acknowledged === false`);
 *  - (B) never latch `daemonConnected` off — a successful heartbeat recovers it,
 *        and a failure does not stop future attempts;
 *  - (C) derive the cadence from the daemon's `next_heartbeat_by` deadline.
 */

import { describe, it, expect, vi } from 'vitest';

import {
  sendHeartbeat,
  nextHeartbeatDelayMs,
} from '../../src/session-lifecycle.js';
import { HEARTBEAT_INTERVAL_MS } from '../../src/server-types.js';
import type { SessionState } from '../../src/server-types.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';

function makeSession(overrides: Partial<SessionState> = {}): SessionState {
  return {
    sessionId: 'sess-1',
    projectId: 'tenant-abc',
    projectPath: '/tmp/not-a-repo',
    selfRepoProjectId: null,
    daemonConnected: true,
    heartbeatInterval: null,
    ...overrides,
  } as unknown as SessionState;
}

describe('nextHeartbeatDelayMs (C — server-driven cadence)', () => {
  const now = 1_000_000_000_000;
  it('aims for ~1/3 of the remaining window', () => {
    vi.spyOn(Date, 'now').mockReturnValue(now);
    // 90s deadline → ~30s (== HEARTBEAT_INTERVAL_MS ceiling here).
    const ms = nextHeartbeatDelayMs({
      next_heartbeat_by: { seconds: Math.floor(now / 1000) + 90, nanos: 0 },
    });
    expect(ms).toBe(30_000);
    vi.restoreAllMocks();
  });

  it('clamps below the interval ceiling and above the 5s floor', () => {
    vi.spyOn(Date, 'now').mockReturnValue(now);
    // 300s deadline → 1/3 = 100s, capped at the 30s ceiling.
    expect(
      nextHeartbeatDelayMs({ next_heartbeat_by: { seconds: Math.floor(now / 1000) + 300, nanos: 0 } })
    ).toBe(HEARTBEAT_INTERVAL_MS);
    // 6s deadline → 1/3 = 2s, raised to the 5s floor.
    expect(
      nextHeartbeatDelayMs({ next_heartbeat_by: { seconds: Math.floor(now / 1000) + 6, nanos: 0 } })
    ).toBe(5_000);
    vi.restoreAllMocks();
  });

  it('falls back to the fixed interval when the deadline is missing or passed', () => {
    vi.spyOn(Date, 'now').mockReturnValue(now);
    expect(nextHeartbeatDelayMs({})).toBe(HEARTBEAT_INTERVAL_MS);
    expect(
      nextHeartbeatDelayMs({ next_heartbeat_by: { seconds: Math.floor(now / 1000) - 5, nanos: 0 } })
    ).toBe(HEARTBEAT_INTERVAL_MS);
    vi.restoreAllMocks();
  });
});

describe('sendHeartbeat self-healing', () => {
  it('(B) recovers daemonConnected on a successful heartbeat and returns a delay', async () => {
    const session = makeSession({ daemonConnected: false });
    const heartbeat = vi.fn().mockResolvedValue({ acknowledged: true });
    const client = { heartbeat } as unknown as DaemonClient;

    const next = await sendHeartbeat(session, client);

    expect(heartbeat).toHaveBeenCalledOnce(); // did NOT early-return on daemonConnected=false
    expect(session.daemonConnected).toBe(true); // flag recovered
    expect(typeof next).toBe('number');
    expect(next).toBeGreaterThan(0);
  });

  it('(A) re-registers when the daemon reports no live session', async () => {
    const session = makeSession();
    const heartbeat = vi.fn().mockResolvedValue({ acknowledged: false });
    const registerProject = vi.fn().mockResolvedValue({
      created: false,
      project_id: 'tenant-abc',
      priority: 'high',
      is_active: true,
      newly_registered: false,
    });
    const client = { heartbeat, registerProject } as unknown as DaemonClient;

    await sendHeartbeat(session, client);

    expect(heartbeat).toHaveBeenCalled();
    expect(registerProject).toHaveBeenCalled(); // re-registered via registerProject()
  });

  it('(B) does not throw and does not latch off permanently on failure', async () => {
    const session = makeSession();
    const heartbeat = vi.fn().mockRejectedValue(new Error('UNAVAILABLE'));
    const client = { heartbeat } as unknown as DaemonClient;

    const next = await sendHeartbeat(session, client);

    expect(next).toBe(HEARTBEAT_INTERVAL_MS); // still schedules the next attempt
    expect(session.daemonConnected).toBe(false);
    // The next successful beat (new client) must recover the flag — no permanent latch.
    const ok = vi.fn().mockResolvedValue({ acknowledged: true });
    await sendHeartbeat(session, { heartbeat: ok } as unknown as DaemonClient);
    expect(session.daemonConnected).toBe(true);
  });
});
