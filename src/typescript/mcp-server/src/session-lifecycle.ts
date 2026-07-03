/**
 * Session lifecycle management: initialization, project registration,
 * heartbeat, and cleanup.
 */

import { randomUUID } from 'node:crypto';
import { basename } from 'node:path';

import type { DaemonClient } from './clients/daemon-client.js';
import type { RegisterProjectResponse } from './clients/grpc-types.js';
import type { SqliteStateManager } from './clients/sqlite-state-manager.js';
import type { ProjectDetector } from './utils/project-detector.js';
import { getGitRemoteUrl } from './utils/project-detector.js';
import { getEffectiveCwd } from './utils/request-context.js';
import { findGitRoot, getCurrentBranch, isWorktree } from './utils/git-utils.js';
import type { HealthMonitor } from './utils/health-monitor.js';
import { logInfo, logError, logDebug, logSessionEvent, logDaemonStatus } from './utils/logger.js';
import { recordDaemonFallback } from './telemetry/metrics.js';
import { HEARTBEAT_INTERVAL_MS } from './server-types.js';
import type { SessionState } from './server-types.js';

/**
 * Initialize session: detect project, connect daemon, auto-register if needed,
 * then start heartbeat.
 * Mutates sessionState in place.
 */
/**
 * Paths that must NEVER be auto-registered as a project, even if they
 * survive the project-marker walk. These are common defaults that processes
 * inherit when launched by GUIs, services, or installers and they would
 * pollute the daemon's watch list with system directories.
 */
