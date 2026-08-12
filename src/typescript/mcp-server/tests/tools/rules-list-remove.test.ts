/**
 * Tests for RulesTool - list, remove, and unknown actions
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { RulesTool, type RuleOptions } from '../../src/tools/rules.js';
import { buildRuleOptions } from '../../src/tool-builders/rules.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

// Mock the Qdrant client
vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    scroll: vi.fn().mockResolvedValue({
      points: [
        {
          id: 'rule-1',
          payload: {
            content: 'Always use TypeScript',
            scope: 'global',
            title: 'TypeScript Rule',
            priority: '10',
            // Present in the payload so the summary-omission assertions bite:
            // without these, `createdAt`/`updatedAt` are undefined in BOTH the
            // shaped and unshaped forms and the test proves nothing.
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-02T00:00:00Z',
          },
        },
        {
          id: 'rule-2',
          payload: {
            content: 'Follow TDD',
            scope: 'project',
            project_id: 'test-project',
            tags: 'testing,quality',
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-02T00:00:00Z',
          },
        },
      ],
    }),
    search: vi.fn().mockResolvedValue([]),
  })),
}));

function createMockDaemonClient(): DaemonClient {
  return {
    isConnected: vi.fn().mockReturnValue(true),
    ingestText: vi.fn().mockResolvedValue({
      success: true,
      document_id: 'new-rule-id',
      chunks_created: 1,
    }),
    embedText: vi.fn(),
    generateSparseVector: vi.fn(),
    connect: vi.fn(),
    close: vi.fn(),
    getConnectionState: vi.fn(),
    healthCheck: vi.fn(),
    getStatus: vi.fn(),
    getMetrics: vi.fn(),
    notifyServerStatus: vi.fn(),
    registerProject: vi.fn(),
    deprioritizeProject: vi.fn(),
    heartbeat: vi.fn(),
  } as unknown as DaemonClient;
}

function createMockStateManager(): SqliteStateManager {
  return {
    initialize: vi.fn().mockReturnValue({ status: 'ok' }),
    close: vi.fn(),
    enqueueUnified: vi.fn().mockReturnValue({
      status: 'ok',
      data: {
        queueId: 'queued-rule-id',
        isNew: true,
        idempotencyKey: 'test-key',
      },
    }),
    upsertRulesMirror: vi.fn(),
    deleteRulesMirror: vi.fn(),
    listRulesMirror: vi.fn().mockReturnValue([]),
  } as unknown as SqliteStateManager;
}

function createMockProjectDetector(): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/test/project'),
    getProjectInfo: vi.fn().mockResolvedValue({
      projectId: 'test-project-123',
      projectPath: '/test/project',
      name: 'test-project',
    }),
  } as unknown as ProjectDetector;
}

describe('RulesTool', () => {
  let rulesTool: RulesTool;
  let mockDaemonClient: DaemonClient;
  let mockStateManager: SqliteStateManager;
  let mockProjectDetector: ProjectDetector;

  beforeEach(() => {
    vi.clearAllMocks();
    mockDaemonClient = createMockDaemonClient();
    mockStateManager = createMockStateManager();
    mockProjectDetector = createMockProjectDetector();

    rulesTool = new RulesTool(
      { qdrantUrl: 'http://localhost:6333' },
      mockDaemonClient,
      mockStateManager,
      mockProjectDetector
    );
  });

  describe('remove action', () => {
    it('should queue removal scoped to the active project (F-015)', async () => {
      const options: RuleOptions = {
        action: 'remove',
        label: 'rule-to-remove',
      };

      const result = await rulesTool.execute(options);

      expect(result.success).toBe(true);
      expect(result.action).toBe('remove');
      expect(result.fallback_mode).toBe('unified_queue');
      // F-015: default scope is 'project'; the detector resolves to
      // 'test-project-123' so the queue payload MUST target that
      // tenant (not 'global'). Otherwise project A's remove could
      // evict a same-labeled rule in project B.
      expect(mockStateManager.enqueueUnified).toHaveBeenCalledWith(
        'text',
        'delete',
        'test-project-123',
        'rules',
        expect.objectContaining({
          label: 'rule-to-remove',
          action: 'remove',
          scope: 'project',
          project_id: 'test-project-123',
        }),
        1, // PRIORITY_HIGH
        'main',
        expect.any(Object)
      );
    });

    it('should remove globally when scope is explicitly global', async () => {
      const options: RuleOptions = {
        action: 'remove',
        label: 'rule-to-remove',
        scope: 'global',
      };

      const result = await rulesTool.execute(options);

      expect(result.success).toBe(true);
      expect(result.action).toBe('remove');
      expect(mockStateManager.enqueueUnified).toHaveBeenCalledWith(
        'text',
        'delete',
        'global',
        'rules',
        expect.objectContaining({
          label: 'rule-to-remove',
          action: 'remove',
          scope: 'global',
        }),
        1,
        'main',
        expect.any(Object)
      );
    });

    it('should reject missing label', async () => {
      const options: RuleOptions = {
        action: 'remove',
      };

      const result = await rulesTool.execute(options);

      expect(result.success).toBe(false);
      expect(result.message).toContain('Label is required');
    });
  });

  describe('list action', () => {
    it('should list global rules', async () => {
      const options: RuleOptions = {
        action: 'list',
        scope: 'global',
      };

      const result = await rulesTool.execute(options);

      expect(result.success).toBe(true);
      expect(result.action).toBe('list');
      expect(result.rules).toBeDefined();
      expect(result.rules!.length).toBe(2);
    });

    it('should list project-scoped rules', async () => {
      const options: RuleOptions = {
        action: 'list',
        scope: 'project',
      };

      const result = await rulesTool.execute(options);

      expect(result.success).toBe(true);
      expect(mockProjectDetector.getProjectInfo).toHaveBeenCalled();
    });

    it('should parse tags from comma-separated string', async () => {
      const options: RuleOptions = {
        action: 'list',
        scope: 'global',
      };

      const result = await rulesTool.execute(options);

      const ruleWithTags = result.rules?.find((r) => r.id === 'rule-2');
      expect(ruleWithTags?.tags).toEqual(['testing', 'quality']);
    });

    it('should parse priority from string', async () => {
      const options: RuleOptions = {
        action: 'list',
        scope: 'global',
      };

      const result = await rulesTool.execute(options);

      const ruleWithPriority = result.rules?.find((r) => r.id === 'rule-1');
      expect(ruleWithPriority?.priority).toBe(10);
    });

    it('should handle Qdrant errors gracefully', async () => {
      const QdrantClientMock = await import('@qdrant/js-client-rest');
      vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
        () =>
          ({
            scroll: vi.fn().mockRejectedValue(new Error('Collection not found')),
          }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
      );

      const newTool = new RulesTool(
        { qdrantUrl: 'http://localhost:6333' },
        mockDaemonClient,
        mockStateManager,
        mockProjectDetector
      );

      const result = await newTool.execute({ action: 'list', scope: 'global' });

      expect(result.success).toBe(false);
      expect(result.message).toContain('Failed to list rules');
    });
  });

  describe('list shaping (summary / budget / cursor)', () => {
    it('summary mode returns preview + content_length and omits the full body', async () => {
      const result = await rulesTool.execute({
        action: 'list',
        scope: 'global',
        summary: true,
      });

      expect(result.success).toBe(true);
      const r1 = result.rules?.find((r) => r.id === 'rule-1');
      expect(r1?.content).toBeUndefined();
      expect(r1?.preview).toBe('Always use TypeScript');
      expect(r1?.content_length).toBe('Always use TypeScript'.length);
      // Metadata survives the projection so the summary is actionable.
      expect(r1?.title).toBe('TypeScript Rule');
      expect(r1?.priority).toBe(10);
      expect(result.hint).toContain('summary:false');
    });

    it('default (no options) preserves full content for internal callers', async () => {
      const result = await rulesTool.execute({ action: 'list', scope: 'global' });

      const r1 = result.rules?.find((r) => r.id === 'rule-1');
      expect(r1?.content).toBe('Always use TypeScript');
      expect(r1?.preview).toBeUndefined();
      expect(result.budget_truncated).toBeUndefined();
      expect(result.next_cursor).toBeUndefined();
    });

    it('byte budget drops trailing rules and sets a lossless resume cursor', async () => {
      const result = await rulesTool.execute({
        action: 'list',
        scope: 'global',
        maxResponseBytes: 10, // smaller than one rule → keep 1, drop the rest
      });

      expect(result.rules?.length).toBe(1);
      expect(result.count).toBe(1);
      expect(result.budget_truncated?.dropped).toBe(1);
      // Cursor resumes at the first dropped rule (inclusive Qdrant scroll id).
      expect(result.next_cursor).toBe('rule-2');
    });

    it('summary omits timestamps but keeps the owner discriminator', async () => {
      const result = await rulesTool.execute({
        action: 'list',
        scope: 'project',
        projectId: 'test-project',
        includeGlobal: true,
        summary: true,
      });

      // Timestamps: in the payload (see the fixtures), surfaced by the FULL
      // form, and dropped by the summary — ~100 chars per entry as two ISO
      // strings that nothing ranks or filters on. Asserted on BOTH rules,
      // unconditionally, so this fails against the pre-change shaping.
      const r1 = result.rules?.find((r) => r.id === 'rule-1');
      const r2 = result.rules?.find((r) => r.id === 'rule-2');
      expect(r1).toBeDefined();
      expect(r2).toBeDefined();
      expect(r1?.createdAt).toBeUndefined();
      expect(r1?.updatedAt).toBeUndefined();
      expect(r2?.createdAt).toBeUndefined();
      expect(r2?.updatedAt).toBeUndefined();

      // `owner` is the documented always-set discriminator; it SURVIVES the
      // summary even where it duplicates projectId (the project-rule case).
      expect(r1?.owner).toBe('global');
      expect(r2?.owner).toBe('test-project');
      expect(r2?.projectId).toBe('test-project');

      // The full form still carries the timestamps.
      const full = await rulesTool.execute({
        action: 'list',
        scope: 'project',
        projectId: 'test-project',
        includeGlobal: true,
        summary: false,
      });
      const f2 = full.rules?.find((r) => r.id === 'rule-2');
      expect(f2?.createdAt).toBe('2026-01-01T00:00:00Z');
      expect(f2?.updatedAt).toBe('2026-01-02T00:00:00Z');
    });

    it('pins the shared defaults: fetch cap 200 in the scroll, budget 40000 in the builder', async () => {
      // Regression coverage for the shared pattern (repo rule): a revert of
      // either default passes the whole suite otherwise. The fetch cap is
      // asserted where it takes effect — the scroll request — because that is
      // the layer whose silent 50 made 11 of 61 rules invisible; the budget is
      // asserted at the builder, the only place that applies it.
      await rulesTool.execute({ action: 'list', scope: 'global' });
      const { QdrantClient } = await import('@qdrant/js-client-rest');
      const instance = vi.mocked(QdrantClient).mock.results.at(-1)?.value as {
        scroll: ReturnType<typeof vi.fn>;
      };
      const lastCall = instance.scroll.mock.calls.at(-1) as
        | [string, { limit?: number }]
        | undefined;
      expect(lastCall?.[1]?.limit).toBe(200);

      const opts = buildRuleOptions({ action: 'list' });
      expect(opts.maxResponseBytes).toBe(40000);
      expect(opts.summary).toBe(true);
      // No builder-level limit: the core default covers EVERY caller (MCP,
      // prompt injection, seeder, admin REST) — the split-default design that
      // kept internal callers at 50 was itself the review finding.
      expect(opts.limit).toBeUndefined();
    });
  });

  describe('unknown action', () => {
    it('should return error for unknown action', async () => {
      const options = {
        action: 'unknown' as unknown as 'add',
      };

      const result = await rulesTool.execute(options);

      expect(result.success).toBe(false);
      expect(result.message).toContain('Unknown action');
    });
  });
});
