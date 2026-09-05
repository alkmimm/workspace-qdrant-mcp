/**
 * Read-side project echo (project_id / project_path / project_source) and the
 * cwd-provenance plumbing it relies on (RequestContext.cwdSource, set from
 * resolveStickyCwd's bindSource).
 *
 * Field feedback: a cwd-less `search` answered from the previous repo via the
 * session's sticky cwd and nothing in the envelope said so. `project_source`
 * is the field that makes that legible: "sticky-cwd" means "you did not pass
 * a cwd on this call; a remembered one was used".
 */
import { describe, it, expect } from 'vitest';
import { projectEcho } from '../../src/tools/project-echo.js';
import { resolveStickyCwd, runWithRequestContext } from '../../src/utils/request-context.js';

describe('projectEcho', () => {
  it('is empty when no project resolved (the tool reports that itself)', () => {
    expect(projectEcho(undefined)).toEqual({});
    expect(projectEcho({ projectId: undefined, projectPath: '/p' })).toEqual({});
  });

  it('reports source "projectId" for an explicit tenant id', () => {
    expect(projectEcho({ projectId: 't1', projectPath: '/p' }, 't1')).toEqual({
      project_id: 't1',
      project_path: '/p',
      project_source: 'projectId',
    });
  });

  it('omits project_path when the registry has none (never fabricated)', () => {
    expect(projectEcho({ projectId: 't1' })).toEqual({ project_id: 't1', project_source: 'cwd' });
  });

  it('reports "cwd" outside a request context and for header/body cwd', () => {
    expect(projectEcho({ projectId: 't1' }).project_source).toBe('cwd');
    const viaBody = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'body' }, () =>
      projectEcho({ projectId: 't1' })
    );
    expect(viaBody.project_source).toBe('cwd');
    const viaHeader = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'header' }, () =>
      projectEcho({ projectId: 't1' })
    );
    expect(viaHeader.project_source).toBe('cwd');
  });

  it('reports "sticky-cwd" when the cwd was remembered from an earlier call', () => {
    const echo = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'sticky' }, () =>
      projectEcho({ projectId: 't1', projectPath: '/repo' })
    );
    expect(echo).toEqual({ project_id: 't1', project_path: '/repo', project_source: 'sticky-cwd' });
  });

  it('an explicit projectId wins over a sticky cwd', () => {
    const echo = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'sticky' }, () =>
      projectEcho({ projectId: 't1' }, 't1')
    );
    expect(echo.project_source).toBe('projectId');
  });
});

describe('resolveStickyCwd bindSource', () => {
  it('names the body as the source when the body cwd binds', () => {
    expect(resolveStickyCwd({ bodyCwd: '/a', stickyCwd: '/b' })).toEqual({
      sticky: '/a',
      bind: '/a',
      bindSource: 'body',
    });
  });

  it('names the sticky cwd as the source when only it binds', () => {
    expect(resolveStickyCwd({ stickyCwd: '/b' })).toEqual({ bind: '/b', bindSource: 'sticky' });
  });

  it('binds nothing (and names no source) when the header is present', () => {
    expect(resolveStickyCwd({ headerCwd: '/h', bodyCwd: '/a' })).toEqual({ sticky: '/h' });
  });

  it('binds nothing when there is no cwd at all', () => {
    expect(resolveStickyCwd({})).toEqual({});
  });
});
