import { constants } from 'node:fs';
import { lstat, open, type FileHandle } from 'node:fs/promises';
import { join } from 'node:path';

import { formatLog } from './read-log-format.js';
import { recordReadLogUsage } from '../telemetry/metrics.js';
import { READ_LOG_MODES } from '../tool-definitions/read-log.js';
import { getLogDirectory } from '../utils/paths.js';

const DEFAULT_LINES = 200;
const MAX_LINES = 2000;
const MAX_RANGE_BYTES = 1024 * 1024;

interface LogRange {
  bytes: Buffer;
  lastLine: number | null;
}

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
): Promise<LogRange> {
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
  const bytes = Buffer.concat(parts, retained);
  // After a final LF the scanner points one line beyond the last retained byte.
  const lastLine = retained === 0 ? null : line - Number(bytes.at(-1) === 0x0a);
  return { bytes, lastLine };
}

export async function readLog(
  args: Record<string, unknown> | undefined,
  logFields?: Record<string, unknown>
): Promise<string> {
  const requested = args?.['mode'];
  const mode = requested === undefined ? 'compact' : READ_LOG_MODES.find((m) => m === requested);
  if (logFields && mode) logFields.mode = mode;
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
  if (!mode) {
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
  if (logFields) Object.assign(logFields, { requested_start_line: start, requested_end_line: end });

  const path = join(getLogDirectory(), name);
  const entry = await lstat(path);
  if (!entry.isFile()) throw new Error('Expected a regular log file');
  const handle = await open(
    path,
    constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0)
  );
  let range: LogRange;
  try {
    const opened = await handle.stat();
    if (!opened.isFile() || opened.dev !== entry.dev || opened.ino !== entry.ino) {
      throw new Error('Log file changed while opening. Retry the read.');
    }
    range = await readRange(handle, opened.size, start, end);
  } finally {
    await handle.close();
  }
  const text = range.bytes.toString('utf8');
  const output = mode === 'raw' ? text : formatLog(text, name, start, mode, groupBy);
  const bytesOut = Buffer.byteLength(output);
  if (logFields) {
    Object.assign(logFields, {
      start_line: range.lastLine === null ? null : start,
      end_line: range.lastLine,
      bytes_in: range.bytes.length,
      bytes_out: bytesOut,
    });
  }
  recordReadLogUsage(mode, range.bytes.length, bytesOut);
  return output;
}
