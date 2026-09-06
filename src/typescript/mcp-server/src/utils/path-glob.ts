function escapeRegexChar(char: string): string {
  return char.replace(/[\\^$+?.()|[\]{}]/g, '\\$&');
}

export function globToRegExp(glob: string): RegExp {
  let pattern = '^';
  const normalized = glob.replace(/\\/g, '/');
  for (let index = 0; index < normalized.length; index += 1) {
    const char = normalized[index];
    if (char === undefined) continue;
    if (char === '*') {
      if (normalized[index + 1] === '*') {
        if (normalized[index + 2] === '/') {
          pattern += '(?:.*/)?';
          index += 2;
          continue;
        }
        pattern += '.*';
        index += 1;
        continue;
      }
      pattern += '[^/]*';
      continue;
    }
    if (char === '?') {
      pattern += '[^/]';
      continue;
    }
    pattern += escapeRegexChar(char);
  }
  pattern += '$';
  return new RegExp(pattern);
}

export function matchesGlob(value: string, glob: string): boolean {
  return globToRegExp(glob).test(value.replace(/\\/g, '/'));
}

/**
 * Ceiling on the patterns one glob may expand into. `{a,b}/{c,d}/{e,f}` is
 * legitimate and small; a pathological nest is not worth compiling. On
 * overflow the pattern is returned verbatim (today's behaviour: the braces
 * match literally, which is a miss, not a wrong match).
 */
const MAX_BRACE_EXPANSIONS = 256;

/** Locate the first brace group, honouring nesting. `undefined` when there is
 *  no `{`, or when it is unbalanced — an unbalanced brace is a literal. */
function findBraceGroup(glob: string): { open: number; close: number } | undefined {
  const open = glob.indexOf('{');
  if (open === -1) return undefined;
  let depth = 0;
  for (let index = open; index < glob.length; index += 1) {
    const char = glob[index];
    if (char === '{') depth += 1;
    else if (char === '}') {
      depth -= 1;
      if (depth === 0) return { open, close: index };
    }
  }
  return undefined;
}

/** Split a brace body on its TOP-LEVEL commas, so `{a,{b,c}}` yields
 *  `["a", "{b,c}"]` rather than splitting the nested group open. */
function splitTopLevelCommas(body: string): string[] {
  const parts: string[] = [];
  let depth = 0;
  let current = '';
  for (const char of body) {
    if (char === '{') {
      depth += 1;
      current += char;
    } else if (char === '}') {
      depth -= 1;
      current += char;
    } else if (char === ',' && depth === 0) {
      parts.push(current);
      current = '';
    } else {
      current += char;
    }
  }
  parts.push(current);
  return parts;
}

function expandBracesInto(glob: string, out: string[]): boolean {
  if (out.length >= MAX_BRACE_EXPANSIONS) return false;
  const group = findBraceGroup(glob);
  if (group === undefined) {
    out.push(glob);
    return true;
  }
  const prefix = glob.slice(0, group.open);
  const suffix = glob.slice(group.close + 1);
  for (const alternative of splitTopLevelCommas(glob.slice(group.open + 1, group.close))) {
    // Trimmed, matching the daemon's `expand_braces`: a human-typed
    // `{rs, toml}` must behave like `{rs,toml}` rather than silently becoming
    // the unmatchable `*. toml`.
    if (!expandBracesInto(`${prefix}${alternative.trim()}${suffix}`, out)) return false;
  }
  return true;
}

/**
 * Expand `{a,b}` alternation into one pattern per alternative, left to right.
 *
 * The daemon's FTS glob has supported braces since it shipped (`expand_braces`
 * in `text_search/escaping.rs`), so `grep`/`exact` answered
 * `pathGlob:"**\/*.{rs,ts}"` correctly while the TWO TypeScript engines —
 * this matcher (semantic search, `search-db-reader`) and the SQLite `GLOB`
 * clause behind `list` — treated the braces as literal characters and returned
 * ZERO, silently. Measured 2026-09-05: the same glob gave 28 grep matches and
 * 0 listed files.
 *
 * Nesting and multiple groups are handled (`{a,{b,c}}`, `{src,tests}/*.{rs,ts}`);
 * an unbalanced brace stays literal, which is what a caller who meant a
 * literal `{` gets today. Kept exported so the `list` SQL builder and the
 * daemon-side normalizer can be asserted against the same semantics.
 */
export function expandBraces(glob: string): string[] {
  const expanded: string[] = [];
  return expandBracesInto(glob, expanded) && expanded.length > 0 ? expanded : [glob];
}

/**
 * Expand a wildcard-free path literal into the globs that let it scope to a
 * DIRECTORY, not just an equally-named file. `matchesGlob('**\/tool-builders')`
 * only matches a path ENDING in `tool-builders`, so a bare directory literal
 * (`integrator-events`, `tool-builders/`) matched nothing — the field-reported
 * `pathGlob` friction, and the same end-anchoring gap the daemon's
 * `normalize_path_glob` closes. A trailing slash is unambiguously a directory
 * (subtree only); an un-suffixed literal may be a file OR a directory, so it
 * matches the exact path OR its subtree. Patterns that already carry a wildcard
 * (`* ? [ {`) keep their exact meaning.
 */
