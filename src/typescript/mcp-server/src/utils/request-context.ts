/**
 * Per-request execution context for MCP tool handlers.
 *
 * The MCP server can run in two transports:
 *   - stdio: the process is spawned by the client; `process.cwd()` already
 *     reflects the user's working directory.
 *   - http  (typically Docker): a long-lived container whose `process.cwd()`
 *     is fixed at WORKDIR and unrelated to the client. Per-request metadata
 *     (e.g. the host CWD) must be carried explicitly from the client.
 *
 * `getEffectiveCwd()` is the single source of truth tool handlers should
 * use instead of `process.cwd()`. Its resolution order is:
 *
 *   1. Per-request override stored in {@link requestContext} (set by the HTTP
 *      transport from the `X-MCP-Host-Cwd` header).
 *   2. `WQM_DEFAULT_HOST_CWD` env var — useful as a stdio fallback when the
 *      MCP is launched from a directory that does not match any registered
 *      project (e.g. Claude Code starts MCP from the user's home).
 *   3. `process.cwd()` — works for stdio when the client launches the MCP
 *      from inside the project / worktree.
 */

import { AsyncLocalStorage } from 'node:async_hooks';

export interface RequestContext {
  /** Host-side absolute path the client is operating from. */
  hostCwd?: string;
  /**
   * MCP session id of the request (the `Mcp-Session-Id` header on the HTTP
   * transport). Consumed by the effectiveness-signals instrumentation to
   * group search events into sessions (spec 20 §1.2); absent on stdio and
   * on the anonymous `initialize` request.
   */
  mcpSessionId?: string;
  /**
   * Where `hostCwd` came from — the transport header, the tool-body `cwd`, or
   * the session's sticky cwd remembered from an EARLIER call. Surfaces in read
   * responses as `project_source` so a call that silently rode a stale sticky
   * cwd (and answered from the previous repo) is legible to the caller.
   */
  cwdSource?: CwdSource;
}

/** Provenance of the effective host cwd bound to a request. */
export type CwdSource = 'header' | 'body' | 'sticky';

const storage = new AsyncLocalStorage<RequestContext>();

/** Run `fn` with the given request context bound to AsyncLocalStorage. */
export function runWithRequestContext<T>(ctx: RequestContext, fn: () => T): T {
  return storage.run(ctx, fn);
}

/** Read the current request context, or `undefined` outside a request. */
export function getRequestContext(): RequestContext | undefined {
  return storage.getStore();
}

/**
 * Resolve the effective working directory for project detection.
 *
 * See module docs for the full resolution chain.
 */
export function getEffectiveCwd(): string {
  const ctx = storage.getStore();
  if (ctx?.hostCwd && ctx.hostCwd.length > 0) {
    return ctx.hostCwd;
  }
  const envCwd = process.env.WQM_DEFAULT_HOST_CWD;
  if (envCwd && envCwd.length > 0) {
    return envCwd;
  }
  return process.cwd();
}

/**
 * If the caller's effective cwd is inside a linked git worktree
 * (`…/.claude/worktrees/<name>[/…]`), return the worktree `name` and its `root`.
 *
 * The worktree-membership model indexes a worktree's content under the MAIN
 * repo's folder, so `search`/`grep` results carry MAIN-anchored paths — a caller
 * working in a worktree must `Read` `${root}/${relativePath}` (its own copy, which
 * for a divergent file differs from main), not the returned main path. Surfacing
 * this once lets the response say so instead of the caller translating every path
 * by hand (field feedback). Path-shape only — no disk/DB lookup; matches `/`
 * and `\` separators (a Windows-host cwd over UNC).
 */
export function getWorktreeContext(): { name: string; root: string } | undefined {
  return worktreeContextOf(getEffectiveCwd());
}

/**
 * Path-shape worktree detection for an arbitrary cwd string: a path inside
 * `.claude/worktrees/<name>` (either separator). Shared by the read tools'
 * worktree note (via {@link getWorktreeContext}) and by scratchpad provenance,
 * which applies it to the cwd it stamps as `origin_cwd`.
 */
export function worktreeContextOf(cwd: string): { name: string; root: string } | undefined {
  const m = /^(.*[/\\]\.claude[/\\]worktrees[/\\]([^/\\]+))(?:[/\\].*)?$/.exec(cwd);
  if (!m || !m[1] || !m[2]) return undefined;
  return { root: m[1], name: m[2] };
}

/** Outcome of {@link resolveStickyCwd}. */
export interface StickyCwdResolution {
  /**
   * The cwd to bind into the request context via {@link runWithRequestContext}
   * before dispatch, or `undefined` to leave the context unchanged (because an
   * authoritative header is already bound, or there is nothing to bind).
   */
  bind?: string;
  /**
   * The cwd to persist as the session's sticky value, or `undefined` to leave
   * the existing sticky value untouched. Only set when an explicit source
   * (header or body `cwd`) was present this call.
   */
  sticky?: string;
  /** Which source `bind` came from; absent whenever `bind` is absent. */
  bindSource?: 'body' | 'sticky';
}

/**
 * Resolve the effective host cwd for one tool call, with session stickiness.
 *
 * The `X-MCP-Host-Cwd` header (already bound into the request context by the
 * HTTP transport) always wins. When it is absent — the Claude-Code-over-HTTP
 * case, which cannot send the header per session — an agent passes its working
 * directory in the tool-body `cwd`. We remember the last explicit cwd on the
 * session (`stickyCwd`) so a later call that omits `cwd` still resolves the
 * project instead of failing with "Could not detect project".
 *
 * Precedence: header > body `cwd` > session sticky cwd > `WQM_DEFAULT_HOST_CWD`
 * > `process.cwd()` (the last two handled downstream by {@link getEffectiveCwd}).
 *
 * Pure and side-effect-free: the caller persists `sticky` onto session state
 * and binds `bind` into the request context.
 */
export function resolveStickyCwd(opts: {
  headerCwd?: string | undefined;
  bodyCwd?: string | undefined;
  stickyCwd?: string | null | undefined;
}): StickyCwdResolution {
  const header = opts.headerCwd && opts.headerCwd.length > 0 ? opts.headerCwd : undefined;
  const body = opts.bodyCwd && opts.bodyCwd.length > 0 ? opts.bodyCwd : undefined;
  const sticky = opts.stickyCwd && opts.stickyCwd.length > 0 ? opts.stickyCwd : undefined;

  const result: StickyCwdResolution = {};

  // An explicit source (header or body) becomes the session's sticky cwd.
  const explicit = header ?? body;
  if (explicit) result.sticky = explicit;

  // The header is already bound by the transport and wins — never rebind it.
  if (header) return result;

  // No header: bind the body cwd if given, else fall back to the sticky value
  // remembered from an earlier call in this session.
  const effective = body ?? sticky;
  if (effective) {
    result.bind = effective;
    result.bindSource = body ? 'body' : 'sticky';
  }
  return result;
}
