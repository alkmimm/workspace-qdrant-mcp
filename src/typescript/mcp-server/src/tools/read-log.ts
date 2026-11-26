import { constants } from 'node:fs';
import { lstat, open, type FileHandle } from 'node:fs/promises';
import { join } from 'node:path';

import { getLogDirectory } from '../utils/paths.js';

const DEFAULT_LINES = 200;
const MAX_LINES = 2000;
const MAX_RANGE_BYTES = 1024 * 1024;
const TIMESTAMP = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})(?=[ \t])/;

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

// Match only the queue span and the success message emitted by memexd.
// Other IDs (including tenant_id), counts, error codes and paths stay literal.
const QUEUE_SPAN =
  /^([ \t]*(?:TRACE|DEBUG|INFO|WARN|ERROR)[ \t]+queue\.process_item\{queue_id=)([a-fA-F0-9]{32})(?=[ \t}])/;
const PROCESSED_ITEM =
  /^([ \t]*INFO[ \t]+workspace_qdrant_core::unified_queue_processor::processing_loop::batch_processing: Successfully processed unified item )([a-fA-F0-9]{32})( \(type=\w+, op=\w+\) in )(0|[1-9]\d*)ms$/;

interface LogEvent {
  timestamp: string;
  beforeId: string;
  id: string;
  afterId: string;
  durationMs?: number;
}

function parseEvent(line: string, mode: 'compact' | 'template'): LogEvent {
  const timestamp = TIMESTAMP.exec(line)?.[0] ?? '';
  const body = line.slice(timestamp.length).replace(/\r?\n$/, '');
  if (mode === 'template') {
    const span = QUEUE_SPAN.exec(body);
    if (span) {
      return { timestamp, beforeId: span[1]!, id: span[2]!, afterId: body.slice(span[0].length) };
    }
    const item = PROCESSED_ITEM.exec(body);
    if (item && Number.isSafeInteger(Number(item[4]))) {
      return {
        timestamp,
        beforeId: item[1]!,
        id: item[2]!,
        afterId: item[3]!,
        durationMs: Number(item[4]),
      };
    }
  }
  return { timestamp, beforeId: body, id: '', afterId: '' };
}

function sameEvent(first: LogEvent, next: LogEvent): boolean {
  return (
    Boolean(first.timestamp) === Boolean(next.timestamp) &&
    Boolean(first.id) === Boolean(next.id) &&
    first.beforeId === next.beforeId &&
    first.afterId === next.afterId
  );
}

function compact(text: string, startLine: number, mode: 'compact' | 'template'): string {
  const lines = text.split(/(?<=\n)/);
  const events = lines.map((line) => parseEvent(line, mode));
  const output: string[] = [];
  let first = 0;
  while (first < events.length) {
    const event = events[first]!;
    let id = event.id;
    let minMs = event.durationMs ?? 0;
    let maxMs = minMs;
    let next = first + 1;
    while (next < events.length && sameEvent(event, events[next]!)) {
      const other = events[next]!;
      if (other.id !== event.id) id = '<id>';
      if (other.durationMs !== undefined) {
        minMs = Math.min(minMs, other.durationMs);
        maxMs = Math.max(maxMs, other.durationMs);
      }
      next++;
    }
    let line = lines[first]!;
    if (next > first + 1) {
      const lastTimestamp = events[next - 1]!.timestamp;
      const until = lastTimestamp ? ` through ${lastTimestamp}` : '';
      output.push(`[L${startLine + first}-${startLine + next - 1} ×${next - first}${until}] `);
      if (id !== event.id || minMs !== maxMs) {
        const ending = line.endsWith('\r\n') ? '\r\n' : '\n';
        const range = minMs === maxMs ? String(minMs) : `${minMs}–${maxMs}`;
        const duration = event.durationMs === undefined ? '' : `${range}ms`;
        line = event.timestamp + event.beforeId + id + event.afterId + duration + ending;
      }
    }
    output.push(line);
    first = next;
  }
  return output.join('');
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
  if (mode !== 'raw' && mode !== 'compact' && mode !== 'template') {
    throw new Error('mode must be compact, template or raw');
  }
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
    return mode === 'raw' ? text : compact(text, start, mode);
  } finally {
    await handle.close();
  }
}
