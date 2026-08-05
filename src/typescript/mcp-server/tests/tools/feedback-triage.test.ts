/**
 * Field-feedback triage (worktree sessions):
 *   A  — a regex metacharacter in a `regex:false` pattern must lead the empty
 *        diagnosis with "retry regex:true", not the generic absence/index-lag
 *        caveat (a grep for `a|b` returned 0 and nearly read as "not indexed").
 *   D  — a caller whose cwd is inside a linked worktree gets a translation note
 *        (result paths are MAIN-anchored).
 */

import { describe, it, expect } from 'vitest';
import {
  regexMetacharInLiteral,
  diagnoseEmptyResult,
} from '../../src/tools/empty-diagnosis.js';
import { worktreeReadNote } from '../../src/tools/worktree-note.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';

describe('A — regexMetacharInLiteral', () => {
  it('flags alternation, char-class escapes, quantified wildcards, groups', () => {
    expect(regexMetacharInLiteral('build_outputs|Route')).toBe('|');
    expect(regexMetacharInLiteral('foo\\s+bar')).toBe('\\s');
    expect(regexMetacharInLiteral('get.*Reason')).toBe('.*');
    expect(regexMetacharInLiteral('a.+b')).toBe('.+');
    expect(regexMetacharInLiteral('(?:foo)')).toBe('(?:');
  });

  it('does NOT flag a plain literal (no false positives on . or ( alone)', () => {
    expect(regexMetacharInLiteral('canToggleActive')).toBeUndefined();
    expect(regexMetacharInLiteral('ScaffoldMessenger.of(context)')).toBeUndefined();
    expect(regexMetacharInLiteral('my_symbol')).toBeUndefined();
  });
});

describe('A — diagnoseEmptyResult leads with the metacharacter (probe 0)', () => {
  it('a literal pattern with | returns the regex:true hint before anything else', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      literalPattern: 'a|b',
      regexRetryHint: 'Retry with regex:true.',
      // A path filter is ALSO set — the metachar verdict must still win (it is the
      // dominant cause of the zero and needs no query).
      branch: 'main',
      pathGlob: 'src/**/*.rs',
      pathExclude: undefined,
      searchDbReader: undefined,
      countWithoutPathFilter: async () => 5,
    });
    expect(msg).toBeDefined();
    expect(msg).toContain('`|`');
    expect(msg).toMatch(/LITERAL substring/);
    expect(msg).toContain('Retry with regex:true.');
  });

  it('a plain literal with no metachar does not trigger the probe', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      literalPattern: 'plainLiteral',
      branch: undefined,
      pathGlob: undefined,
      pathExclude: undefined,
      searchDbReader: undefined,
      countWithoutPathFilter: async () => 0,
    });
    expect(msg).toBeUndefined();
  });
});

describe('D — worktreeReadNote', () => {
  it('surfaces the worktree root when cwd is inside .claude/worktrees', () => {
    const note = runWithRequestContext(
      { hostCwd: '/home/me/repo/.claude/worktrees/wt-feat/packages/x' },
      () => worktreeReadNote()
    );
    expect(note).toBeDefined();
    expect(note?.name).toBe('wt-feat');
    expect(note?.root).toBe('/home/me/repo/.claude/worktrees/wt-feat');
    expect(note?.note).toContain('/.claude/worktrees/');
  });

  it('is undefined for an ordinary (non-worktree) cwd', () => {
    const note = runWithRequestContext({ hostCwd: '/home/me/repo/src' }, () => worktreeReadNote());
    expect(note).toBeUndefined();
  });
});
