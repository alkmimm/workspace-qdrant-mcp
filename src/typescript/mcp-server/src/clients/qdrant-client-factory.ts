/**
 * Construct a `QdrantClient` from MCP-server config fields.
 *
 * Four call sites (search, retrieve, rules, health-monitor) used to repeat
 * the same six-line boilerplate of building a `clientConfig` literal and
 * conditionally setting `apiKey`. This factory centralizes that.
 *
 * It does *not* memoize: a fresh `QdrantClient` is returned on every call
 * so per-test vitest mocks of the constructor keep working. Caching here
 * would silence three of the four "Api key is used with unsecure
 * connection." log lines per MCP session, but the test-isolation cost is
 * not worth the small log-noise win.
 */

import { QdrantClient } from '@qdrant/js-client-rest';

export interface QdrantClientOptions {
  /** Qdrant base URL, e.g. `http://qdrant:6333`. */
  url: string;
  /**
   * Optional API key. `undefined` is accepted (callers spread it from a
   * possibly-unset config field) and treated identically to a missing key.
   */
  apiKey?: string | undefined;
  /**
   * Request timeout in milliseconds. Left unset by every production call site
   * so the shared resolution below (env → default) applies uniformly; pass a
   * value only to pin one client (tests).
   */
  timeout?: number | undefined;
}

/**
 * Default per-request timeout for every Qdrant call the MCP server makes.
 *
 * Was 5000, which is well under the floor for a BURST. Steady-state reads are
 * ~25ms p50 and a raw scroll measures 0.27s, but the daemon periodically
 * saturates Qdrant (a `git switch` triggers a folder-scan storm plus
 * `branch_reconcile`), and during one such window nine parallel MCP calls
 * produced four hard failures — search/list/retrieve aborting with the opaque
 * "This operation was aborted" (measured 2026-09-05, right after a redeploy).
 * Nothing was wrong with the query: the client gave up before the server got
 * to it.
 *
 * 30s matches `WQM_DAEMON_TIMEOUT_MS` (server-factory.ts) and still lands well
 * inside a typical 60s MCP client budget, so a genuinely stuck Qdrant is still
 * reported as an error rather than hanging the caller.
 */
export const DEFAULT_QDRANT_TIMEOUT_MS = 30000;

/** Env knob for {@link DEFAULT_QDRANT_TIMEOUT_MS}, mirroring `WQM_DAEMON_TIMEOUT_MS`. */
export const QDRANT_TIMEOUT_ENV_VAR = 'WQM_QDRANT_TIMEOUT_MS';

/**
 * Resolve the effective timeout: explicit argument → `WQM_QDRANT_TIMEOUT_MS` →
 * {@link DEFAULT_QDRANT_TIMEOUT_MS}. Non-numeric or non-positive values fall
 * through to the next rung instead of disabling the timeout, so a typo in the
 * env cannot silently produce a client that waits forever.
 *
 * Resolution lives HERE, not at the call sites: search/retrieve/rules/
 * scratchpad/health-monitor/agent-rules each used to carry their own
 * `?? 5000`, so a deployment could not raise the timeout without touching six
 * files — and the two that were tuned would silently disagree with the four
 * that were not.
 */
export function resolveQdrantTimeoutMs(explicit?: number): number {
  if (explicit !== undefined && Number.isFinite(explicit) && explicit > 0) {
    return explicit;
  }
  const fromEnv = Number(process.env[QDRANT_TIMEOUT_ENV_VAR]);
  if (Number.isFinite(fromEnv) && fromEnv > 0) {
    return fromEnv;
  }
  return DEFAULT_QDRANT_TIMEOUT_MS;
}

/** Build a `QdrantClient` from the connection options. */
export function getQdrantClient(opts: QdrantClientOptions): QdrantClient {
  const clientConfig: { url: string; apiKey?: string; timeout?: number } = {
    url: opts.url,
    timeout: resolveQdrantTimeoutMs(opts.timeout),
  };
  if (opts.apiKey) {
    clientConfig.apiKey = opts.apiKey;
  }
  return new QdrantClient(clientConfig);
}
