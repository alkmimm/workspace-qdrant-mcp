/**
 * Lazy client-project activation (`ensureClientProjectActive`).
 *
 * Over HTTP the session bootstrap runs before any tool call and cannot see the
 * client's cwd, so a connecting client's project (DOC-V2, bws-engineer, …) never
 * became `is_active` — only the wqm self-repo did. These tests guard the fix:
 * the FIRST tool call carries the client cwd (via the request context), and we
 * lazily resolve + register that project as this session's live project so the
 * existing heartbeat/cleanup machinery keeps it active and tears it down.
 */

import { describe, it, expect, vi } from 'vitest';

import { ensureClientProjectActive } from '../../src/session-lifecycle.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';
import type { SessionState } from '../../src/server-types.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { ProjectDetector, ProjectInfo } from '../../src/utils/project-detector.js';

function makeSession(overrides: Partial<SessionState> = {}): SessionState {
  return {
    sessionId: 'sess-1',
    projectId: null,
    projectPath: null,
    lastHostCwd: null,
    activatedForCwd: null,
    activatingCwd: null,
    watchPath: null,
    isWorktree: false,
    selfRepoProjectId: null,
    currentBranch: null,
    lastBranchRefreshAt: 0,
    heartbeatInterval: null,
    daemonConnected: true,
    cleaned: false,
    ...overrides,
  } as unknown as SessionState;
}

function projectInfo(projectId: string, projectPath: string): ProjectInfo {
  return { projectId, projectPath, isActive: false };
}

/** DaemonClient double capturing register/deprioritize calls. */
function makeDaemon(): {
  client: DaemonClient;
  registerProject: ReturnType<typeof vi.fn>;
  deprioritizeProject: ReturnType<typeof vi.fn>;
} {
  const registerProject = vi.fn().mockResolvedValue({
    created: false,
    project_id: 'ignored',
    priority: 'high',
    is_active: true,
    newly_registered: false,
  });
  const deprioritizeProject = vi.fn().mockResolvedValue({
    success: true,
    is_active: false,
    new_priority: 'normal',
  });
  return {
    client: { registerProject, deprioritizeProject } as unknown as DaemonClient,
    registerProject,
    deprioritizeProject,
  };
}

/** ProjectDetector double whose getProjectInfo returns a scripted result. */
function makeDetector(getProjectInfo: ReturnType<typeof vi.fn>): ProjectDetector {
  return { getProjectInfo } as unknown as ProjectDetector;
}

function run(cwd: string, fn: () => Promise<void>): Promise<void> {
  return runWithRequestContext({ hostCwd: cwd }, fn);
}

