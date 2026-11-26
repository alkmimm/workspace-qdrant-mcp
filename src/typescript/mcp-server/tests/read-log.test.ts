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
