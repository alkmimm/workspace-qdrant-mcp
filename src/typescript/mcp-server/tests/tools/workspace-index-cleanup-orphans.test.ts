import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { describe, it, expect } from 'vitest';

import {
  readRegistry,
  runCleanupOrphans,
  testIndexedBranchHealth,
  type Registry,
  type RegistryBranch,
} from '../../src/tools/indexed-projects-registry.js';

function makeBranch(name: string, path: string): RegistryBranch {
  return {
    name,
    kind: 'agent',
    path,
    status: 'active',
    createdAt: '2026-01-01T00:00:00.000Z',
    lastSeenAt: '2026-01-01T00:00:00.000Z',
  };
}

function writeRegistryFile(dir: string, registry: Registry): string {
  const p = join(dir, 'indexed-projects.json');
  writeFileSync(p, JSON.stringify(registry, null, 4) + '\n', 'utf-8');
  return p;
}

describe('cleanup_orphans (TS-native port of Cleanup-OrphanedIndex)', () => {
  it('flags a registered branch whose checkout is gone as stale (path_missing)', () => {
    const health = testIndexedBranchHealth(makeBranch('gone', join(tmpdir(), 'wqm-does-not-exist-xyz')));
    expect(health.stale).toBe(true);
    expect(health.reason).toBe('path_missing');
    expect(health.pathExists).toBe(false);
  });

  it('report-only (mutate=false) reports orphans without touching the file', () => {
    const dir = mkdtempSync(join(tmpdir(), 'wqm-cleanup-'));
    const registry: Registry = {
      schemaVersion: 2,
      kind: 'indexed-projects',
      updatedAt: '2026-01-01T00:00:00.000Z',
      projects: [
        { name: 'DeadProj', root: join(dir, 'dead'), branches: [makeBranch('b1', join(dir, 'dead'))] },
      ],
    };
    const registryPath = writeRegistryFile(dir, registry);
    const before = readFileSync(registryPath, 'utf-8');

    const result = runCleanupOrphans({ registryPath }, false) as {
      mutated: boolean;
      removedBranchCount: number;
      removedProjectCount: number;
      removedProjects: Array<{ project: string }>;
    };

    expect(result.mutated).toBe(false);
    expect(result.removedBranchCount).toBe(1);
    expect(result.removedProjectCount).toBe(1);
    expect(result.removedProjects[0]?.project).toBe('DeadProj');
    // File must be byte-identical — report-only never writes.
    expect(readFileSync(registryPath, 'utf-8')).toBe(before);
  });

  it('mutate=true prunes stale branches + all-stale projects and keeps live ones', () => {
    const dir = mkdtempSync(join(tmpdir(), 'wqm-cleanup-'));

    // A real git repo with a committed branch → the one healthy entry.
    const liveRepo = join(dir, 'live');
    mkdirSync(liveRepo);
    const g = (...a: string[]) => execFileSync('git', ['-C', liveRepo, ...a], { stdio: 'ignore' });
    execFileSync('git', ['init', '-b', 'keepbranch', liveRepo], { stdio: 'ignore' });
    g('config', 'user.email', 'test@example.com');
    g('config', 'user.name', 'test');
    writeFileSync(join(liveRepo, 'f.txt'), 'x');
    g('add', '.');
    g('commit', '-m', 'init');

    const registry: Registry = {
      schemaVersion: 2,
      kind: 'indexed-projects',
      updatedAt: '2026-01-01T00:00:00.000Z',
      projects: [
        {
          name: 'Mixed',
          root: liveRepo,
          branches: [makeBranch('keepbranch', liveRepo), makeBranch('gone', join(dir, 'gone'))],
        },
        { name: 'AllDead', root: join(dir, 'alldead'), branches: [makeBranch('d', join(dir, 'd'))] },
      ],
    };
    const registryPath = writeRegistryFile(dir, registry);

    const result = runCleanupOrphans({ registryPath }, true) as {
      mutated: boolean;
      removedBranchCount: number;
      removedProjectCount: number;
      keptProjectCount: number;
    };

    expect(result.mutated).toBe(true);
    expect(result.removedProjectCount).toBe(1); // AllDead
    expect(result.keptProjectCount).toBe(1); // Mixed
    expect(result.removedBranchCount).toBe(2); // Mixed/'gone' + AllDead/'d'

    const after = readRegistry(registryPath);
    expect(after.projects).toHaveLength(1);
    expect(after.projects[0]?.name).toBe('Mixed');
    expect(after.projects[0]?.branches).toHaveLength(1);
    expect(after.projects[0]?.branches[0]?.name).toBe('keepbranch');
  });
});
