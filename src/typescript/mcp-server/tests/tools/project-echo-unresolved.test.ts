/**
 * An unresolved project must SAY so (#384).
 *
 * The echo helper used to return `{}` when nothing resolved, documented as
 * "the tool already reports that failure in its own words". Measured, the tools
 * did not: `scratchpad list` without `cwd` answered `total: 0` with no echo and
 * no hint — byte-identical to a genuinely empty project. It was reported from
 * the field as "the scratchpad is empty despite dozens of sessions"; the
 * scratchpad held 16 notes for one project and 52 for another.
 *
 * An absent field cannot carry that distinction. These pin that the field is
 * present and names the state.
 */

import { describe, it, expect } from 'vitest';

import {
  projectEcho,
  scopedTenantEcho,
  PROJECT_SOURCES,
  UNRESOLVED_ECHO,
  UNRESOLVED_PROJECT_HINT,
} from '../../src/tools/project-echo.js';

describe('project echo for an unresolved project', () => {
  it('names the state instead of omitting the field', () => {
    expect(projectEcho(undefined)).toEqual({ project_source: 'unresolved' });
    expect(projectEcho({})).toEqual({ project_source: 'unresolved' });
    expect(projectEcho({ projectId: '' })).toEqual({ project_source: 'unresolved' });
  });

  it('does not fabricate an id or a path it does not have', () => {
    const echo = projectEcho(undefined);
    expect(echo).not.toHaveProperty('project_id');
    expect(echo).not.toHaveProperty('project_path');
  });

  it('still echoes normally when a project DID resolve', () => {
    expect(projectEcho({ projectId: 'abc', projectPath: '/repo', source: 'projectId' }, 'abc')).toEqual(
      { project_id: 'abc', project_path: '/repo', project_source: 'projectId' }
    );
  });

  it('reports the scratchpad fallback rung as unresolved', () => {
    // `fallback` is the exact path that produced the field report.
    expect(scopedTenantEcho({ tenantId: 'global', source: 'fallback' } as never)).toEqual({
      project_source: 'unresolved',
    });
  });

  it('returns a fresh object each time so a caller cannot mutate the shared constant', () => {
    const a = projectEcho(undefined);
    a.project_id = 'scribbled';
    expect(UNRESOLVED_ECHO).toEqual({ project_source: 'unresolved' });
    expect(projectEcho(undefined)).toEqual({ project_source: 'unresolved' });
  });

  it('is a documented rung, not a value an agent cannot look up', () => {
    // The repo already asserts every PROJECT_SOURCES entry appears in
    // help("http"); this pins that the new one joined that list rather than
    // being emitted as an undocumented string.
    expect(PROJECT_SOURCES).toContain('unresolved');
  });

  it('offers a hint that says what to DO, not just what went wrong', () => {
    expect(UNRESOLVED_PROJECT_HINT).toMatch(/cwd/);
    expect(UNRESOLVED_PROJECT_HINT).toMatch(/projectId/);
    // The core correction: an empty result here is not evidence of emptiness.
    expect(UNRESOLVED_PROJECT_HINT).toMatch(/not evidence/i);
  });
});
