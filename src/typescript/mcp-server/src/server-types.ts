/**
 * Shared types and constants for the MCP server
 */

import type { ServerConfig } from './types/index.js';
import { BUILD_NUMBER, BUILD_SHA } from './build-info.js';
import mcpPublicConfig from './constants/mcp-public-config.json' with { type: 'json' };

// Session heartbeat interval (milliseconds). Kept well below the daemon's
// session-liveness reaper timeout (WQM_SESSION_HEARTBEAT_TIMEOUT_SECS, default
// 90s) so a live session is never false-reaped: the reaper deletes
// `project_sessions` rows whose `last_heartbeat_at` is older than its timeout,
// which re-projects `is_active` to 0. Do not raise this above ~a third of that
// timeout without raising the timeout too (see SessionMonitor /
// start_session_monitor in the daemon).
export const HEARTBEAT_INTERVAL_MS = 30 * 1000;

// Server name and version for MCP protocol. Includes the short commit SHA when
// known (build-arg/git) so a running server reports which build it is; falls
// back to just the legacy build number when the SHA is unavailable ('unknown').
export const SERVER_NAME = 'workspace-qdrant-mcp';
export const SERVER_VERSION =
  BUILD_SHA && BUILD_SHA !== 'unknown'
    ? `0.1.0-beta1 (${BUILD_NUMBER} ${BUILD_SHA})`
    : `0.1.0-beta1 (${BUILD_NUMBER})`;

export interface SessionState {
  sessionId: string;
  projectId: string | null;
  projectPath: string | null;
  /**
   * Last host working directory seen on this session, from an explicit
   * `x-mcp-host-cwd` header or a tool-body `cwd` argument. Persisted so that
   * SUBSEQUENT calls in the same HTTP session that omit `cwd` still resolve the
   * project — over HTTP the client (e.g. Claude Code) cannot send the header
   * per session, so without this an agent would have to repeat `cwd` on every
   * single call or get "Could not detect project". `null` until the first
   * explicit cwd is seen. See `resolveStickyCwd` and `handleToolCall`.
   */
  lastHostCwd: string | null;
  /** Canonical watch path returned by daemon (may differ from projectPath due to symlink resolution) */
  watchPath: string | null;
  isWorktree: boolean;
  /**
   * Tenant id of the MCP self-repo (workspace-qdrant-mcp), captured when it
   * registers so the heartbeat loop can keep its stable "self-repo" session
   * alive. `null` until the self-repo registers (or if `WQM_REPO_DIR` is unset).
   */
  selfRepoProjectId: string | null;
  /**
   * Current git branch for `projectPath`, refreshed lazily by the tool
   * dispatcher (see `ensureProjectFresh`). `null` when the project is not
   * a git repo or git is unavailable. Tools that accept a `branch` filter
   * (search, grep) use this as the default when the caller omits it.
   */
  currentBranch: string | null;
  /**
   * Unix-ms timestamp of the most recent successful git state refresh.
   * Used by `ensureProjectFresh` to throttle re-detection — see
   * `BRANCH_FRESHNESS_MS` in `session-lifecycle.ts`.
   */
  lastBranchRefreshAt: number;
  heartbeatInterval: ReturnType<typeof setInterval> | null;
  daemonConnected: boolean;
  /**
   * Idempotence flag for cleanupSession (F-049).
   * Set to `true` on the first cleanup invocation; subsequent calls no-op.
   */
  cleaned: boolean;
}

/**
 * Transport mode for the MCP server.
 *
 * - `stdio`: MCP over stdin/stdout (default; Claude Desktop, `claude mcp ...` CLI).
 * - `http`:  MCP Streamable HTTP transport. Required for Docker deployments and
 *            for any client that cannot spawn a subprocess.
 * - `test`:  In-process only; the server is constructed and wired but no
 *            transport is connected. Used by unit tests that drive the server
 *            directly through its class API.
 */
export type ServerMode = 'stdio' | 'http' | 'test';

/**
 * Optional TLS configuration for the HTTP transport.
 *
 * When populated the server terminates TLS itself (Node `https` module).
 * Recommended production pattern is to run plain HTTP behind a reverse proxy
 * (Caddy / Traefik / nginx) and leave these fields unset; native TLS exists
 * as a fallback for deployments that do not want a proxy.
 */
export interface HttpTlsOptions {
  /** Filesystem path to the server certificate in PEM format (chain ok). */
  certPath: string;
  /** Filesystem path to the server private key in PEM format. */
  keyPath: string;
  /** Optional PEM path for the CA bundle, for intermediate chain serving. */
  caPath?: string;
}

/**
 * HTTP transport configuration.
 *
 * `host` defaults to `127.0.0.1`. Bind to `0.0.0.0` only inside a container
 * (Docker will expose the listener explicitly via `-p`). `path` is the request
 * route; defaults to `/mcp` to match the Streamable HTTP spec convention.
 */
export interface HttpTransportOptions {
  host: string;
  port: number;
  path: string;
  /** Optional native TLS termination. Unset → plain HTTP. */
  tls?: HttpTlsOptions;
}

export interface ServerOptions {
  config: ServerConfig;
  /**
   * Transport mode. When omitted, `stdio` is inferred from the legacy `stdio`
   * boolean (kept for back-compat with older tests): `stdio: false` → `test`,
   * otherwise `stdio`.
   */
  mode?: ServerMode;
  /** Legacy toggle. Prefer `mode`. `stdio: false` maps to `mode: 'test'`. */
  stdio?: boolean;
  /** HTTP transport settings. Required when `mode === 'http'`. */
  http?: HttpTransportOptions;
  /**
   * HTTP auth / rate limit / CORS. If omitted, the server reads the same
   * settings from the process environment (`MCP_HTTP_TOKEN`,
   * `MCP_HTTP_RATE_LIMIT`, `MCP_HTTP_CORS_ORIGINS`). Tests inject an explicit
   * config to avoid touching real env vars.
   */
  auth?: import('./auth-middleware.js').AuthConfig;
}

/** Default HTTP listener configuration for `mode: 'http'`.
 *
 * Values come from src/constants/mcp-public-config.json (single source of
 * truth, shared with admin UI generators and PowerShell renderers).
 * Drift is asserted by tests/admin/port-drift.test.ts.
 */
export const DEFAULT_HTTP_HOST = mcpPublicConfig.http.host;
export const DEFAULT_HTTP_PORT = mcpPublicConfig.http.port;
export const DEFAULT_HTTP_PATH = mcpPublicConfig.http.path;
