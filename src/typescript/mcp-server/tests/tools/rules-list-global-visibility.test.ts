/**
 * Regression tests: a project-scoped `rules list` must also surface GLOBAL
 * rules.
 *
 * Field bug (2026-07-23): `rules action:"list"` — the session-start call the
 * server instructions mandate — answered `Found 0 rule(s)` from a repo whose
 * tenant had no rules of its own, while five global rules existed. The Qdrant
 * filter was `scope=="project" AND project_id==X`, so globals were excluded by
 * construction and the agent concluded no behavioral rules were configured.
 *
 * The widening is GATED (`includeGlobal`), because agent-rules' system-prompt
 * injection lists the two scopes separately and would otherwise inject every
 * global rule twice.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { RulesTool } from '../../src/tools/rules.js';
import { buildRuleOptions } from '../../src/tool-builders/rules.js';
import { listRulesMirror } from '../../src/clients/rules-mirror-queries.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

const PROJECT = 'tenant-abc';

/** Every filter handed to Qdrant scroll, in call order. */
let scrollFilters: unknown[] = [];

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    scroll: vi.fn((_collection: string, request: Record<string, unknown>) => {
      scrollFilters.push(request.filter);
      return Promise.resolve({
        points: [
          {
            id: 'rule-project',
            payload: { content: 'Project rule', scope: 'project', project_id: PROJECT },
          },
          { id: 'rule-global', payload: { content: 'Global rule', scope: 'global' } },
        ],
      });
    }),
    count: vi.fn().mockResolvedValue({ count: 2 }),
    search: vi.fn().mockResolvedValue([]),
  })),
}));

/** Collect every `{key, match:{value}}` leaf in a Qdrant filter tree. */
function collectMatches(node: unknown, out: Array<{ key: string; value: unknown }> = []) {
  if (!node || typeof node !== 'object') return out;
  const obj = node as Record<string, unknown>;
  if (typeof obj.key === 'string' && obj.match && typeof obj.match === 'object') {
    out.push({ key: obj.key, value: (obj.match as Record<string, unknown>).value });
  }
  for (const v of Object.values(obj)) {
    if (Array.isArray(v)) v.forEach((item) => collectMatches(item, out));
    else if (v && typeof v === 'object') collectMatches(v, out);
  }
  return out;
}

function createRulesTool(mirrorRows: unknown[] = []): RulesTool {
  const stateManager = {
    listRulesMirror: vi.fn().mockReturnValue(mirrorRows),
    upsertRulesMirror: vi.fn(),
    deleteRulesMirror: vi.fn(),
  } as unknown as SqliteStateManager;
  const projectDetector = {
    getProjectInfo: vi.fn().mockResolvedValue({ projectId: PROJECT }),
  } as unknown as ProjectDetector;
  return new RulesTool(
    { qdrantUrl: 'http://localhost:6333' },
    { isConnected: vi.fn().mockReturnValue(true) } as unknown as DaemonClient,
    stateManager,
    projectDetector
  );
}

describe('rules list — global visibility', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    scrollFilters = [];
  });

  it('includeGlobal widens the project filter to (project AND tenant) OR global', async () => {
    const result = await createRulesTool().execute({
      action: 'list',
      scope: 'project',
      projectId: PROJECT,
      includeGlobal: true,
    });

    expect(result.success).toBe(true);
    const filter = scrollFilters[0] as Record<string, unknown>;
    // An OR (`should`), not the old AND-only tree.
    expect(Array.isArray(filter.should)).toBe(true);
    const matches = collectMatches(filter);
    expect(matches).toContainEqual({ key: 'scope', value: 'global' });
    expect(matches).toContainEqual({ key: 'scope', value: 'project' });
    expect(matches).toContainEqual({ key: 'project_id', value: PROJECT });
    // Both rules come back, each tagged with its owner.
    expect(result.rules?.map((r) => r.owner).sort()).toEqual(['global', PROJECT]);
    expect(result.message).toContain('global');
  });

  it('omits globals by default so agent-rules does not inject them twice', async () => {
    await createRulesTool().execute({ action: 'list', scope: 'project', projectId: PROJECT });

    const filter = scrollFilters[0] as Record<string, unknown>;
    expect(filter.should).toBeUndefined();
    expect(collectMatches(filter)).toEqual([
      { key: 'scope', value: 'project' },
      { key: 'project_id', value: PROJECT },
    ]);
  });

  it('the MCP tool surface turns includeGlobal on by default', () => {
    expect(buildRuleOptions({ action: 'list' }).includeGlobal).toBe(true);
    expect(buildRuleOptions({ action: 'list', includeGlobal: false }).includeGlobal).toBe(false);
    // Non-list actions never carry it.
    expect(
      buildRuleOptions({ action: 'add', label: 'x', content: 'y' }).includeGlobal
    ).toBeUndefined();
  });

  it('the SQLite mirror fallback widens identically', () => {
    const captured: { sql?: string; params?: unknown[] } = {};
    const db = {
      prepare: (sql: string) => ({
        all: (...params: unknown[]) => {
          captured.sql = sql;
          captured.params = params;
          return [];
        },
      }),
    } as unknown as Parameters<typeof listRulesMirror>[0];

    listRulesMirror(db, 'project', PROJECT, 50, true);
    expect(captured.sql).toContain("scope = 'global'");
    expect(captured.params?.[0]).toBe(PROJECT);

    listRulesMirror(db, 'project', PROJECT, 50, false);
    expect(captured.sql).not.toContain("scope = 'global'");
    expect(captured.params?.[0]).toBe(PROJECT);
  });
});
