/**
 * Read-side project echo (project_id / project_path / project_source) and the
 * cwd-provenance plumbing it relies on (RequestContext.cwdSource, set from
 * resolveStickyCwd's bindSource through withBoundCwd).
 *
 * Field feedback: a cwd-less `search` answered from the previous repo via the
 * session's sticky cwd and nothing in the envelope said so. `project_source`
 * is the field that makes that legible: "sticky-cwd" means "you did not pass
 * a cwd on this call; a remembered one was used"; "sole-project" means "your
 * cwd matched nothing and the only registered project answered".
 */
import { describe, it, expect } from 'vitest';
import { projectEcho, scopedTenantEcho } from '../../src/tools/project-echo.js';
import {
  resolveStickyCwd,
  runWithRequestContext,
  withBoundCwd,
} from '../../src/utils/request-context.js';

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
    // The resolver's own rung says so too (graph passes the identity only).
    expect(projectEcho({ projectId: 't1', source: 'projectId' }).project_source).toBe('projectId');
  });

  it('omits project_path when the registry has none (never fabricated)', () => {
    expect(projectEcho({ projectId: 't1' })).toEqual({ project_id: 't1', project_source: 'cwd' });
  });

  it('reports "cwd" outside a request context (stdio) and for header/body cwd', () => {
    expect(projectEcho({ projectId: 't1', source: 'cwd' }).project_source).toBe('cwd');
    const viaBody = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'body' }, () =>
      projectEcho({ projectId: 't1', source: 'cwd' })
    );
    expect(viaBody.project_source).toBe('cwd');
    const viaHeader = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'header' }, () =>
      projectEcho({ projectId: 't1', source: 'cwd' })
    );
    expect(viaHeader.project_source).toBe('cwd');
  });

  it('reports "sticky-cwd" when the cwd was remembered from an earlier call', () => {
    const echo = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'sticky' }, () =>
      projectEcho({ projectId: 't1', projectPath: '/repo', source: 'cwd' })
    );
    expect(echo).toEqual({ project_id: 't1', project_path: '/repo', project_source: 'sticky-cwd' });
  });

  it('reports "sole-project" when the cwd matched nothing and the only registered project answered', () => {
    expect(projectEcho({ projectId: 't1', source: 'sole-project' }).project_source).toBe(
      'sole-project'
    );
    // The resolver's rung wins over the request's cwd provenance: a sticky cwd
    // that matched nothing is still a sole-project answer, not a cwd one.
    const sticky = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'sticky' }, () =>
      projectEcho({ projectId: 't1', source: 'sole-project' })
    );
    expect(sticky.project_source).toBe('sole-project');
  });

  it('reports "server-default" for an HTTP call that bound no cwd at all', () => {
    const echo = runWithRequestContext({ mcpSessionId: 's1' }, () =>
      projectEcho({ projectId: 't1', source: 'cwd' })
    );
    expect(echo.project_source).toBe('server-default');
  });

  it('an explicit projectId wins over a sticky cwd', () => {
    const echo = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'sticky' }, () =>
      projectEcho({ projectId: 't1' }, 't1')
    );
    expect(echo.project_source).toBe('projectId');
  });
});

describe('scopedTenantEcho (write-side resolution, scratchpad list)', () => {
  it('maps the projectId and session rungs, and omits the global fallback', () => {
    expect(scopedTenantEcho({ tenantId: 't1', projectPath: '/p', source: 'projectId' })).toEqual({
      project_id: 't1',
      project_path: '/p',
      project_source: 'projectId',
    });
    expect(scopedTenantEcho({ tenantId: 't1', source: 'session' })).toEqual({
      project_id: 't1',
      project_source: 'session',
    });
    expect(scopedTenantEcho({ tenantId: 'global', source: 'fallback' })).toEqual({});
  });

  it('labels the cwd rung from the request provenance', () => {
    const echo = runWithRequestContext({ hostCwd: '/repo', cwdSource: 'sticky' }, () =>
      scopedTenantEcho({ tenantId: 't1', source: 'cwd' })
    );
    expect(echo.project_source).toBe('sticky-cwd');
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

describe('withBoundCwd → projectEcho (the link server.ts relies on)', () => {
  it('carries the sticky provenance into the bound context so the echo can report it', () => {
    const { bind, bindSource } = resolveStickyCwd({ stickyCwd: '/remembered/proj' });
    const bound = withBoundCwd({ mcpSessionId: 's1' }, bind ?? '', bindSource);
    expect(bound).toEqual({ mcpSessionId: 's1', hostCwd: '/remembered/proj', cwdSource: 'sticky' });
    const echo = runWithRequestContext(bound, () =>
      projectEcho({ projectId: 't1', source: 'cwd' })
    );
    expect(echo.project_source).toBe('sticky-cwd');
  });

  it('a body cwd binds as "cwd" and preserves the transport-bound session id', () => {
    const { bind, bindSource } = resolveStickyCwd({ bodyCwd: '/new/proj', stickyCwd: '/old' });
    const bound = withBoundCwd({ mcpSessionId: 's1' }, bind ?? '', bindSource);
    expect(bound.mcpSessionId).toBe('s1');
    expect(bound.cwdSource).toBe('body');
    const echo = runWithRequestContext(bound, () =>
      projectEcho({ projectId: 't1', source: 'cwd' })
    );
    expect(echo.project_source).toBe('cwd');
  });
});