describe('ensureClientProjectActive', () => {
  it('activates the resolved client project as this session (first tool call)', async () => {
    const session = makeSession();
    const { client, registerProject } = makeDaemon();
    const getProjectInfo = vi.fn().mockResolvedValue(projectInfo('tenant-doc', '/repos/DOC-V2'));

    await run('/repos/DOC-V2', () =>
      ensureClientProjectActive(session, client, makeDetector(getProjectInfo))
    );

    expect(getProjectInfo).toHaveBeenCalledWith('/repos/DOC-V2', false, {
      fallbackToSoleProject: true,
    });
    expect(session.projectId).toBe('tenant-doc');
    expect(session.projectPath).toBe('/repos/DOC-V2');
    expect(session.activatedForCwd).toBe('/repos/DOC-V2');
    // Registered as a live session (session_id → is_active on the daemon).
    expect(registerProject).toHaveBeenCalledOnce();
    expect(registerProject).toHaveBeenCalledWith(
      expect.objectContaining({
        project_id: 'tenant-doc',
        session_id: 'sess-1',
        priority: 'high',
      })
    );
  });

  it('is a no-op on the next call for the same cwd (does not re-resolve or re-register)', async () => {
    const session = makeSession();
    const { client, registerProject } = makeDaemon();
    const getProjectInfo = vi.fn().mockResolvedValue(projectInfo('tenant-doc', '/repos/DOC-V2'));
    const detector = makeDetector(getProjectInfo);

    await run('/repos/DOC-V2', () => ensureClientProjectActive(session, client, detector));
    await run('/repos/DOC-V2', () => ensureClientProjectActive(session, client, detector));

    expect(getProjectInfo).toHaveBeenCalledOnce(); // short-circuited on the 2nd call
    expect(registerProject).toHaveBeenCalledOnce();
  });

  it('does not activate when the cwd maps to no registered project, and retries later', async () => {
    const session = makeSession();
    const { client, registerProject } = makeDaemon();
    const getProjectInfo = vi.fn().mockResolvedValue(null);
    const detector = makeDetector(getProjectInfo);

    await run('/repos/unknown', () => ensureClientProjectActive(session, client, detector));

    expect(session.projectId).toBeNull();
    expect(session.activatedForCwd).toBeNull(); // not marked → retry allowed
    expect(registerProject).not.toHaveBeenCalled();

    // A later call re-resolves (the project may have since registered).
    await run('/repos/unknown', () => ensureClientProjectActive(session, client, detector));
    expect(getProjectInfo).toHaveBeenCalledTimes(2);
  });

  it('deprioritizes the previous project on a mid-session cwd switch (no is_active leak)', async () => {
    const session = makeSession();
    const { client, registerProject, deprioritizeProject } = makeDaemon();
    const getProjectInfo = vi
      .fn()
      .mockResolvedValueOnce(projectInfo('tenant-a', '/repos/a'))
      .mockResolvedValueOnce(projectInfo('tenant-b', '/repos/b'));
    const detector = makeDetector(getProjectInfo);

    await run('/repos/a', () => ensureClientProjectActive(session, client, detector));
    expect(session.projectId).toBe('tenant-a');

    await run('/repos/b', () => ensureClientProjectActive(session, client, detector));

    // Old project torn down (same session_id) BEFORE the new one is registered.
    expect(deprioritizeProject).toHaveBeenCalledWith(
      expect.objectContaining({ project_id: 'tenant-a', session_id: 'sess-1' })
    );
    expect(session.projectId).toBe('tenant-b');
    expect(session.activatedForCwd).toBe('/repos/b');
    expect(registerProject).toHaveBeenCalledTimes(2);
  });

  it('points the session at the new project BEFORE deprioritizing the old one (leak-free switch)', async () => {
    // Guards the switch ordering: sessionState.projectId must already be the NEW
    // project when the old one is deprioritized, so a heartbeat interleaving the
    // deprioritize/register awaits re-registers the new project (via its
    // acknowledged:false self-heal) instead of resurrecting the old one.
    const session = makeSession();
    const registerProject = vi.fn().mockResolvedValue({
      created: false,
      project_id: 'ignored',
      priority: 'high',
      is_active: true,
      newly_registered: false,
    });
    let projectIdAtDeprioritize: string | null = 'UNSET';
    const deprioritizeProject = vi.fn().mockImplementation(() => {
      projectIdAtDeprioritize = session.projectId;
      return Promise.resolve({ success: true, is_active: false, new_priority: 'normal' });
    });
    const client = { registerProject, deprioritizeProject } as unknown as DaemonClient;
    const getProjectInfo = vi
      .fn()
      .mockResolvedValueOnce(projectInfo('tenant-a', '/repos/a'))
      .mockResolvedValueOnce(projectInfo('tenant-b', '/repos/b'));
    const detector = makeDetector(getProjectInfo);

    await run('/repos/a', () => ensureClientProjectActive(session, client, detector));
    await run('/repos/b', () => ensureClientProjectActive(session, client, detector));

    expect(deprioritizeProject).toHaveBeenCalledWith(
      expect.objectContaining({ project_id: 'tenant-a' })
    );
    expect(projectIdAtDeprioritize).toBe('tenant-b'); // swapped BEFORE the teardown
  });

  it('does not deprioritize/re-register when a new cwd resolves to the same project', async () => {
    const session = makeSession();
    const { client, registerProject, deprioritizeProject } = makeDaemon();
    const getProjectInfo = vi
      .fn()
      .mockResolvedValueOnce(projectInfo('tenant-a', '/repos/a'))
      .mockResolvedValueOnce(projectInfo('tenant-a', '/repos/a'));
    const detector = makeDetector(getProjectInfo);

    await run('/repos/a', () => ensureClientProjectActive(session, client, detector));
    await run('/repos/a/src/deep', () => ensureClientProjectActive(session, client, detector));

    expect(deprioritizeProject).not.toHaveBeenCalled();
    expect(registerProject).toHaveBeenCalledOnce();
    expect(session.projectId).toBe('tenant-a');
    expect(session.activatedForCwd).toBe('/repos/a/src/deep');
  });

  it('skips a suspicious cwd (system dir / container WORKDIR leak) without resolving', async () => {
    const session = makeSession();
    const { client, registerProject } = makeDaemon();
    const getProjectInfo = vi.fn();

    await run('/', () => ensureClientProjectActive(session, client, makeDetector(getProjectInfo)));

    expect(getProjectInfo).not.toHaveBeenCalled();
    expect(registerProject).not.toHaveBeenCalled();
    expect(session.projectId).toBeNull();
  });

  it('skips entirely when the daemon is not connected (retried once connected)', async () => {
    const session = makeSession({ daemonConnected: false });
    const { client, registerProject } = makeDaemon();
    const getProjectInfo = vi.fn().mockResolvedValue(projectInfo('tenant-doc', '/repos/DOC-V2'));

    await run('/repos/DOC-V2', () =>
      ensureClientProjectActive(session, client, makeDetector(getProjectInfo))
    );

    expect(getProjectInfo).not.toHaveBeenCalled();
    expect(registerProject).not.toHaveBeenCalled();
    expect(session.activatedForCwd).toBeNull();
  });

  it('never throws and clears the in-flight guard when resolution fails', async () => {
    const session = makeSession();
    const { client, registerProject } = makeDaemon();
    const getProjectInfo = vi.fn().mockRejectedValue(new Error('SQLITE_CANTOPEN'));

    await expect(
      run('/repos/DOC-V2', () =>
        ensureClientProjectActive(session, client, makeDetector(getProjectInfo))
      )
    ).resolves.toBeUndefined();

    expect(registerProject).not.toHaveBeenCalled();
    expect(session.activatingCwd).toBeNull(); // finally cleared it
    expect(session.activatedForCwd).toBeNull(); // retry allowed
  });
});
