const TIMESTAMP = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})(?=[ \t])/;
const BLOCK_LINES = 100;
const SHORT_AUTO_LINES = 16;
const VARIABLE_ID = '<id>';

type SummaryMode = 'compact' | 'template' | 'interleaved';
type GroupBy = 'block' | 'window';

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

// A structured key keeps literal text distinct from a parsed queue ID, even
// when the log message contains a placeholder or a separator itself.
function eventKey(event: LogEvent): string {
  return JSON.stringify([
    Boolean(event.timestamp),
    Boolean(event.id),
    event.beforeId,
    event.afterId,
  ]);
}

function groupIndices(keys: string[], mode: SummaryMode, groupBy: GroupBy): number[][] {
  const groups: number[][] = [];
  const byKey = new Map<string, number[]>();
  let previousKey: string | undefined;
  for (const [index, key] of keys.entries()) {
    let group: number[] | undefined;
    if (mode === 'interleaved') {
      if (groupBy === 'block' && index % BLOCK_LINES === 0) byKey.clear();
      group = byKey.get(key);
    } else if (key === previousKey) {
      group = groups.at(-1);
    }
    if (!group) {
      group = [];
      groups.push(group);
      if (mode === 'interleaved') byKey.set(key, group);
    }
    group.push(index);
    previousKey = key;
  }
  return groups;
}

// Indices arrive in source order. Coalesce only adjacent absolute lines.
function lineRefs(indices: number[], startLine: number): string {
  const ranges: string[] = [];
  let i = 0;
  while (i < indices.length) {
    const first = startLine + indices[i]!;
    while (i + 1 < indices.length && indices[i + 1] === indices[i]! + 1) i++;
    const last = startLine + indices[i]!;
    ranges.push(first === last ? String(first) : `${first}-${last}`);
    i++;
  }
  return ranges.join(',');
}

interface ParsedLog {
  lines: string[];
  events: LogEvent[];
  keys: string[];
}

function parseLines(lines: string[], mode: 'compact' | 'template'): ParsedLog {
  const events = lines.map((line) => parseEvent(line, mode));
  return { lines, events, keys: events.map(eventKey) };
}

function minimumSummaryBytes(
  log: ParsedLog,
  multipleBlocks: boolean
): { runs: number; blocks: number } {
  let runs = 0;
  let blocks = 0;
  let previousKey: string | undefined;
  const seenInBlock = new Set<string>();
  for (const [index, key] of log.keys.entries()) {
    const event = log.events[index]!;
    // Every group retains these fields from its first event. Only the ID can
    // shrink to VARIABLE_ID. Omit labels, durations and endings from the bound.
    const required =
      event.timestamp.length +
      Buffer.byteLength(event.beforeId) +
      Buffer.byteLength(event.afterId) +
      Math.min(event.id.length, VARIABLE_ID.length);
    if (key !== previousKey) runs += required;
    previousKey = key;

    if (multipleBlocks) {
      if (index % BLOCK_LINES === 0) seenInBlock.clear();
      if (!seenInBlock.has(key)) {
        seenInBlock.add(key);
        blocks += required;
      }
    }
  }
  // Compact can split a template run into more groups. Its first event still
  // contributes at least the same required bytes.
  return { runs, blocks };
}

function renderLog(
  { lines, events, keys }: ParsedLog,
  file: string,
  startLine: number,
  mode: SummaryMode,
  groupBy: GroupBy
): string {
  const interleaved = mode === 'interleaved';
  const output = interleaved ? [`file=${file}\n`] : [];
  for (const indices of groupIndices(keys, mode, groupBy)) {
    const index = indices[0]!;
    const event = events[index]!;
    if (!interleaved && indices.length === 1) {
      output.push(lines[index]!);
      continue;
    }
    let label = 'L' + lineRefs(indices, startLine);
    let id = event.id;
    let minMs = event.durationMs ?? 0;
    let maxMs = minMs;
    for (const i of indices) {
      const other = events[i]!;
      if (other.id !== event.id) id = VARIABLE_ID;
      if (other.durationMs !== undefined) {
        minMs = Math.min(minMs, other.durationMs);
        maxMs = Math.max(maxMs, other.durationMs);
      }
    }
    if (indices.length > 1) {
      label += ` ×${indices.length}`;
      const lastTimestamp = events[indices.at(-1)!]!.timestamp;
      if (lastTimestamp) label += ` through ${lastTimestamp}`;
    }
    const range = minMs === maxMs ? String(minMs) : `${minMs}–${maxMs}`;
    const duration = event.durationMs === undefined ? '' : `${range}ms`;
    const ending = !interleaved && lines[index]!.endsWith('\r\n') ? '\r\n' : '\n';
    output.push(
      `[${label}] ${event.timestamp}${event.beforeId}${id}${event.afterId}${duration}${ending}`
    );
  }
  return output.join('');
}

export function formatLog(
  text: string,
  file: string,
  startLine: number,
  mode: SummaryMode | 'auto',
  groupBy: GroupBy
): string {
  if (!text) return '';
  const lines = text.split(/(?<=\n)/);
  if (mode !== 'auto') {
    const log = parseLines(lines, mode === 'compact' ? 'compact' : 'template');
    return renderLog(log, file, startLine, mode, groupBy);
  }

  if (lines.length === 1) return text;
  const log = parseLines(lines, 'template');
  if (lines.length <= SHORT_AUTO_LINES && !log.keys.some((key, i) => key === log.keys[i - 1])) {
    // Without adjacent template matches, compact and template both reproduce the input.
    // Interleaved can only save bytes if a key occurs more than once.
    if (new Set(log.keys).size === lines.length) return text;
    const interleaved = renderLog(log, file, startLine, 'interleaved', 'block');
    return Buffer.byteLength(text) <= Buffer.byteLength(interleaved) ? text : interleaved;
  }

  const multipleBlocks = lines.length > BLOCK_LINES;
  let best = renderLog(log, file, startLine, 'interleaved', 'window');
  let bestBytes = Buffer.byteLength(best, 'utf8');
  const minimum = minimumSummaryBytes(log, multipleBlocks);
  // Visit the remaining formats in reverse tie priority: a tie wins, so only
  // a strictly larger bound permits skipping a candidate.
  for (const [candidateMode, bound] of [
    ['interleaved', minimum.blocks],
    ['template', minimum.runs],
    ['compact', minimum.runs],
  ] as const) {
    if (candidateMode === 'interleaved' && !multipleBlocks) continue;
    if (bound > bestBytes) continue;
    const parsed = candidateMode === 'compact' ? parseLines(lines, 'compact') : log;
    const candidate = renderLog(parsed, file, startLine, candidateMode, 'block');
    const bytes = Buffer.byteLength(candidate, 'utf8');
    if (bytes <= bestBytes) {
      best = candidate;
      bestBytes = bytes;
    }
  }
  return best;
}
