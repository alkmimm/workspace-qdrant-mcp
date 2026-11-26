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

    it('uses the same template across gaps, retaining tenant, operation, timestamps and raw locators', async () => {
      const times = [1, 2, 3, 4, 5, 6].map((i) => `2026-01-01T10:00:0${i}Z `);
      const messages = [
        done(idA, 2),
        span(idA, 'one', 'retry'),
        done(idB, 100),
        done(idB, 0),
        span(idB, 'two', 'retry'),
        done(idB, 2, 'Add'),
      ];
      const lines = messages.map((msg, i) => times[i] + msg + '\r\n');
      await writeFile(join(dir, args.file), 'outside\n' + lines.join(''));
      const out = await readLog({ ...args, mode: 'interleaved', startLine: 2, groupBy: 'window' });
      expect(out).toBe(
        'file=memexd.log\n[L2,4-5 ×3 through 2026-01-01T10:00:04Z] ' +
          times[0] +
          done('<id>', '0–100') +
          '\n[L3] ' +
          times[1] +
          messages[1] +
          '\n[L6] ' +
          times[4] +
          messages[4] +
          '\n[L7] ' +
          times[5] +
          messages[5] +
          '\n'
      );
      expect(await readLog({ ...args, mode: 'raw', startLine: 4, endLine: 5 })).toBe(
        lines.slice(2, 4).join('')
      );
    });

    it('summarizes changing IDs and all durations within the selected interval', async () => {
      const times = [1, 2, 3, 4, 5].map((i) => `2026-11-26T12:00:0${i}Z `);
      const input = [done(idA, 600), done(idB, 42), done(idA, 107), done(idB, 0), done(idA, 85)];
      const lines = input.map((msg, i) => times[i] + msg + '\r\n');
      await writeFile(join(dir, args.file), lines.join(''));
      const range = { startLine: 2, endLine: 5 };
      expect(await readLog({ ...template, ...range })).toBe(
        '[L2-5 ×4 through 2026-11-26T12:00:05Z] ' + times[1] + done('<id>', '0–107') + '\r\n'
      );
      expect(await readLog({ ...args, ...range, mode: 'auto' })).toBe(
        await readLog({ ...template, ...range })
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

  describe('interleaved block boundaries', () => {
    const selected = { ...args, mode: 'interleaved', startLine: 9, endLine: 109 };
    it('anchors blocks at startLine, clips the window and orders groups by first appearance', async () => {
      const rows = [
        ...Array<string>(8).fill('before'),
        'hit',
        ...Array<string>(98).fill('other'),
        'hit',
        'hit',
        'after',
      ];
      await writeFile(join(dir, args.file), rows.join('\n'));
      const block = 'file=memexd.log\n[L9,108 ×2] hit\n[L10-107 ×98] other\n[L109] hit\n';
      expect(await readLog(selected)).toBe(block);
      expect(await readLog({ ...selected, groupBy: 'block' })).toBe(block);
      expect(await readLog({ ...selected, groupBy: 'window' })).toBe(
        'file=memexd.log\n[L9,108-109 ×3] hit\n[L10-107 ×98] other\n'
      );
    });

    it('identifies blank and unterminated lines and returns empty text past EOF', async () => {
      await writeFile(join(dir, args.file), '\r\nx\n\nlast');
      expect(await readLog({ ...args, mode: 'interleaved' })).toBe(
        'file=memexd.log\n[L1,3 ×2] \n[L2] x\n[L4] last\n'
      );
      expect(await readLog({ ...args, mode: 'raw', startLine: 4 })).toBe('last');
      expect(await readLog(selected)).toBe('');
    });
  });

  describe('auto mode', () => {
    const auto = { ...args, mode: 'auto' };

    it('compares UTF-8 bytes and returns the selected summary without an envelope', async () => {
      const raw = '界界界\nX\n'.repeat(6);
      await writeFile(join(dir, args.file), raw);
      const compact = await readLog(args);
      const interleaved = await readLog({ ...args, mode: 'interleaved' });
      expect(compact.length).toBeLessThan(interleaved.length);
      expect(Buffer.byteLength(compact)).toBeGreaterThan(Buffer.byteLength(interleaved));
      expect(await readLog(auto)).toBe(interleaved);
      expect(await readLog({ ...auto, startLine: 13 })).toBe('');
    });

    it('prefers compact on a byte tie between distinct outputs', async () => {
      const raw = ('界'.repeat(6) + '\nX\n').repeat(3);
      await writeFile(join(dir, args.file), raw);
      const interleaved = await readLog({ ...args, mode: 'interleaved' });
      expect(interleaved).not.toBe(raw);
      expect(Buffer.byteLength(interleaved)).toBe(Buffer.byteLength(raw));
      expect(await readLog(auto)).toBe(raw);
    });

    it('prefers block on a byte tie with a different whole-window summary', async () => {
      const lines = [
        '2026-01-01T00:00:00Z boundary',
        ...Array.from(
          { length: 99 },
          (_, i) => `repeat long log message ${i % 2} for a background worker`
        ),
        '2026-01-01T00:00:01Z boundary',
      ];
      await writeFile(join(dir, args.file), lines.join('\n'));
      const block = await readLog({ ...args, mode: 'interleaved', groupBy: 'block' });
      const window = await readLog({ ...args, mode: 'interleaved', groupBy: 'window' });
      expect(block).not.toBe(window);
      expect(Buffer.byteLength(block)).toBe(Buffer.byteLength(window));
      expect(await readLog(auto)).toBe(block);
    });

    it('considers the entire window when events repeat across block boundaries', async () => {
      const block = Array.from(
        { length: 100 },
        (_, i) => `long message from worker number ${i}\n`
      ).join('');
      await writeFile(join(dir, args.file), 'excluded\n' + block + block);
      const range = { ...args, startLine: 2, endLine: 201 };
      const window = await readLog({ ...range, mode: 'interleaved', groupBy: 'window' });
      expect(window).toContain('[L2,102 ×2]');
      for (const mode of ['compact', 'template', 'interleaved']) {
        expect(Buffer.byteLength(window)).toBeLessThan(
          Buffer.byteLength(await readLog({ ...range, mode }))
        );
      }
      expect(await readLog({ ...range, mode: 'auto' })).toBe(window);
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
    { mode: 'interleaved', groupBy: 'other' },
    { mode: 'interleaved', groupBy: null },
    { mode: 'compact', groupBy: 'window' },
    { mode: 'auto', groupBy: 'block' },
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
