/**
 * Tests for RetrieveTool - retrieve by document ID and metadata extraction
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { RetrieveTool, type RetrieveOptions } from '../../src/tools/retrieve.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

// Mock the Qdrant client
vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    retrieve: vi.fn().mockResolvedValue([
      {
        id: 'doc-123',
        payload: {
          content: 'Document content here',
          title: 'Test Document',
          source_type: 'user_input',
          tenant_id: 'test-project-123',
        },
      },
    ]),
    scroll: vi.fn().mockResolvedValue({
      points: [
        {
          id: 'doc-1',
          payload: {
            content: 'First document',
            title: 'Doc 1',
            source_type: 'file',
            tenant_id: 'test-project-123',
          },
        },
        {
          id: 'doc-2',
          payload: {
            content: 'Second document',
            title: 'Doc 2',
            source_type: 'web',
            tenant_id: 'test-project-123',
          },
        },
      ],
    }),
  })),
}));

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

describe('RetrieveTool - retrieve by document ID', () => {
  let retrieveTool: RetrieveTool;
  let mockProjectDetector: ProjectDetector;

  beforeEach(async () => {
    vi.clearAllMocks();
    mockProjectDetector = createMockProjectDetector();

    retrieveTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);
  });

  it('should retrieve a document by ID', async () => {
    const options: RetrieveOptions = {
      documentId: 'doc-123',
      collection: 'projects',
    };

    const result = await retrieveTool.retrieve(options);

    expect(result.success).toBe(true);
    expect(result.documents).toHaveLength(1);
    expect(result.documents[0].id).toBe('doc-123');
    expect(result.documents[0].content).toBe('Document content here');
    expect(result.documents[0].metadata.title).toBe('Test Document');
    expect(result.total).toBe(1);
    expect(result.hasMore).toBe(false);
  });

  it('should exclude content from metadata', async () => {
    const options: RetrieveOptions = {
      documentId: 'doc-123',
      collection: 'projects',
    };

    const result = await retrieveTool.retrieve(options);

    expect(result.documents[0].content).toBe('Document content here');
    expect(result.documents[0].metadata.content).toBeUndefined();
  });

  it('should return not found for missing document', async () => {
    const QdrantClientMock = await import('@qdrant/js-client-rest');
    vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
      () =>
        ({
          retrieve: vi.fn().mockResolvedValue([]),
          scroll: vi.fn().mockResolvedValue({ points: [] }),
        }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
    );

    const newTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);

    const result = await newTool.retrieve({
      documentId: 'nonexistent',
      collection: 'projects',
    });

    expect(result.success).toBe(false);
    expect(result.documents).toHaveLength(0);
    expect(result.message).toContain('Document not found');
    expect(result.hint).toContain('result `id` field');
    expect(result.hint).toContain('filter');
  });

  it('should fall back to metadata.document_id when the point id is missing', async () => {
    const QdrantClientMock = await import('@qdrant/js-client-rest');
    const scrollFn = vi.fn().mockResolvedValue({
      points: [
        {
          id: 'chunk-1',
          payload: {
            content: 'Chunk content here',
            title: 'Fallback Chunk',
            source_type: 'file',
            tenant_id: 'test-project-123',
            document_id: 'doc-abc',
          },
        },
      ],
    });
    vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
      () =>
        ({
          retrieve: vi.fn().mockResolvedValue([]),
          scroll: scrollFn,
        }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
    );

    const newTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);

    const result = await newTool.retrieve({
      documentId: 'doc-abc',
      collection: 'projects',
      projectId: 'test-project-123',
    });

    expect(result.success).toBe(true);
    expect(result.documents).toHaveLength(1);
    expect(result.documents[0].id).toBe('chunk-1');
    expect(result.documents[0].content).toBe('Chunk content here');
    expect(result.documents[0].metadata.document_id).toBe('doc-abc');
    expect(scrollFn).toHaveBeenCalledTimes(1);
    const [, request] = scrollFn.mock.calls[0];
    const filter = (request as Record<string, unknown>).filter as Record<string, unknown>;
    expect(filter).toBeDefined();
    expect(filter.must).toEqual(
      expect.arrayContaining([
        { key: 'tenant_id', match: { value: 'test-project-123' } },
        { key: 'document_id', match: { value: 'doc-abc' } },
      ])
    );
  });

  it('should suggest filter.document_id when the requested id looks like a content hash', async () => {
    const QdrantClientMock = await import('@qdrant/js-client-rest');
    vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
      () =>
        ({
          retrieve: vi.fn().mockResolvedValue([]),
          scroll: vi.fn().mockResolvedValue({ points: [] }),
        }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
    );

    const newTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);

    const result = await newTool.retrieve({
      documentId: '2aa6f841182b38bdf5ed3beb4c00453a498f3a356d7eb8ee07bcd7bfddbda423',
      collection: 'projects',
    });

    expect(result.success).toBe(false);
    expect(result.message).toContain('Document not found');
    expect(result.hint).toContain('document_id');
    expect(result.hint).toContain('filter');
  });

  it('should handle retrieve errors gracefully', async () => {
    const QdrantClientMock = await import('@qdrant/js-client-rest');
    vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
      () =>
        ({
          retrieve: vi.fn().mockRejectedValue(new Error('Connection failed')),
        }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
    );

    const newTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);

    const result = await newTool.retrieve({
      documentId: 'doc-123',
      collection: 'projects',
    });

    expect(result.success).toBe(false);
    expect(result.message).toContain('Failed to retrieve document');
    expect(result.hint).toContain('Qdrant');
  });
});

describe('RetrieveTool - metadata extraction', () => {
  let mockProjectDetector: ProjectDetector;

  beforeEach(async () => {
    vi.clearAllMocks();
    mockProjectDetector = createMockProjectDetector();
  });

  it('should exclude vector fields from metadata', async () => {
    const QdrantClientMock = await import('@qdrant/js-client-rest');
    vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
      () =>
        ({
          retrieve: vi.fn().mockResolvedValue([
            {
              id: 'doc-123',
              payload: {
                content: 'Document content',
                dense_vector: [0.1, 0.2, 0.3],
                sparse_vector: { indices: [1, 2], values: [0.5, 0.5] },
                title: 'Test',
                // F-002: tenant_id is required for the ownership check
                // performed by `retrieveById` against the projects
                // collection. Must match the mock detector's resolved
                // project id.
                tenant_id: 'test-project-123',
              },
            },
          ]),
        }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
    );

    const newTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);

    const result = await newTool.retrieve({ documentId: 'doc-123' });

    expect(result.documents[0].metadata.title).toBe('Test');
    expect(result.documents[0].metadata.dense_vector).toBeUndefined();
    expect(result.documents[0].metadata.sparse_vector).toBeUndefined();
    expect(result.documents[0].metadata.content).toBeUndefined();
  });

  it('should reject null payload as not-found (F-002 ownership check)', async () => {
    // Pre-F-002 this returned success with empty content. Post-fix the
    // ownership check cannot pass without a payload, so the response
    // collapses to the same not-found shape as a foreign tenant ID.
    const QdrantClientMock = await import('@qdrant/js-client-rest');
    vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
      () =>
        ({
          retrieve: vi.fn().mockResolvedValue([
            {
              id: 'doc-123',
              payload: null,
            },
          ]),
        }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
    );

    const newTool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockProjectDetector);

    const result = await newTool.retrieve({ documentId: 'doc-123' });

    expect(result.success).toBe(false);
    expect(result.documents).toHaveLength(0);
    expect(result.message).toContain('Document not found');
  });
});