const SUSPICIOUS_CWD_PATTERNS: ReadonlyArray<RegExp> = [
  /^[A-Za-z]:[\\/]+WINDOWS([\\/]|$)/i, // C:\WINDOWS\..., C:/Windows/...
  /^[A-Za-z]:[\\/]+Program Files([\\/]|$)/i,
  /^[A-Za-z]:[\\/]+ProgramData([\\/]|$)/i,
  /^[A-Za-z]:[\\/]*$/i, // bare drive root: C:\, C:/, D:\, ...
  /^\/$/, // POSIX root
  // POSIX system dirs: only the directory ITSELF (optional trailing slash),
  // not its descendants. The audit's Bug-3 failure mode is the project walk
  // landing ON a system root (cwd=/tmp inherited from a service launcher,
  // or walking up to /), and that stays blocked. Real marker-bearing
  // projects legitimately live BENEATH these paths — /tmp/<scratch
  // checkout>, /var/www/<site>, /usr/local/src/<repo> — and registration
  // still requires an actual project marker, which bare system dirs lack.
  /^\/(usr|bin|sbin|etc|var|tmp|root|System|Library)\/*$/i,
];

function isSuspiciousCwd(path: string): boolean {
  return SUSPICIOUS_CWD_PATTERNS.some((re) => re.test(path));
}

/** Detect and assign project info to session state. */
async function detectProjectForSession(
  sessionState: SessionState,
  projectDetector: ProjectDetector
): Promise<void> {
  // Allow explicit override for service-style launches (GUI, launchd, systemd).
  const envOverride = process.env['WQM_PROJECT_ROOT'];
  const cwd = getEffectiveCwd();
  const startPath = envOverride ?? cwd;

  // If the search base itself is a system directory, do not even attempt
  // to walk up — anything we'd find is wrong. This avoids the
  // `C:\WINDOWS\system32` → `C:\` failure mode (Bug 3 in audit).
  if (!envOverride && isSuspiciousCwd(cwd)) {
    logDebug('Skipping project detection: cwd is a system directory', { cwd });
    return;
  }

  const foundRoot = projectDetector.findProjectRoot(startPath);
  if (!foundRoot) {
    logDebug('No project marker found; not registering', { startPath });
    return;
  }
  if (isSuspiciousCwd(foundRoot)) {
    logDebug('Found project root is suspicious; refusing to register', {
      startPath,
      foundRoot,
    });
    return;
  }

  const projectInfo = await projectDetector.getProjectInfo(foundRoot, false);
  if (projectInfo) {
    sessionState.projectPath = projectInfo.projectPath;
    sessionState.projectId = projectInfo.projectId;
    logDebug('Project detected', {
      project_path: projectInfo.projectPath,
      project_id: projectInfo.projectId,
    });
  } else {
    sessionState.projectPath = foundRoot;
    logDebug('Project root detected but not registered', { projectRoot: foundRoot, cwd });
  }

  // Capture initial git state. Best-effort: silently skip if git is
  // unavailable or path is not a repo. Refreshed lazily by
  // `ensureProjectFresh` on every tool call.
  if (sessionState.projectPath) {
    refreshGitState(sessionState);
  }
}

/**
 * Time-to-live for cached git state. After this many milliseconds since the
 * last refresh, the next tool call will re-read `branch` and `isWorktree`
 * from `git`. Short enough that branch switches show up quickly; long enough
 * that a burst of tool calls doesn't cost a `git` invocation each.
 */
const BRANCH_FRESHNESS_MS = 5_000;

function refreshGitState(sessionState: SessionState): void {
  if (!sessionState.projectPath) return;
  try {
    const branch = getCurrentBranch(sessionState.projectPath);
    if (branch !== null) sessionState.currentBranch = branch;
    sessionState.isWorktree = isWorktree(sessionState.projectPath);
    sessionState.lastBranchRefreshAt = Date.now();
  } catch {
    // Silent: best-effort enrichment.
  }
}

/**
 * Refresh the cached git state (branch + worktree flag) if it has aged out.
 *
 * Called by the tool dispatcher before each tool call so search/grep that
 * depend on the current branch see fresh values without re-detecting on
 * every call. Cheap when within TTL — single `Date.now()` comparison.
 */
export function ensureProjectFresh(sessionState: SessionState): void {
  if (!sessionState.projectPath) return;
  const age = Date.now() - sessionState.lastBranchRefreshAt;
  if (age < BRANCH_FRESHNESS_MS) return;
  refreshGitState(sessionState);
}

export async function initializeSession(
  sessionState: SessionState,
  daemonClient: DaemonClient,
  projectDetector: ProjectDetector,
  startHeartbeatFn: () => void
): Promise<void> {
  sessionState.sessionId = randomUUID();
  const { setSessionId } = await import('./utils/logger.js');
  setSessionId(sessionState.sessionId);
  logSessionEvent('start', { session_id: sessionState.sessionId });

  await detectProjectForSession(sessionState, projectDetector);

  try {
    await daemonClient.connect();
    sessionState.daemonConnected = true;
    logDaemonStatus(true);
    if (sessionState.projectPath) await registerProject(sessionState, daemonClient);
    // Keep this checkout registered for LSP regardless of the client's project
    // (cwd-detection can't find the self-repo in the container — see fn docs).
    await ensureSelfRepoRegistered(daemonClient, sessionState);
    startHeartbeatFn();
  } catch (error) {
    sessionState.daemonConnected = false;
    logDaemonStatus(false, { reason: 'connection_failed' });
    logError('Daemon connection error', error);
    recordDaemonFallback('session', 'connection_failed');
  }
}

/**
 * Register/activate the current project with the daemon.
 *
 * First tries to re-activate an existing project (`register_if_new: false`).
 * If the current path is unknown to the daemon, falls back to
 * `register_if_new: true` so fresh projects and worktrees are registered
 * automatically during session startup.
 */
/** Apply daemon registration response to session state. */
function applyRegistrationResponse(
  sessionState: SessionState,
  response: { project_id: string; is_worktree?: boolean; watch_path?: string }
): void {
  if (response.project_id && !sessionState.projectId) {
    sessionState.projectId = response.project_id;
    logDebug('Project ID assigned by daemon', { project_id: response.project_id });
  }
  if (response.is_worktree) {
    sessionState.isWorktree = true;
    sessionState.watchPath = response.watch_path ?? sessionState.projectPath ?? null;
    logInfo('Registered as worktree', {
      project_path: sessionState.projectPath,
      project_id: response.project_id,
      watch_path: sessionState.watchPath,
    });
  }
}

export async function registerProject(
  sessionState: SessionState,
  daemonClient: DaemonClient
): Promise<void> {
  if (!sessionState.projectPath) return;

  try {
    const gitRemote = getGitRemoteUrl(sessionState.projectPath);
    const projectName = basename(sessionState.projectPath) || 'unknown';
    const unknownProject = sessionState.projectId === null;

    let response = await callRegisterProject(
      daemonClient,
      sessionState.projectPath,
      projectName,
      gitRemote,
      sessionState.projectId ?? '',
      false,
      sessionState.sessionId ?? '',
      false
    );

    if (!response.created && !response.is_active && unknownProject) {
      logInfo('Project not registered yet; auto-registering on session start', {
        project_path: sessionState.projectPath,
        project_id: response.project_id,
      });
      response = await callRegisterProject(
        daemonClient,
        sessionState.projectPath,
        projectName,
        gitRemote,
        '',
        true,
        sessionState.sessionId ?? '',
        false
      );
    }

    if (!response.is_active && !response.created) {
      logInfo('Project not registered with daemon, skipping activation', {
        project_path: sessionState.projectPath,
        project_id: response.project_id,
      });
      return;
    }

    applyRegistrationResponse(sessionState, response);
    logSessionEvent('register', {
      project_id: response.project_id,
      project_path: sessionState.projectPath,
      created: response.created,
      priority: response.priority,
      is_active: response.is_active,
      newly_registered: response.newly_registered,
      is_worktree: response.is_worktree,
      watch_path: response.watch_path,
    });
  } catch (error) {
    logError('Failed to register project', error, { project_path: sessionState.projectPath });
  }
}

/**
 * Register a project via the store tool (type: "project").
 *
 * Unlike session-start registration, this explicitly registers new projects
 * with `register_if_new: true`.
 */
/** Call the daemon to register a project and log the event. */
async function callRegisterProject(
  daemonClient: DaemonClient,
  resolvedPath: string,
  name: string,
  gitRemote: string | null,
  projectId: string,
  registerIfNew: boolean,
  sessionId: string,
  logEvent = true
): Promise<RegisterProjectResponse> {
  const response = await daemonClient.registerProject({
    path: resolvedPath,
    project_id: projectId,
    name,
    register_if_new: registerIfNew,
    priority: 'high',
    session_id: sessionId,
    ...(gitRemote ? { git_remote: gitRemote } : {}),
  });
  if (logEvent && (response.created || response.is_active || registerIfNew)) {
    logSessionEvent('register', {
      project_id: response.project_id,
      project_path: resolvedPath,
      created: response.created,
      priority: response.priority,
      is_active: response.is_active,
      newly_registered: response.newly_registered,
      register_if_new: registerIfNew,
    });
  }
  return response;
}

/**
 * Ensure the workspace-qdrant-mcp checkout itself is registered with the daemon
 * so ITS LSP servers (rust-analyzer, typescript-language-server, …) come up.
 *
 * cwd-based detection can't find this repo: the containerized MCP runs from
 * /app and `WQM_PROJECT_ROOT` points at the bind-mounted *dev root* (the parent
 * of all repos, which has no project marker), so `detectProjectForSession`
 * never registers this checkout — leaving Rust/TS grey on the LSP dashboard
 * after a (re)start. `WQM_REPO_DIR` is the exact container path of the checkout,
 * so register it explicitly. Idempotent (`register_if_new`) and best-effort: a
 * failure here must never break session startup. Records the self-repo tenant
 * on `sessionState` (`selfRepoProjectId`) so the heartbeat loop can keep its
 * "self-repo" session alive; otherwise independent of whatever project the
 * connecting client is in.
 */
export async function ensureSelfRepoRegistered(
  daemonClient: DaemonClient,
  sessionState: SessionState
): Promise<void> {
  const repoDir = process.env['WQM_REPO_DIR'];
  if (!repoDir) return;
  try {
    const resolvedPath = findGitRoot(repoDir) ?? repoDir;
    const gitRemote = getGitRemoteUrl(resolvedPath);
    const response = await callRegisterProject(
      daemonClient,
      resolvedPath,
      basename(resolvedPath) || 'workspace-qdrant-mcp',
      gitRemote,
      '',
      true, // register_if_new
      'self-repo', // stable session id — idempotent across restarts (no leak)
      false // housekeeping call — don't emit a session 'register' event
    );
    // Remember the self-repo tenant so the heartbeat loop keeps its "self-repo"
    // session alive while this server runs (it expires ~timeout after we stop).
    sessionState.selfRepoProjectId = response.project_id;
    logDebug('Self-repo registered for LSP', {
      repo_dir: resolvedPath,
      project_id: response.project_id,
    });
  } catch (error) {
    logDebug('Self-repo registration skipped', { error: String(error) });
  }
}

export async function registerProjectFromTool(
  args: Record<string, unknown> | undefined,
  sessionState: SessionState,
  daemonClient: DaemonClient
): Promise<{
  success: boolean;
  project_id: string;
  created: boolean;
  is_active: boolean;
  message: string;
}> {
  const path = args?.['path'] as string;
  if (!path) throw new Error('path is required for store type "project"');
  if (!sessionState.daemonConnected)
    throw new Error('Daemon is not connected — cannot register project');

  const resolvedPath = findGitRoot(path) ?? path;
  const name = (args?.['name'] as string) ?? (basename(resolvedPath) || 'unknown');
  const gitRemote = getGitRemoteUrl(resolvedPath);

  const response = await callRegisterProject(
    daemonClient,
    resolvedPath,
    name,
    gitRemote,
    '',
    true,
    sessionState.sessionId ?? '',
    true
  );

  return {
    success: true,
    project_id: response.project_id,
    created: response.newly_registered,
    is_active: response.is_active,
    message: response.newly_registered
      ? `Project registered and activated: ${resolvedPath}`
      : `Project already registered and activated: ${resolvedPath}`,
  };
}

/**
 * Minimum spacing between heartbeats. The daemon's `next_heartbeat_by` deadline
 * drives the cadence (see {@link nextHeartbeatDelayMs}); this floor stops a
 * tight or misconfigured deadline from making the client spin.
 */
const MIN_HEARTBEAT_MS = 5_000;

/**
 * Delay (ms) until the next heartbeat, derived from the daemon-provided
 * `next_heartbeat_by` deadline: aim for ~1/3 of the remaining window so one or
 * two missed heartbeats never cross the daemon's session-liveness reaper
 * timeout. This makes the cadence server-driven — the daemon's reaper timeout
 * dictates how often the client must beat, instead of a constant that has to be
 * kept in sync by hand. Falls back to `HEARTBEAT_INTERVAL_MS` when the deadline
 * is absent/already passed, clamped to `[MIN_HEARTBEAT_MS, HEARTBEAT_INTERVAL_MS]`.
 */
export function nextHeartbeatDelayMs(response: {
  next_heartbeat_by?: { seconds: number; nanos: number };
}): number {
  const dl = response.next_heartbeat_by;
  if (dl && typeof dl.seconds === 'number') {
    const deadlineMs = dl.seconds * 1000 + (dl.nanos ?? 0) / 1e6;
    const remaining = deadlineMs - Date.now();
    if (remaining > 0) {
      const target = Math.floor(remaining / 3);
      return Math.min(HEARTBEAT_INTERVAL_MS, Math.max(MIN_HEARTBEAT_MS, target));
    }
  }
  return HEARTBEAT_INTERVAL_MS;
}

/**
 * Start the heartbeat loop. Mutates sessionState in place.
 *
 * Uses a self-rescheduling timer (not a fixed `setInterval`) so the cadence can
 * follow the daemon's `next_heartbeat_by` deadline. The `.then`/`.catch` chain
 * ALWAYS reschedules, so the heartbeat can never permanently stop — the
 * property the daemon's session-liveness reaper relies on to not deactivate a
 * live session.
 */
export function startHeartbeat(
  sessionState: SessionState,
  sendHeartbeatFn: () => Promise<number>
): void {
  if (sessionState.heartbeatInterval) {
    clearTimeout(sessionState.heartbeatInterval);
  }

  const scheduleNext = (ms: number): void => {
    sessionState.heartbeatInterval = setTimeout(runTick, ms);
  };
  const runTick = (): void => {
    void sendHeartbeatFn()
      .then(scheduleNext)
      .catch(() => scheduleNext(HEARTBEAT_INTERVAL_MS));
  };

  runTick();
  logDebug('Heartbeat started', { max_interval_ms: HEARTBEAT_INTERVAL_MS });
}

/**
 * Send a heartbeat to the daemon and return the delay (ms) until the next one.
 *
 * Self-healing so an agent's session survives a transient daemon blip AND the
 * daemon's session-liveness reaper:
 *  - Does NOT early-return on `daemonConnected`. The daemon client auto-
 *    reconnects per RPC (callWithRetry/ensureConnected); latching the flag off
 *    on the first failure used to stop the heartbeat forever, after which the
 *    reaper would eventually deactivate the still-live session.
 *  - On a successful heartbeat, marks the daemon connected again (recovers the
 *    flag other call sites read).
 *  - When the daemon reports the session is gone (`acknowledged === false` —
 *    after a reap, or a daemon restart that cleared `project_sessions`),
 *    re-registers the project so `is_active` is restored.
 *
 * Never throws; returns the next-heartbeat delay derived from the daemon's
 * `next_heartbeat_by` deadline (falling back to `HEARTBEAT_INTERVAL_MS`).
 */
export async function sendHeartbeat(
  sessionState: SessionState,
  daemonClient: DaemonClient
): Promise<number> {
  let nextMs = HEARTBEAT_INTERVAL_MS;

  // Keep this client session's project alive (when this session has one).
  if (sessionState.projectId) {
    try {
      const response = await daemonClient.heartbeat({
        project_id: sessionState.projectId,
        ...(sessionState.sessionId ? { session_id: sessionState.sessionId } : {}),
      });
      // The RPC succeeded (the client reconnected if needed) — recover the flag.
      sessionState.daemonConnected = true;
      nextMs = nextHeartbeatDelayMs(response);

      if (response.acknowledged) {
        logSessionEvent('heartbeat', {
          project_id: sessionState.projectId,
          acknowledged: true,
        });
      } else {
        // The daemon no longer knows this session (reaped, or cleared on a
        // daemon restart). Re-register so is_active is restored instead of the
        // project silently losing queue priority + LSP mid-session.
        logInfo('Heartbeat found no live session; re-registering', {
          project_id: sessionState.projectId,
        });
        await registerProject(sessionState, daemonClient);
      }
    } catch (error) {
      logError('Heartbeat failed', error, { project_id: sessionState.projectId });
      sessionState.daemonConnected = false;
      logDaemonStatus(false, { reason: 'heartbeat_failed' });
      recordDaemonFallback('session', 'heartbeat_failed');
      return HEARTBEAT_INTERVAL_MS; // retry at the default cadence; do not latch off
    }
  }

  // Keep the MCP self-repo's "self-repo" session alive so its LSP/priority
  // persist while this server runs; re-register it if the daemon lost it.
  if (sessionState.selfRepoProjectId) {
    try {
      const response = await daemonClient.heartbeat({
        project_id: sessionState.selfRepoProjectId,
        session_id: 'self-repo',
      });
      if (!response.acknowledged) {
        await ensureSelfRepoRegistered(daemonClient, sessionState);
      }
    } catch (error) {
      logDebug('Self-repo heartbeat failed (non-fatal)', { error: String(error) });
    }
  }

  return nextMs;
}

/**
 * Clean up session resources: stop health monitor, heartbeat, deprioritize project,
 * close daemon and state manager.
 */
export async function cleanup(
  sessionState: SessionState,
  daemonClient: DaemonClient,
  stateManager: SqliteStateManager,
  healthMonitor: HealthMonitor
): Promise<void> {
  healthMonitor.stop();
  logDebug('Health monitoring stopped');

  if (sessionState.heartbeatInterval) {
    clearTimeout(sessionState.heartbeatInterval);
    sessionState.heartbeatInterval = null;
    logDebug('Heartbeat stopped');
  }

  if (sessionState.projectId && sessionState.daemonConnected) {
    try {
      const projectId = sessionState.projectId;
      const response = await daemonClient.deprioritizeProject({
        project_id: projectId,
        ...(sessionState.sessionId ? { session_id: sessionState.sessionId } : {}),
        ...(sessionState.isWorktree && sessionState.watchPath
          ? { watch_path: sessionState.watchPath }
          : {}),
      });
      logSessionEvent('deprioritize', {
        project_id: sessionState.projectId,
        is_active: response.is_active,
        new_priority: response.new_priority,
      });
    } catch (error) {
      logError('Failed to deprioritize project', error, {
        project_id: sessionState.projectId,
      });
    }
  }

  // Best-effort closes: teardown runs from session cleanup / onclose and must
  // never propagate. daemonClient.close() is already internally guarded; wrap
  // stateManager.close() too so a failure there can't skip the end event (or,
  // before the onclose re-entrancy fix, abort the session-count decrement).
  daemonClient.close();
  try {
    stateManager.close();
  } catch (error) {
    logError('Failed to close state manager during cleanup', error);
  }

  logSessionEvent('end', { session_id: sessionState.sessionId });
}
