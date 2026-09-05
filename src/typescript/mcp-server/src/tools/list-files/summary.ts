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
 * as a cap on rendered ENTRIES (directories), the shared byte budget still
 * applies (entries are cut from the end and reported), and there is no cursor:
 * a summary is one shot. `tree` and `flat` stay paged.
 */
import type {
  TrackedFileEntry,
  SubmoduleEntry,
} from '../../clients/tracked-files-queries/index.js';
import type {
  ComponentSummary,
  FolderNode,
  ListOptions,
  ListResponse,
} from '../list-files-types.js';
import { DEFAULT_MAX_RESPONSE_BYTES } from '../search-types.js';
import { buildTree } from './tree-builder.js';
import { renderSummary } from './renderers.js';

/**
 * Hard cap on rows scanned for one summary. Far above any repo this server
 * indexes today; a repo beyond it still gets a summary, flagged `truncated`
 * with a message saying the aggregate is a floor.
 */
export const SUMMARY_SCAN_CAP = 50_000;

export interface SummaryAssembly {
  /** Every matching file (up to SUMMARY_SCAN_CAP), not a page. */
  files: TrackedFileEntry[];
  submodules: SubmoduleEntry[];
  basePath: string;
  depth: number;
  /** Cap on rendered directory entries (the tool's `limit`). */
  limit: number;
  totalMatching: number;
  projectPath: string | null;
  componentSummaries: ComponentSummary[] | undefined;
  options: ListOptions;
  /** True when the scan hit SUMMARY_SCAN_CAP — counts are a lower bound. */
  partialScan: boolean;
}

/** Directories in the tree, excluding the root itself. */
function countFolders(node: FolderNode): number {
  let n = 0;
  for (const child of node.children.values()) n += 1 + countFolders(child);
  return n;
}

export function assembleSummaryResponse(a: SummaryAssembly): ListResponse {
  const root = buildTree(a.files, a.submodules, a.basePath);

  // Render once unbounded to learn the full entry count, then cap to `limit`
  // and to the byte budget by bisecting the entry cap. Entries are directories
  // and the tree is in memory, so a handful of re-renders is cheap.
  const full = renderSummary(root, a.depth, Number.MAX_SAFE_INTEGER);
  let cap = Math.min(a.limit, full.count);
  let render = cap === full.count ? full : renderSummary(root, a.depth, cap);
  let budgetCut = 0;
  const budget = a.options.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES;
  if (budget > 0 && render.text.length > budget && cap > 1) {
    let lo = 1;
    let hi = cap - 1;
    let best = renderSummary(root, a.depth, 1);
    let bestCap = 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const probe = renderSummary(root, a.depth, mid);
      if (probe.text.length <= budget) {
        best = probe;
        bestCap = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    budgetCut = cap - bestCap;
    render = best;
    cap = bestCap;
  }

  const entriesDropped = full.count - render.count;
  const truncated = entriesDropped > 0 || a.partialScan;
  const notes: string[] = [];
  if (entriesDropped > 0) {
    notes.push(
      `${entriesDropped} more director${entriesDropped === 1 ? 'y' : 'ies'} not shown — ` +
        `raise limit${budgetCut > 0 ? ' / maxResponseBytes' : ''}, lower depth, or narrow with path`
    );
  }
  if (a.partialScan) {
    notes.push(
      `aggregated over the first ${a.files.length} of ${a.totalMatching} matching files (scan cap) — counts are a floor`
    );
  }
  const listing = notes.length > 0 ? `${render.text}\n... (${notes.join('; ')})` : render.text;

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
