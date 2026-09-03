/**
 * Write-path tenant precedence — the cwd a caller passes must win over the
 * session's sticky project.
 *
 * The READ surfaces resolve a tenant with `resolveProjectIdentity`: explicit
 * `projectId` -> the effective cwd (header > body `cwd` > session sticky cwd),
 * and NEVER the session's activated project. The write paths used to consult
 * `sessionState.projectId` BEFORE the cwd, so a `store` carrying an explicit
 * `cwd` for project A landed in project B whenever the session was still
 * activated for B (a previous cwd, or an in-flight `ensureClientProjectActive`).
 * That is a silent misroute: the note is written, reported as success, and is
 * then invisible to A's tenant-strict recall lane.
 *
 * These tests pin the shared precedence across every project-scoped write.
 */

import { describe, it, expect, vi } from 'vitest';
import { storeScratchpad, storeUrl } from '../src/store-handlers.js';
import { resolveScopedTenant } from '../src/tools/tenant-scope.js';
import { routeTool } from '../src/tool-dispatcher.js';
import { runWithRequestContext } from '../src/utils/request-context.js';
import { TENANT_GLOBAL } from '../src/constants/tenants.js';
import type { ServerComponents } from '../src/server-factory.js';
import type { SessionState } from '../src/server-types.js';
import type { SqliteStateManager } from '../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../src/utils/project-detector.js';

const WQ = {
  projectId: '367157a01d98',
  projectPath: '/home/alkmimm/respositorios/workspace-qdrant-mcp',
};
const DOC = { projectId: '685731a5e037', projectPath: '/home/alkmimm/respositorios/DOC-V2' };

function mockStateManager(): SqliteStateManager {
  return {
    enqueueUnified: vi.fn().mockResolvedValue({ status: 'ok', data: { queueId: 'q-1' } }),
    upsertScratchpadMirror: vi.fn(),
    getProjectById: vi.fn().mockImplementation((projectId: string) => {
      const known = [WQ, DOC].find((p) => p.projectId === projectId);
      return { status: 'ok', data: known ? { project_path: known.projectPath } : null };
    }),
  } as unknown as SqliteStateManager;
}

/** Resolves a project from a cwd prefix, like the real path-matching detector. */
function pathDetector(): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockImplementation(async (cwd: string) => {
      if (typeof cwd !== 'string') return null;
      if (cwd.startsWith(WQ.projectPath)) return { ...WQ, isActive: false };
      if (cwd.startsWith(DOC.projectPath)) return { ...DOC, isActive: true };
      return null;
    }),
  } as unknown as ProjectDetector;
}

function nullDetector(): ProjectDetector {
  return { getProjectInfo: vi.fn().mockResolvedValue(null) } as unknown as ProjectDetector;
}

/** The tenant_id is the 3rd positional arg of enqueueUnified. */
function tenantOf(sm: SqliteStateManager): unknown {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock.calls[0][2];
}

function session(projectId: string | null) {
  return { projectId, currentBranch: 'main', isWorktree: false };
}

describe('resolveScopedTenant — shared write-path precedence', () => {
  it('prefers an explicit projectId over the cwd and the session project', async () => {
    const detector = pathDetector();
    const scoped = await runWithRequestContext({ hostCwd: DOC.projectPath }, () =>
      resolveScopedTenant({
        explicitProjectId: WQ.projectId,
        projectDetector: detector,
        sessionProjectId: DOC.projectId,
        stateManager: mockStateManager(),
      })
    );

    expect(scoped.tenantId).toBe(WQ.projectId);
    expect(scoped.projectPath).toBe(WQ.projectPath);
    expect(scoped.source).toBe('projectId');
    expect(detector.getProjectInfo).not.toHaveBeenCalled();
  });

  it('prefers the cwd project over a differing sticky session project', async () => {
    const scoped = await runWithRequestContext({ hostCwd: WQ.projectPath }, () =>
      resolveScopedTenant({
        projectDetector: pathDetector(),
        sessionProjectId: DOC.projectId,
        stateManager: mockStateManager(),
      })
    );

    expect(scoped.tenantId).toBe(WQ.projectId);
    expect(scoped.projectPath).toBe(WQ.projectPath);
    expect(scoped.source).toBe('cwd');
  });

  it('falls back to the session project when the cwd resolves nothing', async () => {
    const scoped = await resolveScopedTenant({
      projectDetector: nullDetector(),
      sessionProjectId: DOC.projectId,
      stateManager: mockStateManager(),
    });

    expect(scoped.tenantId).toBe(DOC.projectId);
    expect(scoped.projectPath).toBe(DOC.projectPath);
    expect(scoped.source).toBe('session');
  });

  it('falls back to the session project when detection throws', async () => {
    const detector = {
      getProjectInfo: vi.fn().mockRejectedValue(new Error('boom')),
    } as unknown as ProjectDetector;

    const scoped = await resolveScopedTenant({
      projectDetector: detector,
      sessionProjectId: DOC.projectId,
    });

    expect(scoped.tenantId).toBe(DOC.projectId);
    expect(scoped.source).toBe('session');
  });

  it('falls back to the global tenant when nothing resolves', async () => {
    const scoped = await resolveScopedTenant({
      projectDetector: nullDetector(),
      sessionProjectId: null,
    });

    expect(scoped.tenantId).toBe(TENANT_GLOBAL);
    expect(scoped.source).toBe('fallback');
    expect(scoped.projectPath).toBeUndefined();
  });
});

