import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import Database, { type Database as DatabaseType } from 'better-sqlite3';

import { listChunkCandidates } from '../../src/clients/tracked-files-queries/index.js';

const WATCH_ID = 'watch-001';
const NOW = '2026-06-13T12:00:00Z';

const SCHEMA = `
CREATE TABLE tracked_files (
  file_id INTEGER PRIMARY KEY AUTOINCREMENT,
  watch_folder_id TEXT NOT NULL,
  file_path TEXT NOT NULL,
  relative_path TEXT,
  branches TEXT NOT NULL DEFAULT '[]',
  file_type TEXT,
  file_mtime TEXT NOT NULL,
  file_hash TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE qdrant_chunks (
  chunk_id INTEGER PRIMARY KEY AUTOINCREMENT,
  file_id INTEGER NOT NULL,
  point_id TEXT NOT NULL,
  chunk_index INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  chunk_type TEXT,
  symbol_name TEXT,
  start_line INTEGER,
  end_line INTEGER,
  created_at TEXT NOT NULL
);
`;

function seedFile(db: DatabaseType, relativePath: string, fileType = 'code', branch = 'main'): number {
  const info = db.prepare(`
    INSERT INTO tracked_files
      (watch_folder_id, file_path, relative_path, branches, file_type, file_mtime, file_hash, created_at, updated_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
  `).run(
    WATCH_ID,
    `/repo/${relativePath}`,
    relativePath,
    JSON.stringify([branch]),
    fileType,
    NOW,
    `hash-${relativePath}-${branch}`,
    NOW,
    NOW
  );
  return Number(info.lastInsertRowid);
}

function seedChunk(
  db: DatabaseType,
  fileId: number,
  pointId: string,
  symbolName: string | null,
  startLine: number
): void {
  db.prepare(`
    INSERT INTO qdrant_chunks
      (file_id, point_id, chunk_index, content_hash, chunk_type, symbol_name, start_line, end_line, created_at)
    VALUES (?, ?, 0, ?, 'function', ?, ?, ?, ?)
  `).run(fileId, pointId, `content-${pointId}`, symbolName, startLine, startLine + 10, NOW);
}

describe('listChunkCandidates', () => {
  let db: DatabaseType;

  beforeEach(() => {
    db = new Database(':memory:');
    db.exec(SCHEMA);
  });

  afterEach(() => {
    db.close();
  });

  it('finds candidates by exact chunk symbol before path matches', () => {
    const symbolFileId = seedFile(db, 'src/search-qdrant.ts');
    const pathFileId = seedFile(db, 'docs/applyRRFFusion-notes.md', 'docs');
    seedChunk(db, pathFileId, 'path-point', null, 1);
    seedChunk(db, symbolFileId, 'symbol-point', 'applyRRFFusion', 165);

    const result = listChunkCandidates(db, {
      watchFolderId: WATCH_ID,
      needles: ['applyRRFFusion'],
      limit: 5,
    });

    expect(result.status).toBe('ok');
    expect(result.data.map((entry) => entry.pointId)).toEqual(['symbol-point', 'path-point']);
    expect(result.data[0]?.symbolName).toBe('applyRRFFusion');
  });

  it('filters by file type when provided', () => {
    const codeFileId = seedFile(db, 'src/search-qdrant.ts', 'code');
    const docFileId = seedFile(db, 'docs/search-qdrant.md', 'docs');
    seedChunk(db, codeFileId, 'code-point', null, 10);
    seedChunk(db, docFileId, 'doc-point', null, 20);

    const result = listChunkCandidates(db, {
      watchFolderId: WATCH_ID,
      needles: ['search-qdrant'],
      fileType: 'code',
      limit: 5,
    });

    expect(result.status).toBe('ok');
    expect(result.data.map((entry) => entry.pointId)).toEqual(['code-point']);
  });

  it('restricts candidates to the requested branch, and spans branches when omitted', () => {
    const mainFileId = seedFile(db, 'src/widget.ts', 'code', 'main');
    const featFileId = seedFile(db, 'src/widget.ts', 'code', 'feature/x');
    seedChunk(db, mainFileId, 'main-point', 'WidgetFactory', 10);
    seedChunk(db, featFileId, 'feat-point', 'WidgetFactory', 10);

    // A concrete branch must not surface chunks indexed under another branch.
    const onlyMain = listChunkCandidates(db, {
      watchFolderId: WATCH_ID,
      needles: ['WidgetFactory'],
      branch: 'main',
      limit: 5,
    });
    expect(onlyMain.status).toBe('ok');
    expect(onlyMain.data.map((entry) => entry.pointId)).toEqual(['main-point']);

    // Omitting branch spans every indexed branch (explicit cross-branch, e.g. `*`).
    const spanning = listChunkCandidates(db, {
      watchFolderId: WATCH_ID,
      needles: ['WidgetFactory'],
      limit: 5,
    });
    expect(spanning.data.map((entry) => entry.pointId).sort()).toEqual(['feat-point', 'main-point']);
  });
});
