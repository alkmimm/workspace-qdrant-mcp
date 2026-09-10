/**
 * Tests for telemetry/http-server.ts
 *
 * Verifies:
 *  - GET /metrics responds 200 with Prometheus text content-type
 *  - Response body contains Prometheus metric lines
 *  - Non-/metrics paths return 404
 *  - Non-GET methods return 404
 */

import { describe, it, expect } from 'vitest';
import { request as httpRequest, type IncomingMessage } from 'node:http';
import type { Server } from 'node:http';
import { createServer as createProbeServer } from 'node:net';
import { startMetricsServer } from '../../src/telemetry/http-server.js';

/**
 * Ask the OS for a port that is genuinely free, rather than guessing one.
 *
 * Each test still gets its own port — that part was right, and avoids the
 * TIME_WAIT races that come from reusing one. What was wrong is where the
 * numbers came from: a counter starting at 19100, which is exactly where the
 * running stack binds its own metrics endpoints. With the daemon up, 19100,
 * 19101 and 19102 are taken and three of these six tests fail on EADDRINUSE —
 * on the developer's machine, every time, for a reason that has nothing to do
 * with the code under test.
 *
 * A test that goes red because your own service is running teaches you to
 * ignore red, which costs more than the coverage is worth.
 *
 * Port 0 is not passed to `startMetricsServer` on purpose: it rejects anything
 * below 1, and that guard protects a real misconfiguration (`MCP_METRICS_PORT=0`
 * should be an error, not a silent bind to a random port). So the free port is
 * discovered here and handed over as a normal value.
 *
 * The probe binds the same host the server will use, because a listener on
 * 0.0.0.0 blocks a 127.0.0.1 bind of the same port — the collision seen here.
 */
function freePort(): Promise<number> {
  const host = process.env['MCP_METRICS_HOST'] ?? '127.0.0.1';
  return new Promise((resolve, reject) => {
    const probe = createProbeServer();
    probe.on('error', reject);
    probe.listen(0, host, () => {
      const address = probe.address();
      const port = typeof address === 'object' && address !== null ? address.port : 0;
      probe.close((err) => {
        if (err) reject(err);
        else if (port === 0) reject(new Error('could not resolve an ephemeral port'));
        else resolve(port);
      });
    });
  });
}

function closeServer(server: Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((err) => {
      if (err) reject(err);
      else resolve();
    });
  });
}

function makeRequest(
  port: number,
  path: string,
  method = 'GET'
): Promise<{ statusCode: number; contentType: string; body: string }> {
  return new Promise((resolve, reject) => {
    const req = httpRequest(
      { hostname: '127.0.0.1', port, path, method },
      (res: IncomingMessage) => {
        let body = '';
        res.on('data', (chunk: Buffer) => {
          body += chunk.toString();
        });
        res.on('end', () => {
          resolve({
            statusCode: res.statusCode ?? 0,
            contentType: (res.headers['content-type'] as string) ?? '',
            body,
          });
        });
      }
    );
    req.on('error', reject);
    req.end();
  });
}

function startAndWait(port: number): Promise<Server> {
  return new Promise((resolve, reject) => {
    const server = startMetricsServer(port);
    server.once('listening', () => resolve(server));
    server.once('error', reject);
  });
}

describe('startMetricsServer', () => {
  it('GET /metrics returns 200', async () => {
    const port = await freePort();
    const server = await startAndWait(port);
    try {
      const { statusCode } = await makeRequest(port, '/metrics');
      expect(statusCode).toBe(200);
    } finally {
      await closeServer(server);
    }
  });

  it('GET /metrics returns Prometheus text content-type', async () => {
    const port = await freePort();
    const server = await startAndWait(port);
    try {
      const { contentType } = await makeRequest(port, '/metrics');
      expect(contentType).toContain('text/plain');
    } finally {
      await closeServer(server);
    }
  });

  it('GET /metrics body contains Prometheus comment lines', async () => {
    const port = await freePort();
    const server = await startAndWait(port);
    try {
      const { body } = await makeRequest(port, '/metrics');
      // Prometheus text format always has # HELP or # TYPE lines
      expect(body).toMatch(/^#\s+(HELP|TYPE)\s+/m);
    } finally {
      await closeServer(server);
    }
  });

  it('GET /metrics body contains wqm_mcp metric names', async () => {
    const port = await freePort();
    const server = await startAndWait(port);
    try {
      const { body } = await makeRequest(port, '/metrics');
      expect(body).toContain('wqm_mcp_tool_invocations_total');
      expect(body).toContain('wqm_mcp_session_count');
    } finally {
      await closeServer(server);
    }
  });

  it('GET /unknown returns 404', async () => {
    const port = await freePort();
    const server = await startAndWait(port);
    try {
      const { statusCode } = await makeRequest(port, '/unknown');
      expect(statusCode).toBe(404);
    } finally {
      await closeServer(server);
    }
  });

  it('POST /metrics returns 404', async () => {
    const port = await freePort();
    const server = await startAndWait(port);
    try {
      const { statusCode } = await makeRequest(port, '/metrics', 'POST');
      expect(statusCode).toBe(404);
    } finally {
      await closeServer(server);
    }
  });
});
