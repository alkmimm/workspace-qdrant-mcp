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
 * Floating glob match: matches the glob at the repo root AND at any nested
 * depth. `matchesGlob` anchors both ends, so a bare `old_project/**` would only
 * hit the root copy; also testing `**\/<glob>` lets a nested `pkg/old_project/x`
 * match too. Absolute paths (the metadata `file_path`) match via the same
 * floated form. Patterns already floating (`**\/…`) stay correct — the extra
 * prefix is a no-op there. Single source of truth for both the exclude and
 * include matchers so their semantics can never drift apart.
 */
function matchesFloatingGlob(value: string, glob: string): boolean {
  return matchesGlob(value, glob) || matchesGlob(value, `**/${glob}`);
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
