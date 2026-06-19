/**
 * Tests for request-context: the getEffectiveCwd resolution chain and the
 * body-cwd override decision that backs the tool `cwd` argument.
 *
 * Precedence for project detection: header > body `cwd` > WQM_DEFAULT_HOST_CWD
 * > process.cwd().
 */

import { describe, it, expect, afterEach } from 'vitest';
import {
  runWithRequestContext,
  getEffectiveCwd,
  resolveStickyCwd,
} from '../../src/utils/request-context.js';

describe('request-context', () => {
  const savedEnv = process.env.WQM_DEFAULT_HOST_CWD;
  afterEach(() => {
    if (savedEnv === undefined) {
      delete process.env.WQM_DEFAULT_HOST_CWD;
    } else {
      process.env.WQM_DEFAULT_HOST_CWD = savedEnv;
    }
  });

  describe('getEffectiveCwd', () => {
    it('prefers the request-context host cwd (header) above env and process.cwd', () => {
      process.env.WQM_DEFAULT_HOST_CWD = '/env/default';
      const got = runWithRequestContext({ hostCwd: '/from/header' }, () => getEffectiveCwd());
      expect(got).toBe('/from/header');
    });

    it('falls back to WQM_DEFAULT_HOST_CWD when no header is bound', () => {
      process.env.WQM_DEFAULT_HOST_CWD = '/env/default';
      expect(getEffectiveCwd()).toBe('/env/default');
    });

    it('falls back to process.cwd() when neither header nor env is set', () => {
      delete process.env.WQM_DEFAULT_HOST_CWD;
      expect(getEffectiveCwd()).toBe(process.cwd());
    });
  });

  describe('resolveStickyCwd', () => {
    it('binds the body cwd and remembers it when no header is present', () => {
      expect(resolveStickyCwd({ bodyCwd: 'C:\\Users\\x\\proj' })).toEqual({
        bind: 'C:\\Users\\x\\proj',
        sticky: 'C:\\Users\\x\\proj',
      });
    });

    it('header wins: never rebinds, but still becomes the sticky value', () => {
      // The header is already bound by the transport, so `bind` stays undefined;
      // it is recorded as sticky so later header-less calls reuse it.
      expect(
        resolveStickyCwd({ headerCwd: '/from/header', bodyCwd: 'C:\\Users\\x\\proj' })
      ).toEqual({ sticky: '/from/header' });
    });

    it('falls back to the session sticky cwd when header and body are absent', () => {
      // No explicit source this call, so it does not overwrite the sticky value;
      // it only rebinds the remembered one for project detection.
      expect(resolveStickyCwd({ stickyCwd: '/remembered/proj' })).toEqual({
        bind: '/remembered/proj',
      });
    });

    it('body cwd overrides a stale sticky value and refreshes it', () => {
      expect(resolveStickyCwd({ bodyCwd: '/new/proj', stickyCwd: '/old/proj' })).toEqual({
        bind: '/new/proj',
        sticky: '/new/proj',
      });
    });

    it('returns an empty resolution when nothing is available (clean miss → fail-fast)', () => {
      expect(resolveStickyCwd({})).toEqual({});
      expect(resolveStickyCwd({ bodyCwd: '', stickyCwd: null })).toEqual({});
    });

    it('ignores empty strings for every source', () => {
      expect(resolveStickyCwd({ headerCwd: '', bodyCwd: '', stickyCwd: '' })).toEqual({});
    });
  });
});
