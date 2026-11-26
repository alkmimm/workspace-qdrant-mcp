import { constants } from 'node:fs';
import { lstat, open, type FileHandle } from 'node:fs/promises';
import { join } from 'node:path';

import { formatLog } from './read-log-format.js';
import { getLogDirectory } from '../utils/paths.js';

const DEFAULT_LINES = 200;
const MAX_LINES = 2000;
const MAX_RANGE_BYTES = 1024 * 1024;

function lineNumber(value: unknown, field: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) {
    throw new Error(`${field} must be a positive integer`);
  }
  return value;
}

// Scan a snapshot of the opened file, retaining only bytes in the requested range.
// Even an unterminated line before startLine consumes at most one scan buffer.
async function readRange(
  file: FileHandle,
  size: number,
  startLine: number,
  endLine: number
): Promise<string> {
  const buffer = Buffer.allocUnsafe(64 * 1024);
  const parts: Buffer[] = [];
  let retained = 0;
  let line = 1;
  let position = 0;
  while (position < size && line <= endLine) {
    const { bytesRead } = await file.read(
      buffer,
      0,
      Math.min(buffer.length, size - position),
      position
    );
    if (bytesRead === 0) break;
    position += bytesRead;
    const chunk = buffer.subarray(0, bytesRead);
    let offset = 0;
    let start = line >= startLine ? 0 : bytesRead;
    while (line <= endLine && offset < bytesRead) {
      const lf = chunk.indexOf(10, offset);
      if (lf === -1) {
        offset = bytesRead;
        break;
      }
      offset = lf + 1;
      line++;
      if (line === startLine) start = offset;
    }
    if (offset > start) {
      retained += offset - start;
      if (retained > MAX_RANGE_BYTES) throw new Error('Range exceeds 1 MiB. Request fewer lines.');
      parts.push(Buffer.from(chunk.subarray(start, offset)));
    }
  }
  return Buffer.concat(parts, retained).toString('utf8');
}

export async function readLog(args: Record<string, unknown> | undefined): Promise<string> {
  const name = args?.['file'];
  if (
    typeof name !== 'string' ||
    !/^[\w-][\w.-]*\.(?:log|jsonl)(?:\.\d+)?$/i.test(name) ||
    /^(?:con|prn|aux|nul|com\d|lpt\d)\./i.test(name)
  ) {
    throw new Error(
      'file must be a log filename under WQM_LOG_DIR (.log or .jsonl, optional numeric rotation)'
    );
  }
  const mode = args?.['mode'] === undefined ? 'compact' : args['mode'];
  if (
    mode !== 'raw' &&
    mode !== 'compact' &&
    mode !== 'template' &&
    mode !== 'interleaved' &&
    mode !== 'auto'
  ) {
    throw new Error('mode must be compact, template, interleaved, auto or raw');
  }
  const groupBy = args?.['groupBy'] === undefined ? 'block' : args['groupBy'];
  if (args?.['groupBy'] !== undefined && mode !== 'interleaved') {
    throw new Error('groupBy requires interleaved mode');
  }
  if (groupBy !== 'block' && groupBy !== 'window')
    throw new Error('groupBy must be block or window');
  const start = lineNumber(args?.['startLine'] === undefined ? 1 : args['startLine'], 'startLine');
  const end = lineNumber(
    args?.['endLine'] === undefined ? start + DEFAULT_LINES - 1 : args['endLine'],
    'endLine'
  );
  if (end < start || end - start >= MAX_LINES)
    throw new Error(`Select 1 to ${MAX_LINES} consecutive lines`);

  const path = join(getLogDirectory(), name);
  const entry = await lstat(path);
  if (!entry.isFile()) throw new Error('Expected a regular log file');
  const handle = await open(
    path,
    constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0)
  );
  try {
    const opened = await handle.stat();
    if (!opened.isFile() || opened.dev !== entry.dev || opened.ino !== entry.ino) {
      throw new Error('Log file changed while opening. Retry the read.');
    }
    const text = await readRange(handle, opened.size, start, end);
    return mode === 'raw' ? text : formatLog(text, name, start, mode, groupBy);
  } finally {
    await handle.close();
  }
}
