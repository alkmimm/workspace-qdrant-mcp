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
 */
function matchesFloatingGlob(value: string, glob: string): boolean {
  return directoryAwareGlobs(glob).some(
    (g) => matchesGlob(value, g) || matchesGlob(value, `**/${g}`)
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
 * The linked-worktree subtree of a repo. Worktree branch content is stored
 * MAIN-ANCHORED (the daemon ignore-gates this subtree), so a read result whose
 * path is a LITERAL `.claude/worktrees/<wt>/…` is a leaked/stale generation that
 * is never the intended representation — and, worse, points into ANOTHER
 * worktree's checkout, so reading or editing it touches the wrong tree (field
 * feedback 2026-08-10, DOC-V2). The read surfaces drop these by default for a
 * main-tenant caller; see {@link isWorktreeSubtreePath}.
 */
export const WORKTREE_SUBTREE_GLOB = '.claude/worktrees/**';

/** True when a path lies inside any repo's `.claude/worktrees/<wt>/` subtree. */
export function isWorktreeSubtreePath(value: string): boolean {
  return matchesPathExclude(value, WORKTREE_SUBTREE_GLOB);
}