describe('store(type:"scratchpad") — tenant follows the caller cwd', () => {
  it('routes to the cwd project, not the sticky session project', async () => {
    const sm = mockStateManager();

    const res = await runWithRequestContext({ hostCwd: WQ.projectPath }, () =>
      storeScratchpad({ content: 'note', cwd: WQ.projectPath }, sm, pathDetector(), session(DOC.projectId))
    );

    expect(res.success).toBe(true);
    expect(tenantOf(sm)).toBe(WQ.projectId);
  });

  it('echoes the resolved tenant AND project path so a misroute is visible', async () => {
    const sm = mockStateManager();

    const res = await runWithRequestContext({ hostCwd: WQ.projectPath }, () =>
      storeScratchpad({ content: 'note', cwd: WQ.projectPath }, sm, pathDetector(), session(DOC.projectId))
    );

    expect(res.project_id).toBe(WQ.projectId);
    expect(res.project_path).toBe(WQ.projectPath);
    expect(res.message).toContain(WQ.projectPath);
  });

  it('still honours an explicit projectId over the cwd', async () => {
    const sm = mockStateManager();

    await runWithRequestContext({ hostCwd: DOC.projectPath }, () =>
      storeScratchpad(
        { content: 'note', cwd: DOC.projectPath, projectId: WQ.projectId },
        sm,
        pathDetector(),
        session(DOC.projectId)
      )
    );

    expect(tenantOf(sm)).toBe(WQ.projectId);
  });
});

describe('store(type:"url") — project-scoped capture follows the caller cwd', () => {
  it('routes a project-scoped URL capture to the cwd project', async () => {
    const sm = mockStateManager();

    const res = await runWithRequestContext({ hostCwd: WQ.projectPath }, () =>
      storeUrl(
        { url: 'https://example.com/doc', cwd: WQ.projectPath },
        sm,
        pathDetector(),
        session(DOC.projectId)
      )
    );

    expect(res.success).toBe(true);
    expect(tenantOf(sm)).toBe(WQ.projectId);
    expect(res.project_id).toBe(WQ.projectId);
  });

  it('keeps libraryName as the tenant for a library capture (unchanged)', async () => {
    const sm = mockStateManager();

    const res = await runWithRequestContext({ hostCwd: WQ.projectPath }, () =>
      storeUrl(
        { url: 'https://docs.rs/tokio', libraryName: 'tokio' },
        sm,
        pathDetector(),
        session(DOC.projectId)
      )
    );

    expect(res.success).toBe(true);
    expect(tenantOf(sm)).toBe('tokio');
    expect(res.project_id).toBeUndefined();
  });
});

// ── store(type:"library", forProject:true), through the real dispatcher ──────

function storeComponents(detector: ProjectDetector = pathDetector()): ServerComponents {
  return {
    daemonClient: {
      logSearchEvent: vi.fn().mockResolvedValue(undefined),
      updateSearchEvent: vi.fn().mockResolvedValue(undefined),
      updateSearchEventEconomy: vi.fn().mockResolvedValue(undefined),
    },
    projectDetector: detector,
    stateManager: mockStateManager(),
    storeTool: {
      store: vi.fn().mockImplementation(async (options: { projectId?: string }) => ({
        success: true,
        collection: 'libraries',
        message: 'Content queued for processing by daemon',
        fallback_mode: 'unified_queue',
        _tenant: options.projectId,
      })),
    },
  } as unknown as ServerComponents;
}

function storeOptionsOf(components: ServerComponents): { projectId?: string } {
  const store = components.storeTool.store as unknown as ReturnType<typeof vi.fn>;
  return store.mock.calls[0][0] as { projectId?: string };
}

describe('store(type:"library", forProject) — tenant follows the caller cwd', () => {
  it('scopes the entry to the cwd project, not the sticky session project', async () => {
    const components = storeComponents();
    const sessionState = { projectId: DOC.projectId } as SessionState;

    const result = await runWithRequestContext({ hostCwd: WQ.projectPath }, () =>
      routeTool(
        'store',
        { type: 'library', forProject: true, content: 'ref', cwd: WQ.projectPath },
        components,
        sessionState
      )
    );

    expect(storeOptionsOf(components).projectId).toBe(WQ.projectId);
    const echoed = result as { project_id?: string; project_path?: string };
    expect(echoed.project_id).toBe(WQ.projectId);
    expect(echoed.project_path).toBe(WQ.projectPath);
  });

  it('leaves projectId unset when nothing resolves, so StoreTool raises its own error', async () => {
    const components = storeComponents(nullDetector());
    const sessionState = { projectId: null } as unknown as SessionState;

    await routeTool(
      'store',
      { type: 'library', forProject: true, content: 'ref' },
      components,
      sessionState
    );

    expect(storeOptionsOf(components).projectId).toBeUndefined();
  });
});
