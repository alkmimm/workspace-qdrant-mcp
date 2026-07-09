/**
 * POST /admin/api/tools/invoke — the admin "tools playground" endpoint.
 *
 * The handler is a thin, faithful wrapper around the SAME `routeTool` path the
 * MCP client uses, so these tests pin the wrapper's own contract (name
 * validation, latency, ok/error shape, request-context cwd binding) while the
 * dispatcher boundary is mocked — routeTool itself is exercised by the tool
 * tests. Mirrors tests/admin/queue-cancel-route.test.ts for req/res mocking.
 */

import type { IncomingMessage, ServerResponse } from 'node:http';
import { Readable } from 'node:stream';

import { describe, it, expect, vi, beforeEach } from 'vitest';

// Mock the dispatcher boundary: a tiny allowlist + a controllable routeTool.
vi.mock('../../src/tool-dispatcher.js', () => ({
  KNOWN_TOOLS: ['search', 'grep', 'store'],
  routeTool: vi.fn(),
}));

import { dispatchAdminApi } from '../../src/admin/routes.js';
import type { AdminDeps } from '../../src/admin/routes.js';
import { routeTool } from '../../src/tool-dispatcher.js';
import { getRequestContext } from '../../src/utils/request-context.js';

const PATH = '/admin/api/tools/invoke';
const routeToolMock = vi.mocked(routeTool);

function mockReq(method: string, url: string, body?: unknown): IncomingMessage {
  const chunks = body === undefined ? [] : [Buffer.from(JSON.stringify(body))];
  const req = Readable.from(chunks) as unknown as IncomingMessage & { method?: string; url?: string };
  req.method = method;
  req.url = url;
  return req as IncomingMessage;
}

function mockRes() {
  const res = {
    statusCode: 0,
    headers: {} as Record<string, string>,
    body: '',
    headersSent: false,
    writeHead(status: number, headers: Record<string, string>) {
      res.statusCode = status;
      res.headers = headers;
      res.headersSent = true;
      return res;
    },
    end(payload?: string) {
      if (payload != null) res.body = payload;
      return res;
    },
  };
  return res;
}

const deps = { components: {} } as unknown as AdminDeps;

async function invoke(body: unknown, res = mockRes()) {
  const handled = await dispatchAdminApi(
    mockReq('POST', PATH, body),
    res as unknown as ServerResponse,
    PATH,
    deps
  );
  return { handled, res };
}

describe('POST /admin/api/tools/invoke', () => {
  beforeEach(() => routeToolMock.mockReset());

  it('dispatches a known tool through routeTool and wraps the raw result', async () => {
    routeToolMock.mockResolvedValueOnce({ results: [{ id: 'a' }] });

    const { handled, res } = await invoke({ tool: 'search', args: { query: 'hi', cwd: '/repo' } });

    expect(handled).toBe(true);
    // routeTool gets (tool, args, components, session) — the real MCP signature.
    expect(routeToolMock).toHaveBeenCalledTimes(1);
    const [tool, args, , session] = routeToolMock.mock.calls[0]!;
    expect(tool).toBe('search');
    expect(args).toMatchObject({ query: 'hi', cwd: '/repo' });
    expect(session).toMatchObject({ sessionId: 'admin-playground', projectPath: '/repo' });

    expect(res.statusCode).toBe(200);
    const parsed = JSON.parse(res.body);
    expect(parsed.ok).toBe(true);
    expect(parsed.tool).toBe('search');
    expect(parsed.result).toEqual({ results: [{ id: 'a' }] });
    expect(typeof parsed.latencyMs).toBe('number');
  });

  it('binds the cwd into the request context (parity with a real MCP call)', async () => {
    // The tool sees the cwd via AsyncLocalStorage, not just the args object.
    routeToolMock.mockImplementationOnce(() =>
      Promise.resolve({ ctxCwd: getRequestContext()?.hostCwd ?? null })
    );

    const { res } = await invoke({ tool: 'search', args: { query: 'x', cwd: '/abs/proj' } });

    expect(JSON.parse(res.body).result).toEqual({ ctxCwd: '/abs/proj' });
  });

  it('runs without a bound context when no cwd is given', async () => {
    routeToolMock.mockImplementationOnce(() =>
      Promise.resolve({ ctxCwd: getRequestContext()?.hostCwd ?? null })
    );

    const { res } = await invoke({ tool: 'grep', args: { pattern: 'foo' } });

    expect(JSON.parse(res.body).result).toEqual({ ctxCwd: null });
  });

  it('rejects an unknown tool with 400 and never dispatches', async () => {
    const { res } = await invoke({ tool: 'definitely_not_a_tool', args: {} });

    expect(res.statusCode).toBe(400);
    expect(routeToolMock).not.toHaveBeenCalled();
  });

  it('rejects a missing tool name with 400', async () => {
    const { res } = await invoke({ args: { query: 'x' } });

    expect(res.statusCode).toBe(400);
    expect(routeToolMock).not.toHaveBeenCalled();
  });

  it('returns 200 with ok:false when the tool throws (renders in the results pane)', async () => {
    routeToolMock.mockRejectedValueOnce(new Error('bad args: query required'));

    const { res } = await invoke({ tool: 'search', args: {} });

    expect(res.statusCode).toBe(200);
    const parsed = JSON.parse(res.body);
    expect(parsed.ok).toBe(false);
    expect(parsed.error).toContain('bad args');
    expect(typeof parsed.latencyMs).toBe('number');
  });
});
