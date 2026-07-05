/**
 * Tests for the shared exact-search whitespace-sensitivity hint.
 */

import { describe, it, expect } from 'vitest';
import { isMultiTokenLiteral, whitespaceSensitivityHint } from '../../src/tools/exact-hints.js';

describe('isMultiTokenLiteral', () => {
  it('is true for a pattern with content-bearing internal whitespace', () => {
    expect(isMultiTokenLiteral('final currentUserProvider =')).toBe(true);
    expect(isMultiTokenLiteral('a b')).toBe(true);
    expect(isMultiTokenLiteral('foo\tbar')).toBe(true);
  });

  it('is false for a single token or empty/whitespace-only input', () => {
    expect(isMultiTokenLiteral('currentUserProvider')).toBe(false);
    expect(isMultiTokenLiteral('')).toBe(false);
    expect(isMultiTokenLiteral('   ')).toBe(false);
    // Leading/trailing space around a single token is not "internal".
    expect(isMultiTokenLiteral(' foo ')).toBe(false);
  });
});

describe('whitespaceSensitivityHint', () => {
  it('returns null for a single-token pattern (spacing cannot be the cause)', () => {
    expect(whitespaceSensitivityHint('currentUserProvider', true)).toBeNull();
    expect(whitespaceSensitivityHint('currentUserProvider', false)).toBeNull();
  });

  it('suggests regex:true with \\s+ for a regex-capable tool (grep)', () => {
    const hint = whitespaceSensitivityHint('final currentUserProvider =', true);
    expect(hint).not.toBeNull();
    expect(hint).toContain('regex:true');
    expect(hint).toContain('whitespace-sensitive');
    // The example replaces each whitespace run with \s+.
    expect(hint).toContain('final\\s+currentUserProvider\\s+=');
  });

  it('points at the grep tool for a tool without regex (search exact)', () => {
    const hint = whitespaceSensitivityHint('final currentUserProvider =', false);
    expect(hint).not.toBeNull();
    expect(hint).toContain('grep tool');
    expect(hint).toContain('no regex mode');
    expect(hint).toContain('final\\s+currentUserProvider\\s+=');
  });

  it('collapses tabs and multiple spaces in the example', () => {
    const hint = whitespaceSensitivityHint('foo\t  bar', true);
    expect(hint).toContain('foo\\s+bar');
  });
});
