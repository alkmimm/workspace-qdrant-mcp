/**
 * Whole-project `summary` assembly for the list tool.
 *
 * `summary` is the layout entry point the tool description (and help) send
 * agents to first. It used to be rendered from the same paged window as
 * `tree`/`flat` — the first `limit` (200) files in relative-path order — so on
 * any repo with more files than one page the overview silently omitted every
 * top-level directory that sorted after the window. Measured on this repo
 * (1,924 indexed files): `src/` (1,635 files) never appeared at the default
 * limit, `stats.languages` lacked rust and typescript, and `limit:500`,
 * `depth:2` or `path:"src"` did not help — the page just moved. A layout view
 * built from a fraction of the files is not a partial answer, it is a wrong
 * one: agents paged ~10 times or fell back to native tools.
 *
 * Summary now scans EVERY matching file (path-only rows, no bodies — cheap)
 * and aggregates directory counts over the full set. `limit` keeps its meaning
 * as a cap on rendered ENTRIES (directories) — clamped to SUMMARY_MAX_ENTRIES,
 * not to the paged formats' MAX_LIMIT — and when the requested depth would
 * exceed it the DEPTH shrinks rather than the walk being cut mid-way: the walk
 * is depth-first, so a cap applied inside one render lets the sub-directories
 * of an early top-level dir starve every later top-level dir — the same
 * "src/ never appeared" shape this module exists to fix. The shared byte budget
 * still applies (entries are cut from the end and reported), and there is no
 * cursor: a summary is one shot. `tree` and `flat` stay paged.
 */
import type {
  TrackedFileEntry,
  SubmoduleEntry,
} from '../../clients/tracked-files-queries/index.js';
import type { ComponentSummary, ListOptions, ListResponse } from '../list-files-types.js';
import { DEFAULT_LIMIT } from '../list-files-types.js';
import { DEFAULT_MAX_RESPONSE_BYTES } from '../search-types.js';
import { applyByteBudget } from '../response-budget.js';
import { buildTree } from './tree-builder.js';
import { countFolders } from './filters.js';
import { renderSummary } from './renderers.js';

/**
 * Hard cap on rows scanned for one summary. Far above any repo this server
 * indexes today; a repo beyond it still gets a summary, flagged `truncated`
 * with a message saying the aggregate is a floor.
 */
export const SUMMARY_SCAN_CAP = 50_000;

/**
 * Cap on rendered directory entries for one summary. The tool's `limit` is
 * clamped to THIS (the paged formats clamp to MAX_LIMIT=500, which would make
 * the trailing note's "raise limit" advice a dead end on a monorepo with more
 * than 500 directories); the note names it.
 */
export const SUMMARY_MAX_ENTRIES = 5_000;

export interface SummaryAssembly {
  /** Every matching file (up to SUMMARY_SCAN_CAP), not a page. */
  files: TrackedFileEntry[];
  submodules: SubmoduleEntry[];
  basePath: string;
  depth: number;
  /** The caller's raw `limit` option (before the paged formats' clamp). */
  requestedLimit: number | undefined;
  totalMatching: number;
  projectPath: string | null;
  componentSummaries: ComponentSummary[] | undefined;
  options: ListOptions;
  /** True when the scan hit SUMMARY_SCAN_CAP — counts are a lower bound. */
  partialScan: boolean;
}

export function assembleSummaryResponse(a: SummaryAssembly): ListResponse {
  const root = buildTree(a.files, a.submodules, a.basePath);
  const entryCap = Math.min(Math.max(a.requestedLimit ?? DEFAULT_LIMIT, 1), SUMMARY_MAX_ENTRIES);

  // Render unbounded at the requested depth; while the entry count exceeds the
  // cap, shrink the depth (a complete overview at a shallower depth is honest;
  // a depth-first walk cut at the cap is not). Depth 1 is the floor.
  let depth = a.depth;
  let full = renderSummary(root, depth, Number.MAX_SAFE_INTEGER);
  while (full.count > entryCap && depth > 1) {
    depth -= 1;
    full = renderSummary(root, depth, Number.MAX_SAFE_INTEGER);
  }

  // walkSummary emits one line per entry in a fixed order and stops at the cap,
  // so a capped rendering is exactly a line prefix of the unbounded one: apply
  // the entry cap and the byte budget by slicing lines — no re-render.
  const lines = full.text.length > 0 ? full.text.split('\n') : [];
  const capped = lines.slice(0, entryCap);
  const budget = a.options.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES;
  const { kept, dropped: budgetCut } = applyByteBudget(capped, (l) => l.length + 1, budget);
  const entriesDropped = lines.length - kept.length;
  const depthReduced = depth < a.depth;
  const truncated = entriesDropped > 0 || depthReduced || a.partialScan;

  const notes: string[] = [];
  if (depthReduced) {
    notes.push(
      `depth reduced from ${a.depth} to ${depth} so every directory at that depth fits within limit ${entryCap}`
    );
  }
  if (entriesDropped > 0) {
    notes.push(
      `${entriesDropped} more director${entriesDropped === 1 ? 'y' : 'ies'} not shown — ` +
        `raise limit (up to ${SUMMARY_MAX_ENTRIES})${budgetCut > 0 ? ' / maxResponseBytes' : ''}, lower depth, or narrow with path`
    );
  }
  if (a.partialScan) {
    notes.push(
      `aggregated over the first ${a.files.length} of ${a.totalMatching} matching files (scan cap) — counts are a floor`
    );
  }
  const text = kept.join('\n');
  const listing = notes.length > 0 ? `${text}\n... (${notes.join('; ')})` : text;

  const languageSet = new Set<string>();
  for (const f of a.files) {
    if (f.language !== null && f.language !== '') languageSet.add(f.language);
  }

  const response: ListResponse = {
    success: true,
    projectPath: a.projectPath,
    basePath: a.basePath || '.',
    format: 'summary',
    listing,
    stats: {
      files: a.files.length,
      folders: countFolders(root),
      languages: Array.from(languageSet).sort(),
      truncated,
      totalMatching: a.totalMatching,
      ...(a.componentSummaries ? { components: a.componentSummaries } : {}),
    },
  };
  if (budgetCut > 0) {
    response.budget_truncated = { dropped: budgetCut };
    response.message =
      `Response byte budget dropped ${budgetCut} trailing director${budgetCut === 1 ? 'y' : 'ies'} ` +
      `from the summary — lower depth, narrow with path/pattern, or raise maxResponseBytes ` +
      `(summary has no cursor: it aggregates the whole matching set in one shot).`;
  }
  return response;
}
