/**
 * Tests for tracked-files-queries: listTrackedFiles
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import Database, { type Database as DatabaseType } from 'better-sqlite3';

import {
  countTrackedFiles,
  listTrackedFiles,
} from '../../src/clients/tracked-files-queries/index.js';

const TRACKED_FILES_SCHEMA = `
CREATE TABLE IF NOT EXISTS watch_folders (
    watch_id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    collection TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    parent_watch_id TEXT,
    submodule_path TEXT,
    git_remote_url TEXT,
    remote_hash TEXT,
    disambiguation_path TEXT,
    is_active INTEGER DEFAULT 0,
    last_activity_at TEXT,
    library_mode TEXT,
    follow_symlinks INTEGER DEFAULT 0,
    enabled INTEGER DEFAULT 1,
    cleanup_on_disable INTEGER DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_scan TEXT,
    FOREIGN KEY (parent_watch_id) REFERENCES watch_folders(watch_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tracked_files (
    file_id INTEGER PRIMARY KEY AUTOINCREMENT,
    watch_folder_id TEXT NOT NULL,
    file_path TEXT NOT NULL,
    branches TEXT NOT NULL DEFAULT '[]',
    file_type TEXT,
    language TEXT,
    file_mtime TEXT NOT NULL,
    file_hash TEXT NOT NULL,
    chunk_count INTEGER DEFAULT 0,
    chunking_method TEXT,
    lsp_status TEXT DEFAULT 'none',
    treesitter_status TEXT DEFAULT 'none',
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    collection TEXT NOT NULL DEFAULT 'projects',
    extension TEXT,
    is_test INTEGER DEFAULT 0,
    base_point TEXT,
    relative_path TEXT,
    FOREIGN KEY (watch_folder_id) REFERENCES watch_folders(watch_id),
    UNIQUE(watch_folder_id, relative_path, file_hash)
);
`;

const WATCH_ID = 'watch-001';
const NOW = '2026-02-24T12:00:00Z';

function seedProject(db: DatabaseType): void {
  db.prepare(
    `INSERT INTO watch_folders (watch_id, path, collection, tenant_id, is_active, created_at, updated_at)
     VALUES (?, ?, 'projects', 'tenant-001', 1, ?, ?)`
  ).run(WATCH_ID, '/home/user/project', NOW, NOW);
}

function attachSearchMetadata(db: DatabaseType): void {
  db.exec(`
    ATTACH DATABASE ':memory:' AS searchdb;
    CREATE TABLE searchdb.file_metadata (
      file_id INTEGER PRIMARY KEY,
      tenant_id TEXT NOT NULL,
      branches TEXT NOT NULL DEFAULT '[]',
      file_path TEXT NOT NULL,
      size_bytes INTEGER,
      fts5_skipped INTEGER NOT NULL DEFAULT 0,
      reindex_count INTEGER NOT NULL DEFAULT 0,
      first_indexed_at TEXT,
      base_point TEXT,
      relative_path TEXT,
      file_hash TEXT
    );
  `);
}

function seedSearchMetadata(db: DatabaseType, relativePath: string, branch = 'main'): void {
  db.prepare(
    `INSERT INTO searchdb.file_metadata
     (tenant_id, branches, file_path, relative_path, size_bytes, fts5_skipped)
     VALUES ('tenant-001', ?, ?, ?, 100, 0)`
  ).run(JSON.stringify([branch]), `/home/user/project/${relativePath}`, relativePath);
}
function seedFile(
  db: DatabaseType,
  relativePath: string,
  opts: {
    fileType?: string;
    language?: string;
    extension?: string;
    isTest?: boolean;
    branch?: string;
  } = {}
): void {
  const ext = opts.extension ?? relativePath.split('.').pop() ?? null;
  db.prepare(
    `INSERT INTO tracked_files
     (watch_folder_id, file_path, relative_path, file_type, language, extension, is_test, branches, file_mtime, file_hash, created_at, updated_at)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
  ).run(
    WATCH_ID,
    `/home/user/project/${relativePath}`,
    relativePath,
    opts.fileType ?? 'code',
    opts.language ?? null,
    ext,
    opts.isTest ? 1 : 0,
    JSON.stringify([opts.branch ?? 'main']),
    NOW,
    'hash-' + relativePath + '-' + (opts.branch ?? 'main'),
    NOW,
    NOW
  );
}

describe('listTrackedFiles', () => {
  let db: DatabaseType;

  beforeEach(() => {
    db = new Database(':memory:');
    db.exec(TRACKED_FILES_SCHEMA);
    seedProject(db);

    // Seed a realistic file set
    seedFile(db, 'src/main.rs', { language: 'rust', fileType: 'code' });
    seedFile(db, 'src/lib.rs', { language: 'rust', fileType: 'code' });
    seedFile(db, 'src/utils/helpers.rs', { language: 'rust', fileType: 'code' });
    seedFile(db, 'src/server.ts', { language: 'typescript', fileType: 'code' });
    seedFile(db, 'tests/test_main.rs', { language: 'rust', fileType: 'code', isTest: true });
    seedFile(db, 'README.md', { fileType: 'text', extension: 'md' });
    seedFile(db, 'Cargo.toml', { fileType: 'build', extension: 'toml' });
    seedFile(db, 'config.yaml', { fileType: 'config', extension: 'yaml' });
  });

  afterEach(() => {
    db.close();
  });

  it('should list all files for a watch folder', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(8);
    expect(result.data[0].relativePath).toBe('Cargo.toml'); // sorted ASC
  });

  it('should filter by path prefix', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, path: 'src' });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(4);
    for (const f of result.data) {
      expect(f.relativePath).toMatch(/^src\//);
    }
  });

  it('should filter by nested path prefix', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, path: 'src/utils' });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(1);
    expect(result.data[0].relativePath).toBe('src/utils/helpers.rs');
  });

  it('should filter by fileType', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, fileType: 'code' });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(5);
  });
  it('should include search metadata fallback rows when filtering by fileType and language', () => {
    attachSearchMetadata(db);
    seedSearchMetadata(db, 'src/orphan.ts', 'dev-clean');
    seedSearchMetadata(db, 'src/main-only.ts', 'main');
    seedSearchMetadata(db, 'src/orphan.scss');
    seedSearchMetadata(db, 'tests/orphan_test.ts');

    const code = listTrackedFiles(db, {
      watchFolderId: WATCH_ID,
      path: 'src',
      fileType: 'code',
      branch: 'dev-clean',
    });
    expect(code.status).toBe('ok');
    expect(code.data.map((file) => file.relativePath)).toContain('src/orphan.ts');
    expect(code.data.find((file) => file.relativePath === 'src/orphan.ts')).toMatchObject({
      fileType: 'code',
      language: 'typescript',
      extension: 'ts',
    });
    expect(code.data.map((file) => file.relativePath)).not.toContain('src/orphan.scss');
    expect(code.data.map((file) => file.relativePath)).not.toContain('src/main-only.ts');

    const typescript = listTrackedFiles(db, {
      watchFolderId: WATCH_ID,
      path: 'src',
      language: 'typescript',
      branch: 'dev-clean',
    });
    expect(typescript.data.map((file) => file.relativePath)).toContain('src/orphan.ts');

    const noTests = listTrackedFiles(db, { watchFolderId: WATCH_ID, includeTests: false });
    expect(noTests.data.map((file) => file.relativePath)).not.toContain('tests/orphan_test.ts');
    expect(
      countTrackedFiles(db, {
        watchFolderId: WATCH_ID,
        path: 'src',
        fileType: 'code',
        branch: 'dev-clean',
      })
    ).toBe(code.data.length);
  });

  it('should infer missing tracked file metadata when filtering by fileType and language', () => {
    seedFile(db, 'src/metadata-missing.ts', {
      fileType: '',
      language: '',
      extension: 'ts',
      branch: 'dev-clean',
    });
    seedFile(db, 'src/style-missing.scss', {
      fileType: '',
      language: '',
      extension: 'scss',
      branch: 'dev-clean',
    });

    const code = listTrackedFiles(db, {
      watchFolderId: WATCH_ID,
      path: 'src',
      fileType: 'code',
      branch: 'dev-clean',
    });
    expect(code.data.map((file) => file.relativePath)).toContain('src/metadata-missing.ts');
    expect(code.data.map((file) => file.relativePath)).not.toContain('src/style-missing.scss');
    expect(code.data.find((file) => file.relativePath === 'src/metadata-missing.ts')).toMatchObject(
      {
        fileType: 'code',
        language: 'typescript',
        extension: 'ts',
      }
    );

    const typescript = listTrackedFiles(db, {
      watchFolderId: WATCH_ID,
      path: 'src',
      language: 'typescript',
      branch: 'dev-clean',
    });
    expect(typescript.data.map((file) => file.relativePath)).toContain('src/metadata-missing.ts');
  });
  it('should filter by language', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, language: 'rust' });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(4); // main.rs, lib.rs, helpers.rs, test_main.rs
  });

  it('should filter by extension', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, extension: 'rs' });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(4); // main.rs, lib.rs, helpers.rs, test_main.rs
  });

  it('should exclude test files when includeTests is false', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, includeTests: false });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(7);
    for (const f of result.data) {
      expect(f.isTest).toBe(false);
    }
  });

  it('should respect limit', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, limit: 3 });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(3);
  });

  it('should combine filters', () => {
    const result = listTrackedFiles(db, {
      watchFolderId: WATCH_ID,
      path: 'src',
      language: 'rust',
    });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(3); // main.rs, lib.rs, helpers.rs
  });

  it('should return empty for non-existent path', () => {
    const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, path: 'nonexistent' });
    expect(result.status).toBe('ok');
    expect(result.data).toHaveLength(0);
  });

  it('should return degraded when db is null', () => {
    const result = listTrackedFiles(null, { watchFolderId: WATCH_ID });
    expect(result.status).toBe('degraded');
    expect(result.reason).toBe('database_not_found');
    expect(result.data).toEqual([]);
  });

  it('should map fields correctly', () => {
    const result = listTrackedFiles(db, {
      watchFolderId: WATCH_ID,
      path: 'src',
      language: 'typescript',
    });
    expect(result.data).toHaveLength(1);
    const file = result.data[0];
    expect(file.relativePath).toBe('src/server.ts');
    expect(file.fileType).toBe('code');
    expect(file.language).toBe('typescript');
    expect(file.extension).toBe('ts');
    expect(file.isTest).toBe(false);
  });

  // ── Glob / pattern floating (regression) ──────────────────────────────────
  // A relative pattern must "float" (match at any depth), not be anchored to the
  // repo root. Mirrors the daemon grep `normalize_path_glob` fix; before it a
  // bare pattern like "V*.sql" against a nested file returned nothing, silently.
  describe('glob pattern floating', () => {
    it('matches a bare filename glob at any nested depth', () => {
      // "helpers.rs" lives at src/utils/helpers.rs — a root-anchored GLOB missed it.
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'helpers.rs' });
      expect(result.status).toBe('ok');
      expect(result.data.map((f) => f.relativePath)).toEqual(['src/utils/helpers.rs']);
    });

    it('matches a wildcard filename glob at any nested depth (V*.sql migrations)', () => {
      seedFile(db, 'db/migration/V56__cohort.sql', { language: 'sql', extension: 'sql' });
      seedFile(db, 'db/migration/V57__shift.sql', { language: 'sql', extension: 'sql' });
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'V*.sql' });
      expect(result.status).toBe('ok');
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'db/migration/V56__cohort.sql',
        'db/migration/V57__shift.sql',
      ]);
    });

    it('still matches a relative filename glob at the repo root', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'Cargo.toml' });
      expect(result.data.map((f) => f.relativePath)).toEqual(['Cargo.toml']);
    });

    it('floats a nested relative path glob to any depth', () => {
      seedFile(db, 'backend/db/migration/V1__init.sql', { language: 'sql', extension: 'sql' });
      const result = listTrackedFiles(db, {
        watchFolderId: WATCH_ID,
        glob: 'db/migration/V*.sql',
      });
      expect(result.data.map((f) => f.relativePath)).toContain(
        'backend/db/migration/V1__init.sql'
      );
    });

    it('leaves already-floating (leading *) globs matching at any depth', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '*.rs' });
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'src/lib.rs',
        'src/main.rs',
        'src/utils/helpers.rs',
        'tests/test_main.rs',
      ]);
    });

    it('honors ** in a floating pattern (collapsed to * for SQLite)', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.rs' });
      expect(result.data.map((f) => f.relativePath)).toContain('src/utils/helpers.rs');
    });

    // `**/` means any depth INCLUDING ZERO. Collapsing it to `*/` forced a
    // literal slash, so a root-level file was skipped while grep and semantic
    // search — where `**/` is optional — matched it. Same glob, three surfaces,
    // different sets.
    it('matches a root-level file through a **/ prefix', () => {
      seedFile(db, 'root.rs', { language: 'rust', fileType: 'code' });
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.rs' });
      const paths = result.data.map((f) => f.relativePath);
      expect(paths).toContain('root.rs'); // zero directories
      expect(paths).toContain('src/utils/helpers.rs'); // and still any depth
    });

    it('matches a directory-level file through a mid-pattern **/', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src/**/*.rs' });
      const paths = result.data.map((f) => f.relativePath).sort();
      expect(paths).toEqual(['src/lib.rs', 'src/main.rs', 'src/utils/helpers.rs']);
    });

    it('keeps a mid-pattern **/ from gaining subtree semantics', () => {
      // `a/**/b` expands to `a/b` (zero dirs), which is wildcard-free — it must
      // NOT be treated as a directory literal and pull in `a/b/anything`.
      seedFile(db, 'pkg/mod.rs', { language: 'rust', fileType: 'code' });
      seedFile(db, 'pkg/mod.rs.bak/inner.rs', { language: 'rust', fileType: 'code' });
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'pkg/**/mod.rs' });
      expect(result.data.map((f) => f.relativePath)).toEqual(['pkg/mod.rs']);
    });

    it('countTrackedFiles agrees for a **/ pattern reaching the root', () => {
      seedFile(db, 'root.rs', { language: 'rust', fileType: 'code' });
      const list = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.rs' });
      const count = countTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.rs' });
      expect(count).toBe(list.data.length);
      expect(count).toBe(5);
    });

    it('countTrackedFiles agrees with listTrackedFiles for a floated glob', () => {
      seedFile(db, 'db/migration/V9__x.sql', { language: 'sql', extension: 'sql' });
      const list = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'V*.sql' });
      const count = countTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'V*.sql' });
      expect(count).toBe(list.data.length);
      expect(count).toBe(1);
    });
  });

  // ── Directory-shaped glob / excludeGlob scoping (regression) ──────────────
  // A wildcard-free literal is a PATH the caller scopes to. Before the fix it
  // floated only as a same-named FILE (`*/dir`), so a bare directory name
  // selected/excluded nothing — the files live UNDER it. It must now match the
  // whole subtree, mirroring the daemon `normalize_path_glob` / TS
  // `matchesFloatingGlob`. A trailing slash is directory-only.
  describe('directory-shaped glob / excludeGlob scoping', () => {
    it('a bare directory name (include) matches its whole subtree', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src' });
      expect(result.status).toBe('ok');
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'src/lib.rs',
        'src/main.rs',
        'src/server.ts',
        'src/utils/helpers.rs',
      ]);
    });

    it('a nested bare directory name floats to any depth', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'utils' });
      expect(result.data.map((f) => f.relativePath)).toEqual(['src/utils/helpers.rs']);
    });

    it('a trailing slash is directory-only (subtree yes, same-named file no)', () => {
      seedFile(db, 'src', { fileType: 'text', extension: '' }); // a FILE literally named "src"
      const dirOnly = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src/' });
      expect(dirOnly.data.map((f) => f.relativePath).sort()).toEqual([
        'src/lib.rs',
        'src/main.rs',
        'src/server.ts',
        'src/utils/helpers.rs',
      ]);
      // ...whereas the un-suffixed literal also matches the same-named file.
      const withFile = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src' });
      expect(withFile.data.map((f) => f.relativePath)).toContain('src');
    });

    it('does not over-match a sibling with a shared prefix', () => {
      seedFile(db, 'srcgen/out.ts', { language: 'typescript' });
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src' });
      expect(result.data.map((f) => f.relativePath)).not.toContain('srcgen/out.ts');
    });

    it('excludeGlob with a bare directory name drops its whole subtree', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, excludeGlob: 'src' });
      expect(result.status).toBe('ok');
      // All four src/** files gone; the four non-src files remain.
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'Cargo.toml',
        'README.md',
        'config.yaml',
        'tests/test_main.rs',
      ]);
    });

    it('excludeGlob for a nested directory drops only that subtree', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, excludeGlob: 'utils' });
      expect(result.data.map((f) => f.relativePath)).not.toContain('src/utils/helpers.rs');
      expect(result.data.map((f) => f.relativePath)).toContain('src/main.rs');
    });

    it('countTrackedFiles agrees for a directory-shaped glob', () => {
      const list = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src' });
      const count = countTrackedFiles(db, { watchFolderId: WATCH_ID, glob: 'src' });
      expect(count).toBe(list.data.length);
      expect(count).toBe(4);
    });
  });

  // ── Brace alternation (regression) ────────────────────────────────────────
  // SQLite's GLOB operator has no `{a,b}`, so a braced pattern reached the
  // database as a literal and selected NOTHING — silently. The daemon-side FTS
  // glob behind `grep` has always expanded braces, so the identical pattern
  // answered from one tool and returned zero from `list`.
  describe('brace alternation', () => {
    it('expands {rs,ts} into both extensions', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.{rs,ts}' });
      expect(result.status).toBe('ok');
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'src/lib.rs',
        'src/main.rs',
        'src/server.ts',
        'src/utils/helpers.rs',
        'tests/test_main.rs',
      ]);
    });

    it('tolerates whitespace after the comma, like the daemon expansion', () => {
      const spaced = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.{rs, ts}' });
      const tight = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.{rs,ts}' });
      expect(spaced.data.map((f) => f.relativePath).sort()).toEqual(
        tight.data.map((f) => f.relativePath).sort()
      );
      expect(spaced.data.length).toBe(5);
    });

    it('applies directory shaping to each wildcard-free alternative', () => {
      const result = listTrackedFiles(db, {
        watchFolderId: WATCH_ID,
        glob: '{README.md,Cargo.toml}',
      });
      expect(result.data.map((f) => f.relativePath).sort()).toEqual(['Cargo.toml', 'README.md']);
    });

    it('expands nested groups, splitting only on top-level commas', () => {
      const result = listTrackedFiles(db, {
        watchFolderId: WATCH_ID,
        glob: '{src/{lib,main}.rs,README.md}',
      });
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'README.md',
        'src/lib.rs',
        'src/main.rs',
      ]);
    });

    it('excludes every alternative (NOT GLOB joined by AND is the complement)', () => {
      const result = listTrackedFiles(db, {
        watchFolderId: WATCH_ID,
        excludeGlob: '**/*.{md,toml,yaml}',
      });
      expect(result.data.map((f) => f.relativePath).sort()).toEqual([
        'src/lib.rs',
        'src/main.rs',
        'src/server.ts',
        'src/utils/helpers.rs',
        'tests/test_main.rs',
      ]);
    });

    it('countTrackedFiles agrees with listTrackedFiles for a braced glob', () => {
      const glob = '**/*.{rs,ts}';
      const list = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob });
      const count = countTrackedFiles(db, { watchFolderId: WATCH_ID, glob });
      expect(count).toBe(list.data.length);
      expect(count).toBe(5);
    });

    it('leaves an unbalanced brace literal (matches nothing here, never everything)', () => {
      const result = listTrackedFiles(db, { watchFolderId: WATCH_ID, glob: '**/*.{rs' });
      expect(result.status).toBe('ok');
      expect(result.data).toHaveLength(0);
    });
  });
});
