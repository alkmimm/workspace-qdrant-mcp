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
