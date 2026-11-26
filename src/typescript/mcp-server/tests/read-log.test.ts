import { mkdtemp, mkdir, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { readLog } from '../src/tools/read-log.js';

describe('read_log', () => {
  let dir: string;
  const args = { file: 'memexd.log' };

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'read-log-'));
    vi.stubEnv('WQM_LOG_DIR', dir);
  });

  afterEach(async () => {
    vi.unstubAllEnvs();
    await rm(dir, { recursive: true, force: true });
  });

  it('groups consecutive events within the range and preserves changes and stack frames', async () => {
    const t = (i: number) => `2026-11-26T10:00:0${i}.123456789+00:00`;
    const events = ['WARN 429', 'WARN 429', 'WARN 429', 'ERROR 503', '    at retry', 'WARN 429'];
    await writeFile(join(dir, args.file), events.map((s, i) => `${t(i)} ${s}\n`).join(''));
    expect(await readLog({ ...args, startLine: 2 })).toBe(
      `[L2-3 ×2 through ${t(2)}] ${t(1)} WARN 429\n${t(3)} ERROR 503\n${t(4)}     at retry\n${t(5)} WARN 429\n`
    );
    expect(await readLog({ ...args, mode: 'raw', startLine: 2, endLine: 3 })).toBe(
      `${t(1)} WARN 429\n${t(2)} WARN 429\n`
    );
  });

  it('preserves UTF-8, CRLF, blank lines and an unterminated last line across chunks', async () => {
    await writeFile(join(dir, args.file), 'x'.repeat(65535) + '\npré\r\n\r\nend');
    expect(await readLog({ ...args, mode: 'raw', startLine: 2 })).toBe('pré\r\n\r\nend');
    expect(await readLog({ ...args, startLine: 5 })).toBe('');
  });

  it('keeps a repeated untimestamped message separate from its timestamped counterpart', async () => {
    await writeFile(join(dir, args.file), ' msg\n msg\n2026-01-01T00:00:00Z msg\n');
    expect(await readLog(args)).toBe('[L1-2 ×2]  msg\n2026-01-01T00:00:00Z msg\n');
  });

  describe('template mode', () => {
    const idA = 'a'.repeat(32);
    const idB = 'b'.repeat(32);
    const target =
      'INFO workspace_qdrant_core::unified_queue_processor::processing_loop::batch_processing: ';
    const done = (id: string, ms: number | string, op = 'Delete') =>
      `${target}Successfully processed unified item ${id} (type=File, op=${op}) in ${ms}ms`;
    const span = (id: string, tenant: string, message: string) =>
      `WARN queue.process_item{queue_id=${id} item_type=File op=Delete collection=projects tenant_id=${tenant}}: worker: ${message}`;
    const template = { ...args, mode: 'template' };

    it('summarizes changing IDs and all durations within the selected interval', async () => {
      const times = [1, 2, 3, 4, 5].map((i) => `2026-11-26T12:00:0${i}Z `);
      const input = [done(idA, 600), done(idB, 42), done(idA, 107), done(idB, 0), done(idA, 85)];
      const lines = input.map((msg, i) => times[i] + msg + '\r\n');
      await writeFile(join(dir, args.file), lines.join(''));
      const range = { startLine: 2, endLine: 5 };
      expect(await readLog({ ...template, ...range })).toBe(
        '[L2-5 ×4 through 2026-11-26T12:00:05Z] ' + times[1] + done('<id>', '0–107') + '\r\n'
      );
      const original = lines.slice(1).join('');
      expect(await readLog({ ...args, ...range, mode: 'raw' })).toBe(original);
      expect(await readLog({ ...args, ...range })).toBe(original);
    });

    it('keeps stable IDs and durations, singleton lines and boundaries between operations', async () => {
      const lines = [done(idA, 42), done(idA, 41), done(idB, 2, 'Add'), done(idA, 9), done(idB, 9)];
      await writeFile(join(dir, args.file), lines.join('\n'));
      expect(await readLog(template)).toBe(
        '[L1-2 ×2] ' +
          done(idA, '41–42') +
          '\n' +
          lines[2] +
          '\n[L4-5 ×2] ' +
          done('<id>', 9) +
          '\n'
      );
      expect(await readLog({ ...template, startLine: 5 })).toBe(lines[4]);
    });

    it('masks only the queue span ID and keeps tenants, error codes and intervening events distinct', async () => {
      const lines = [
        span(idA, 'one', 'HTTP 429'),
        span(idB, 'one', 'HTTP 429'),
        span(idB, 'two', 'HTTP 429'),
        span(idA, 'two', 'HTTP 503'),
        '    at retry',
        span(idA, 'two', 'HTTP 503'),
      ];
      await writeFile(join(dir, args.file), lines.join('\n'));
      expect(await readLog(template)).toBe(
        '[L1-2 ×2] ' + span('<id>', 'one', 'HTTP 429') + '\n' + lines.slice(2).join('\n')
      );
    });

    it('uses exact grouping for unrecognized lines and never merges a literal placeholder with an ID', async () => {
      const lines = [
        span('<id>', 'one', 'ok'),
        span(idA, 'one', 'ok'),
        `ERROR worker: queue_id=${idA}`,
        `ERROR worker: queue_id=${idB}`,
        'heartbeat',
        'heartbeat',
      ];
      await writeFile(join(dir, args.file), lines.join('\n'));
      expect(await readLog(template)).toBe(
        lines.slice(0, 4).join('\n') + '\n[L5-6 ×2] heartbeat\n'
      );
    });
  });

  it('caps retained bytes, even for a single line, and can read past it', async () => {
    await writeFile(join(dir, args.file), 'x'.repeat(1024 * 1024 + 1) + '\nOK\n');
    await expect(readLog(args)).rejects.toThrow('1 MiB');
    expect(await readLog({ ...args, startLine: 2 })).toBe('OK\n');
  });

  it('defaults to 200 lines and clips a range at EOF', async () => {
    await writeFile(join(dir, args.file), 'a\n'.repeat(201));
    expect(await readLog(args)).toBe('[L1-200 ×200] a\n');
    expect(await readLog({ ...args, startLine: 201, endLine: 205 })).toBe('a\n');
  });

  it.each([
    { startLine: 0 },
    { endLine: 2.5 },
    { startLine: 2, endLine: 1 },
    { endLine: 2001 },
    { endLine: null },
    { mode: 'other' },
  ])('rejects invalid options %o', async (bad) => {
    await expect(readLog({ ...args, ...bad })).rejects.toThrow();
  });

  it.each(['../other.log', '/outside.log', 'C:\\tmp\\outside.log', 'other.txt', 'NUL.log'])(
    'rejects filenames outside the log contract: %s',
    async (file) => {
      await expect(readLog({ file })).rejects.toThrow('filename');
    }
  );

  it('rejects directories and missing logs', async () => {
    await expect(readLog(args)).rejects.toThrow();
    await mkdir(join(dir, args.file));
    await expect(readLog(args)).rejects.toThrow('regular log file');
  });

  it.skipIf(process.platform === 'win32')('refuses symbolic links', async () => {
    await writeFile(join(dir, 'private'), 'private');
    await symlink(join(dir, 'private'), join(dir, args.file));
    await expect(readLog(args)).rejects.toThrow('regular log file');
  });
});
