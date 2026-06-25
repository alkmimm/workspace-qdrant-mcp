import type { IncomingMessage, ServerResponse } from 'node:http';
import { Readable } from 'node:stream';

import { describe, it, expect, vi } from 'vitest';

import { dispatchAdminApi } from '../../src/admin/routes.js';
import type { AdminDeps } from '../../src/admin/routes.js';

const PATH = '/admin/api/queue/cancel';

function mockReq(method: string, url: string, body?: unknown): IncomingMessage {
  const chunks = body === undefined ? [] : [Buffer.from(JSON.stringify(body))];
  const req = Readable.from(chunks) as unknown as IncomingMessage & {
    method?: string;
    url?: string;
  };
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

function depsWith(cancelItems: ReturnType<typeof vi.fn>): AdminDeps {
  return { daemonClient: { cancelItems } } as unknown as AdminDeps;
}

describe('POST /admin/api/queue/cancel', () => {
  it('dry-runs with default statuses=[pending] and maps the gRPC response', async () => {
    const cancelItems = vi.fn().mockResolvedValue({
      count: 7,
      tenant_id: 't1',
      project_path: '/repo/t1',
      is_dry_run: true,
    });
    const res = mockRes();

    const handled = await dispatchAdminApi(
      mockReq('POST', PATH, { tenantId: 't1', dryRun: true }),
      res as unknown as ServerResponse,
      PATH,
      depsWith(cancelItems)
    );

    expect(handled).toBe(true);
    expect(cancelItems).toHaveBeenCalledWith({
      tenant_id: 't1',
      statuses: ['pending'],
      dry_run: true,
    });
    expect(res.statusCode).toBe(200);
    expect(JSON.parse(res.body)).toEqual({
      ok: true,
      count: 7,
      tenantId: 't1',
      projectPath: '/repo/t1',
      isDryRun: true,
    });
  });

  it('passes custom statuses through and defaults dryRun to false', async () => {
    const cancelItems = vi.fn().mockResolvedValue({
      count: 3,
      tenant_id: 't2',
      project_path: '/repo/t2',
      is_dry_run: false,
    });
    const res = mockRes();

    await dispatchAdminApi(
      mockReq('POST', PATH, { tenantId: 't2', statuses: ['pending', 'failed'] }),
      res as unknown as ServerResponse,
      PATH,
      depsWith(cancelItems)
    );

    expect(cancelItems).toHaveBeenCalledWith({
      tenant_id: 't2',
      statuses: ['pending', 'failed'],
      dry_run: false,
    });
    expect(res.statusCode).toBe(200);
    expect(JSON.parse(res.body).count).toBe(3);
  });

  it('returns 400 and does not call the daemon when tenantId is missing', async () => {
    const cancelItems = vi.fn();
    const res = mockRes();

    await dispatchAdminApi(
      mockReq('POST', PATH, { dryRun: true }),
      res as unknown as ServerResponse,
      PATH,
      depsWith(cancelItems)
    );

    expect(res.statusCode).toBe(400);
    expect(cancelItems).not.toHaveBeenCalled();
  });

  it('returns 502 when the daemon gRPC call fails', async () => {
    const cancelItems = vi.fn().mockRejectedValue(new Error('grpc down'));
    const res = mockRes();

    await dispatchAdminApi(
      mockReq('POST', PATH, { tenantId: 't3' }),
      res as unknown as ServerResponse,
      PATH,
      depsWith(cancelItems)
    );

    expect(res.statusCode).toBe(502);
  });
});
