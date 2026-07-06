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
 * Match a path against an EXCLUDE glob, floating so it drops a dir at the repo
 * root AND at any nested depth — the forgiving behaviour a caller expects from
 * "exclude old_project/". `matchesGlob` anchors both ends, so a bare
 * `old_project/**` would only hit the root copy; also testing `** /<glob>`
 * lets a nested `pkg/old_project/x` match too. Absolute paths (the metadata
 * `file_path`) match via the same floated form. Patterns already floating
 * (`**\/…`) stay correct — the extra prefix is a no-op there.
 */
export function matchesPathExclude(value: string, glob: string): boolean {
  return matchesGlob(value, glob) || matchesGlob(value, `**/${glob}`);
}