function directoryAwareGlobs(glob: string): string[] {
  if (/[*?[{]/.test(glob)) return [glob];
  const dir = glob.replace(/\/+$/, '');
  if (dir.length === 0) return [glob];
  return glob.endsWith('/') ? [`${dir}/**`] : [glob, `${glob}/**`];
}

/**
 * Floating glob match: matches the glob at the repo root AND at any nested
 * depth. `matchesGlob` anchors both ends, so a bare `old_project/**` would only
 * hit the root copy; also testing `**\/<glob>` lets a nested `pkg/old_project/x`
 * match too. Absolute paths (the metadata `file_path`) match via the same
 * floated form. Patterns already floating (`**\/…`) stay correct — the extra
 * prefix is a no-op there. A wildcard-free literal is additionally expanded via
 * {@link directoryAwareGlobs} so it scopes to a directory subtree, mirroring the
 * daemon-side `normalize_path_glob`. Single source of truth for both the exclude
 * and include matchers so their semantics can never drift apart.
 *
 * Brace alternation is expanded FIRST ({@link expandBraces}): each alternative
 * is then floated and directory-shaped on its own, so `{src,tests}` scopes to
 * both subtrees exactly as the two literals would separately.
 */
function matchesFloatingGlob(value: string, glob: string): boolean {
  return expandBraces(glob).some((alternative) =>
    directoryAwareGlobs(alternative).some(
      (g) => matchesGlob(value, g) || matchesGlob(value, `**/${g}`)
    )
  );
}

/**
 * Match a path against an EXCLUDE glob (`pathExclude`) — the forgiving behaviour
 * a caller expects from "exclude old_project/". Floats via
 * {@link matchesFloatingGlob}.
 */
export function matchesPathExclude(value: string, glob: string): boolean {
  return matchesFloatingGlob(value, glob);
}

/**
 * Match a path against an INCLUDE glob (`pathGlob`). Floats exactly like
 * {@link matchesPathExclude} (both delegate to {@link matchesFloatingGlob}) so a
 * relative pattern like `management/test/**` matches at any depth — parity with
 * the daemon-side FTS glob used by grep / exact search, instead of the previous
 * both-ends anchored `matchesGlob` under which it only matched when the path
 * STARTED with `management/test/`.
 */
export function matchesPathInclude(value: string, glob: string): boolean {
  return matchesFloatingGlob(value, glob);
}

/**
 * The linked-worktree subtree of a repo.
 *
 * A result path under `.claude/worktrees/<wt>/…` is usually a leaked/stale
 * generation or a first-ingest-anchored shared point — for a MAIN caller it
 * points into another session's checkout (field feedback 2026-08-10, DOC-V2),
 * so the read surfaces drop these by default for main-tenant callers.
 *
 * It is NOT universally garbage: new-on-branch and divergent worktree content
 * (read_root ingests, worktree_membership B1.1/B2) legitimately lives ONLY
 * there. That is why the drop is gated on the CALLER (a worktree caller keeps
 * its own subtree) and suppressed by an explicit worktree include — do not
 * extend it unconditionally to other surfaces or push it into a Qdrant-side
 * filter, or that content becomes unreachable. Root fix is the daemon
 * persisting main-anchored paths (planned PR-7); until then this stays a
 * read-time, caller-aware measure.
 */
export const WORKTREE_SUBTREE_GLOB = '.claude/worktrees/**';

/** The literal segment that marks the worktree subtree inside a path. */
const WORKTREE_SEGMENT = '.claude/worktrees/';

/**
 * Precompiled matchers for the constant worktree glob. `isWorktreeSubtreePath`
 * runs by DEFAULT on every search/grep result for a main-tenant caller (the
 * other-worktree drop), so recompiling the same glob per result — hundreds of
 * times per query — was pure waste. Compiled once per process.
 */
const WORKTREE_SUBTREE_REGEXES: RegExp[] = directoryAwareGlobs(WORKTREE_SUBTREE_GLOB).flatMap(
  (g) => [globToRegExp(g), globToRegExp(`**/${g}`)]
);

/** True when a path lies inside any repo's `.claude/worktrees/<wt>/` subtree. */
export function isWorktreeSubtreePath(value: string): boolean {
  const normalized = value.replace(/\\/g, '/');
  return WORKTREE_SUBTREE_REGEXES.some((re) => re.test(normalized));
}

/**
 * The repo-relative view of a worktree-anchored ABSOLUTE path: the slice from
 * `.claude/worktrees/` onward (`/repo/.claude/worktrees/wt/lib/a.dart` →
 * `.claude/worktrees/wt/lib/a.dart`), or undefined when the path is not under a
 * worktree subtree.
 *
 * This exists so a caller-supplied exclude glob can be tested against a
 * worktree copy WITHOUT ever testing the raw absolute path. Floated globs make
 * raw absolutes dangerous for excludes: `docs/**` floats to `**\/docs/**`,
 * which matches a checkout that merely LIVES under `~/docs/` — every hit
 * dropped because of a host directory outside the repo. The slice keeps the
 * one absolute-only fact a glob legitimately needs (the worktree location)
 * in repo-relative coordinates, where globs are safe.
 */
export function worktreeSubpathOf(value: string): string | undefined {
  const normalized = value.replace(/\\/g, '/');
  const idx = normalized.indexOf(WORKTREE_SEGMENT);
  return idx >= 0 ? normalized.slice(idx) : undefined;
}

/** The worktree NAME (`<wt>` in `.claude/worktrees/<wt>/…`) a path lies under,
 *  or undefined when it is not a worktree path. */
export function worktreeNameOf(value: string): string | undefined {
  const sub = worktreeSubpathOf(value);
  if (sub === undefined) return undefined;
  const name = sub.slice(WORKTREE_SEGMENT.length).split('/')[0];
  return name !== undefined && name.length > 0 ? name : undefined;
}
