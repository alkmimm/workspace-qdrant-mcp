/**
 * indexed-projects-registry.ts — TypeScript port of the Phase 1 surface of
 * `scripts/windows/indexed-projects-registry.ps1`.
 *
 * Implements the registry actions that don't require the `wqm` CLI binary,
 * so the dockerized MCP container (Linux, no PowerShell, no wqm.exe) can
 * serve them. Actions that integrate with `wqm` (observe, incremental-check,
 * register-wqm) stay PowerShell-only for now and fall back to spawn(pwsh).
 *
 * Schema is byte-compatible with the PS1 implementation: same field names,
 * same enum values for `kind` / `status`, same `[ordered]` insertion order in
 * the serialized JSON.
 */

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';

import {
  findGitRoot,
  getCurrentBranch,
  getGitRemoteUrl,
  getHeadCommit,
  isGitRepository,
  isWorktree,
} from './../utils/git-utils.js';
import {
  getGitSnapshot,
  newObservation,
  probeDaemonProjectStatus,
  probeDaemonQueue,
  probeDaemonWatches,
  saveObservation,
  type DaemonProjectStatusResult,
  type DaemonQueueResult,
  type DaemonWatchListResult,
} from './indexed-projects-observations.js';
import { DEFAULT_CONFIG } from './../types/generated-defaults.js';
import type { DaemonClient } from './../clients/daemon-client.js';

// ── Types ───────────────────────────────────────────────────────────────────

export interface RegistryBranch {
  name: string;
  kind: string; // 'manual' | 'manual-worktree' | 'agent' | 'primary'
  path: string;
  baseBranch?: string;
  returnBranch?: string;
  status: string; // 'active' | 'ready_for_review' | 'abandoned'
  createdBy?: string;
  createdAt: string;
  lastSeenAt: string;
  baseCommit?: string | null;
  headCommit?: string | null;
  lastIndexedCommit?: string | null;
  watchEnabled?: boolean;
  indexed?: boolean;
  purpose?: string;
  useWorktree?: boolean;
  note?: string;
}

export interface RegistryProject {
  name: string;
  root: string;
  projectId?: string | null;
  qdrantUrl?: string;
  daemonEndpoint?: string;
  defaultBranch?: string;
  tenantStrategy?: string;
  enabled?: boolean;
  createdAt?: string;
  updatedAt?: string;
  branches: RegistryBranch[];
}

export interface Registry {
  schemaVersion: number;
  kind: string;
  updatedAt: string;
  projects: RegistryProject[];
}

export interface ProjectSelector {
  projectName?: string;
  projectId?: string;
  projectDir?: string;
}

// ── Constants ───────────────────────────────────────────────────────────────

const SCHEMA_VERSION = 2;
const REGISTRY_KIND = 'indexed-projects';

// ── Time + path helpers ─────────────────────────────────────────────────────

function utcNow(): string {
  return new Date().toISOString();
}

/**
 * Translate a host-style path (`C:\Users\alber\...` or `C:/Users/alber/...`)
 * to the bind-mount path visible inside the container (`/run/desktop/...`),
 * using the WQM_HOST_DEV_ROOT → WQM_DEV_ROOT translation declared in
 * `docker/.env`. Returns the original path unchanged when:
 *   - env vars are unset (host-side execution: paths are already native);
 *   - the input doesn't sit under WQM_HOST_DEV_ROOT;
 *   - the input is already in container form.
 */
function translateHostPath(pathValue: string): string {
  if (!pathValue) return pathValue;
  const hostRoot = process.env['WQM_HOST_DEV_ROOT'];
  const devRoot = process.env['WQM_DEV_ROOT'];
  if (!hostRoot || !devRoot) return pathValue;

  // Normalize separators on the input AND the host-root marker so the prefix
  // match works regardless of which slash style the JSON happens to use.
  const inputForward = pathValue.replace(/\\/g, '/');
  const hostForward = hostRoot.replace(/\\/g, '/').replace(/\/+$/, '');

  // Case-insensitive comparison covers `C:` vs `c:` on Windows-origin paths.
  const lowerInput = inputForward.toLowerCase();
  const lowerHost = hostForward.toLowerCase();
  if (lowerInput === lowerHost) return devRoot;
  if (lowerInput.startsWith(lowerHost + '/')) {
    return devRoot + inputForward.slice(hostForward.length);
  }
  return pathValue;
}

function toAbs(pathValue: string): string {
  if (!pathValue) return pathValue;
  // First, translate host→container if we're running inside the docker MCP.
  const translated = translateHostPath(pathValue);
  // Then resolve relative components against cwd. We only call resolve() for
  // already-absolute paths in practice; this also handles posix normalization.
  if (translated.match(/^([A-Za-z]:[\\/]|\/)/)) {
    // Already absolute (POSIX or Windows). Just normalize separators.
    return translated.replace(/\\/g, '/');
  }
  return resolve(translated);
}

export function defaultRegistryPath(repoDir: string): string {
  return resolve(repoDir, '.wqm-fork', 'indexed-projects.json');
}

function ensureDir(dirPath: string): void {
  if (!existsSync(dirPath)) {
    mkdirSync(dirPath, { recursive: true });
  }
}

// ── Branch slug (matches PS Safe-BranchSlug) ────────────────────────────────

export function safeBranchSlug(name: string): string {
  return name.replace(/[^A-Za-z0-9._-]+/g, '-').replace(/^-+|-+$/g, '');
}

// ── Registry IO ─────────────────────────────────────────────────────────────

export function newRegistry(): Registry {
  return {
    schemaVersion: SCHEMA_VERSION,
    kind: REGISTRY_KIND,
    updatedAt: utcNow(),
    projects: [],
  };
}

export function readRegistry(registryPath: string): Registry {
  if (!existsSync(registryPath)) return newRegistry();
  const raw = readFileSync(registryPath, 'utf-8').trim();
  if (!raw) return newRegistry();
  const parsed = JSON.parse(raw) as Registry;
  // Defensive: tolerate missing schema fields from older registries.
  if (!parsed.projects) parsed.projects = [];
  if (!parsed.schemaVersion) parsed.schemaVersion = SCHEMA_VERSION;
  if (!parsed.kind) parsed.kind = REGISTRY_KIND;
  return parsed;
}

export function writeRegistry(registryPath: string, registry: Registry): void {
  registry.updatedAt = utcNow();
  ensureDir(dirname(registryPath));
  writeFileSync(registryPath, JSON.stringify(registry, null, 4) + '\n', 'utf-8');
}

// ── Orphan cleanup (port of PS Cleanup-OrphanedIndex) ────────────────────────

export interface IndexedBranchHealth {
  path: string;
  pathExists: boolean;
  gitRepo: boolean;
  branchExists: boolean;
  stale: boolean;
  reason: string;
  branchName: string;
  kind?: string | undefined;
  status?: string | undefined;
  useWorktree?: boolean | undefined;
}

/** Does `branchName` still exist as a local ref in the git repo at `repoPath`? */
function branchExistsInRepo(repoPath: string, branchName: string): boolean {
  try {
    execFileSync(
      'git',
      ['-C', repoPath, 'rev-parse', '--verify', '--quiet', `refs/heads/${branchName}`],
      { stdio: ['ignore', 'ignore', 'ignore'], timeout: 10_000 }
    );
    return true;
  } catch {
    return false;
  }
}

/**
 * Port of the PowerShell `Test-IndexedBranchHealth`: a registered branch is
 * stale when its checkout path is gone, is no longer a git repo, or the branch
 * ref itself has been deleted. The path is host→container translated first
 * (via toAbs), so this works host-native and inside the dockerized MCP
 * container alike.
 */
export function testIndexedBranchHealth(branch: RegistryBranch): IndexedBranchHealth {
  const path = toAbs(branch.path);
  const pathExists = existsSync(path);
  let gitRepo = false;
  let branchExists = false;
  const reasons: string[] = [];

  if (!pathExists) {
    reasons.push('path_missing');
  } else {
    gitRepo = existsSync(join(path, '.git'));
    if (!gitRepo) {
      reasons.push('git_repo_missing');
    } else if (!branchExistsInRepo(path, branch.name)) {
      reasons.push('branch_missing');
    } else {
      branchExists = true;
    }
  }

  return {
    path,
    pathExists,
    gitRepo,
    branchExists,
    stale: reasons.length > 0,
    reason: reasons.join(','),
    branchName: branch.name,
    kind: branch.kind,
    status: branch.status,
    useWorktree: branch.useWorktree,
  };
}

interface RemovedBranchReport {
  project: string;
  branch: string;
  path: string;
  kind?: string | undefined;
  status?: string | undefined;
  reason: string;
}

interface RemovedProjectReport {
  project: string;
  root: string;
}

/**
 * Port of the PowerShell `Cleanup-OrphanedIndex`. Drops registry branches whose
 * checkout is gone (or is no longer a valid repo/ref) and any project left with
 * zero live branches. TS-native so it runs inside the dockerized MCP container
 * — the PowerShell bridge cannot (no `pwsh` in the image, see issue #300).
 *
 * Scope: this operates ONLY on the `.wqm-fork/indexed-projects.json` registry.
 * It does NOT touch daemon-side watch/tenant state, so a daemon-only orphan (a
 * project the daemon indexes but that is absent from this file) is out of scope.
 *
 * @param mutate when true, writes the pruned registry back; otherwise reports
 *   what WOULD be removed without touching the file.
 */
export function runCleanupOrphans(args: BaseArgs, mutate: boolean): unknown {
  const registry = readRegistry(args.registryPath);
  const removedBranches: RemovedBranchReport[] = [];
  const removedProjects: RemovedProjectReport[] = [];
  const keptProjects: RegistryProject[] = [];

  for (const project of registry.projects) {
    const keptBranches: RegistryBranch[] = [];
    for (const branch of project.branches ?? []) {
      const health = testIndexedBranchHealth(branch);
      if (health.stale) {
        removedBranches.push({
          project: project.name,
          branch: branch.name,
          path: health.path,
          kind: health.kind,
          status: health.status,
          reason: health.reason,
        });
        continue;
      }
      keptBranches.push(branch);
    }

    if (keptBranches.length === 0) {
      removedProjects.push({ project: project.name, root: project.root });
      continue;
    }

    if (mutate) {
      project.branches = keptBranches;
      project.updatedAt = utcNow();
    }
    keptProjects.push(project);
  }

  if (mutate) {
    registry.projects = keptProjects;
    writeRegistry(args.registryPath, registry);
  }

  return {
    success: true,
    action: 'cleanup_orphans',
    mutated: mutate,
    removedBranchCount: removedBranches.length,
    removedProjectCount: removedProjects.length,
    removedBranches,
    removedProjects,
    keptProjectCount: keptProjects.length,
  };
}

// ── Lookup helpers ──────────────────────────────────────────────────────────

function normalizeProject(p: RegistryProject): RegistryProject {
  return {
    ...p,
    root: toAbs(p.root),
    qdrantUrl: p.qdrantUrl ?? DEFAULT_CONFIG.qdrant.url,
    daemonEndpoint:
      p.daemonEndpoint ?? `${DEFAULT_CONFIG.daemon.grpcHost}:${DEFAULT_CONFIG.daemon.grpcPort}`,
    defaultBranch: p.defaultBranch ?? 'main',
    tenantStrategy: p.tenantStrategy ?? 'project',
    enabled: p.enabled ?? true,
    branches: p.branches ?? [],
  };
}

export function findProject(registry: Registry, sel: ProjectSelector): RegistryProject {
  const projects = registry.projects.map(normalizeProject);
  let candidates = projects;

  if (sel.projectDir) {
    const target = resolveProjectRoot(sel.projectDir);
    const exact = projects.filter((p) => toAbs(p.root) === target);
    if (exact.length === 1) return exact[0] as RegistryProject;
    if (exact.length > 1) {
      throw new Error(`Ambiguous project (root match): ${sel.projectDir}`);
    }
    if (!sel.projectName && !sel.projectId) {
      throw new Error(`Indexed project not found: ${sel.projectDir}`);
    }
    // Fall through to name/id matching if root didn't pin it but an explicit
    // name/id was also supplied.
  }

  if (sel.projectName) {
    candidates = candidates.filter((p) => p.name === sel.projectName);
  }
  if (sel.projectId) {
    candidates = candidates.filter((p) => p.projectId === sel.projectId);
  }

  if (!sel.projectName && !sel.projectId && !sel.projectDir) {
    throw new Error('Provide projectName, projectId, or projectDir.');
  }
  if (candidates.length === 0) {
    throw new Error(
      `Indexed project not found: ${sel.projectName ?? ''} ${sel.projectId ?? ''} ${sel.projectDir ?? ''}`.trim()
    );
  }
  if (candidates.length > 1) {
    throw new Error(
      `Ambiguous project: ${sel.projectName ?? ''} ${sel.projectId ?? ''} ${sel.projectDir ?? ''}`.trim()
    );
  }
  return candidates[0] as RegistryProject;
}

export function findProjectByRoot(registry: Registry, rootPath: string): RegistryProject | null {
  if (!rootPath) return null;
  const abs = resolveProjectRoot(rootPath);
  return registry.projects.map(normalizeProject).find((p) => toAbs(p.root) === abs) ?? null;
}

export function findBranch(
  project: RegistryProject,
  branchName: string
): RegistryBranch | undefined {
  return (project.branches ?? []).find((b) => b.name === branchName);
}

export function upsertProject(registry: Registry, project: RegistryProject): void {
  const idx = registry.projects.findIndex((p) => p.name === project.name);
  if (idx >= 0) {
    registry.projects[idx] = project;
  } else {
    registry.projects.push(project);
  }
}

export function upsertBranch(
  registry: Registry,
  projectName: string,
  branch: RegistryBranch
): void {
  const project = registry.projects.find((p) => p.name === projectName);
  if (!project) throw new Error(`Project not found: ${projectName}`);
  if (!project.branches) project.branches = [];
  const idx = project.branches.findIndex((b) => b.name === branch.name);
  if (idx >= 0) {
    project.branches[idx] = branch;
  } else {
    project.branches.push(branch);
  }
  project.updatedAt = utcNow();
}

// ── Git helpers (using local git CLI) ───────────────────────────────────────

function resolveProjectRoot(pathValue: string): string {
  // For worktrees, walk up until we hit the *main* repo (the one whose .git
  // is a directory). The PS impl uses git --git-common-dir; we mirror it.
  if (!pathValue) return pathValue;
  const root = findGitRoot(pathValue);
  if (!root) return toAbs(pathValue);
  // If linked worktree, traverse to common dir's parent.
  if (isWorktree(root)) {
    try {
      const commonDir = execFileSync('git', ['-C', root, 'rev-parse', '--git-common-dir'], {
        encoding: 'utf-8',
        timeout: 5000,
      }).trim();
      const absCommon = isAbsolute(commonDir) ? commonDir : resolve(root, commonDir);
      return resolve(dirname(absCommon));
    } catch {
      return toAbs(root);
    }
  }
  return toAbs(root);
}

function branchExists(repo: string, branch: string): boolean {
  try {
    execFileSync('git', ['-C', repo, 'rev-parse', '--verify', branch], {
      encoding: 'utf-8',
      timeout: 5000,
      stdio: ['ignore', 'pipe', 'ignore'],
    });
    return true;
  } catch {
    return false;
  }
}

function gitRevParse(repo: string, ref: string): string | null {
  try {
    return execFileSync('git', ['-C', repo, 'rev-parse', ref], {
      encoding: 'utf-8',
      timeout: 5000,
    }).trim();
  } catch {
    return null;
  }
}

// ── Action handlers ─────────────────────────────────────────────────────────

export interface BaseArgs {
  registryPath: string;
}

export interface ProjectArgs extends BaseArgs, ProjectSelector {}

export interface BranchArgs extends ProjectArgs {
  branchName: string;
}

export interface StartAgentBranchArgs extends ProjectArgs {
  branchName: string;
  baseBranch?: string;
  returnBranch?: string;
  worktreePath?: string;
  worktreeRoot?: string;
  useWorktree?: boolean;
  purpose?: string;
  createdBy?: string;
}

export interface AbandonAgentBranchArgs extends BranchArgs {
  removeWorktree?: boolean;
}

export function runInit(args: BaseArgs): unknown {
  if (!existsSync(args.registryPath)) {
    writeRegistry(args.registryPath, newRegistry());
  }
  return { success: true, action: 'init', registry: args.registryPath };
}

interface ListedProject {
  name: string;
  projectId: string | null;
  root: string;
  defaultBranch: string;
  tenantStrategy: string;
  enabled: boolean;
  /** `registered` = in indexed-projects.json only; `indexed` = the daemon is
   *  indexing it but it's not in the registry; `both` = present in both. */
  source: 'registered' | 'indexed' | 'both';
}

export async function runListProjects(
  args: BaseArgs,
  daemonClient?: DaemonClient | null
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  const projects: ListedProject[] = registry.projects.map((p) => ({
    name: p.name,
    projectId: p.projectId ?? null,
    root: toAbs(p.root),
    defaultBranch: p.defaultBranch ?? 'main',
    tenantStrategy: p.tenantStrategy ?? 'project',
    enabled: p.enabled ?? true,
    source: 'registered',
  }));

  // Cross-reference the daemon's actually-indexed projects (watch_folders) so a
  // project the daemon indexes but that was never written to
  // indexed-projects.json (the eval's DOC-V2 case) is still visible. Match on
  // canonical root (then name); a daemon match fills a null registry projectId
  // and promotes the entry to `both`. Daemon-only projects are appended as
  // `indexed`. Best-effort: if the daemon is unreachable, fall back to the
  // registry-only listing (prior behavior).
  let daemonReachable = false;
  if (daemonClient) {
    try {
      const list = await daemonClient.listProjects({});
      daemonReachable = true;
      const byRoot = new Map(projects.map((p) => [p.root.toLowerCase(), p]));
      for (const dp of list.projects ?? []) {
        const root = toAbs(dp.project_root);
        const match =
          byRoot.get(root.toLowerCase()) ?? projects.find((p) => p.name === dp.project_name);
        if (match) {
          match.source = 'both';
          if (!match.projectId && dp.project_id) match.projectId = dp.project_id;
        } else {
          projects.push({
            name: dp.project_name,
            projectId: dp.project_id,
            root,
            defaultBranch: 'main',
            tenantStrategy: 'project',
            enabled: true,
            source: 'indexed',
          });
        }
      }
    } catch {
      // Daemon unavailable — registry-only listing.
    }
  }

  return {
    success: true,
    registry: args.registryPath,
    daemonReachable,
    projects,
  };
}

export async function runListBranches(
  args: ProjectArgs,
  daemonClient?: DaemonClient | null
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  try {
    const project = findProject(registry, args);
    return {
      success: true,
      project: project.name,
      branches: project.branches ?? [],
    };
  } catch (err) {
    if (!daemonClient) throw err;

    const daemonProject = await findDaemonProject(daemonClient, args);
    if (!daemonProject) throw err;

    const root = toAbs(daemonProject.project_root);
    const branchName = getCurrentBranch(root) ?? 'main';
    return {
      success: true,
      project: daemonProject.project_name,
      projectId: daemonProject.project_id,
      source: 'indexed',
      branches: [
        {
          name: branchName,
          kind: 'primary',
          path: root,
          status: daemonProject.is_active ? 'active' : 'inactive',
          createdAt: utcNow(),
          lastSeenAt: utcNow(),
          watchEnabled: true,
          indexed: true,
          note: 'Synthesized from daemon ListProjects; project is not registered in indexed-projects.json.',
        },
      ],
    };
  }
}

async function findDaemonProject(
  daemonClient: DaemonClient,
  sel: ProjectSelector
): Promise<Awaited<ReturnType<DaemonClient['listProjects']>>['projects'][number] | null> {
  const list = await daemonClient.listProjects({});
  const projects = list.projects ?? [];
  if (sel.projectDir) {
    const target = resolveProjectRoot(sel.projectDir).toLowerCase();
    const match = projects.find((p) => toAbs(p.project_root).toLowerCase() === target);
    if (match) return match;
  }
  if (sel.projectId) {
    const match = projects.find((p) => p.project_id === sel.projectId);
    if (match) return match;
  }
  if (sel.projectName) {
    const match = projects.find((p) => p.project_name === sel.projectName);
    if (match) return match;
  }
  return null;
}

export function runAgentBranchStatus(args: BranchArgs): unknown {
  const registry = readRegistry(args.registryPath);
  const project = findProject(registry, args);
  const branch = findBranch(project, args.branchName);
  if (!branch) throw new Error(`Branch not registered: ${args.branchName}`);
  // Match PS shape: include the live git snapshot from the branch's working
  // tree. `path` may be a worktree separate from project.root, so we snapshot
  // that path directly. When the path no longer exists, surface ok=false with
  // a structured error instead of throwing — the registry entry is still
  // useful to the LLM in that case.
  const branchPath = toAbs(branch.path);
  const git = existsSync(branchPath)
    ? getGitSnapshot(branchPath, branch.baseBranch ?? '')
    : { ok: false, error: 'path missing' };
  return {
    success: true,
    project: project.name,
    branch,
    git,
  };
}

// ── Read-action handlers (Phase 2 port: observation/status surface) ──────

export async function runProjectStatus(
  args: ProjectArgs,
  daemonClient: DaemonClient | null | undefined
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  const project = findProject(registry, args);
  const observation = await newObservation(project, daemonClient);
  return {
    success: true,
    project: project.name,
    root: project.root,
    branches: project.branches ?? [],
    observation,
  };
}

export async function runStatusAll(
  args: BaseArgs,
  daemonClient: DaemonClient | null | undefined
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  const projects: RegistryProject[] = registry.projects
    .map(normalizeProjectExport)
    .filter((p) => p.enabled);

  // Surface daemon-indexed projects that are absent from indexed-projects.json
  // (the DOC-V2 case) so status_all reflects what the daemon actually serves,
  // not a stale/polluted registry. Mirrors runListProjects' daemon
  // cross-reference and the incremental_check synthesized-project fallback.
  // Best-effort: if the daemon is unreachable, fall back to the registry-only
  // listing (prior behavior).
  if (daemonClient) {
    try {
      const list = await daemonClient.listProjects({});
      const knownRoots = new Set(projects.map((p) => toAbs(p.root).toLowerCase()));
      const knownNames = new Set(projects.map((p) => p.name));
      for (const dp of list.projects ?? []) {
        const root = toAbs(dp.project_root);
        if (knownRoots.has(root.toLowerCase()) || knownNames.has(dp.project_name)) continue;
        // Route through normalizeProject so synthesized daemon-only projects get
        // the same qdrantUrl / daemonEndpoint defaults as registry projects —
        // otherwise newObservation's qdrant/daemonTcp probes report "not
        // configured" for every synthesized project (the 11-of-12 case).
        projects.push(
          normalizeProject({
            name: dp.project_name,
            projectId: dp.project_id,
            root,
            defaultBranch: getCurrentBranch(root) ?? 'main',
            tenantStrategy: 'project',
            enabled: true,
            branches: [],
          })
        );
      }
    } catch {
      // Daemon unavailable — registry-only status.
    }
  }

  // The unified ingestion queue is daemon-WIDE: GetQueueStats(Empty) has no
  // tenant filter and every project shares the `projects` collection, so a
  // per-project queue breakdown is not available from the daemon. Probing it
  // inside each observation just repeated the same global counts across every
  // project (misleading). Probe it ONCE and report it at the top level as
  // `daemonQueue`; skip the per-project queue probe and strip the placeholder.
  // (Per-tenant queue stats would need a tenant filter on GetQueueStats — see
  // docs/specs/14-future-development.md.)
  const daemonQueue = await probeDaemonQueue(daemonClient);
  const rawObservations = await Promise.all(
    projects.map((p) => newObservation(p, daemonClient, { skipQueue: true }))
  );
  const observations = rawObservations.map(({ queue: _queue, ...rest }) => rest);
  return {
    success: true,
    count: observations.length,
    daemonQueue,
    projects: observations,
  };
}

export async function runObserveProject(
  args: ProjectArgs,
  daemonClient: DaemonClient | null | undefined
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  const project = findProject(registry, args);
  const observation = await newObservation(project, daemonClient);
  const savedTo = saveObservation(args.registryPath, observation);
  return {
    success: true,
    action: 'observe_project',
    observation,
    savedTo,
  };
}

export async function runObserveAll(
  args: BaseArgs,
  daemonClient: DaemonClient | null | undefined
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  const enabled = registry.projects.map(normalizeProjectExport).filter((p) => p.enabled);
  const observations = await Promise.all(
    enabled.map(async (p) => {
      const obs = await newObservation(p, daemonClient);
      saveObservation(args.registryPath, obs);
      return obs;
    })
  );
  return {
    success: true,
    action: 'observe_all',
    count: observations.length,
    observations,
  };
}

interface IncrementalBranchResult {
  project?: string;
  branch: string;
  path: string;
  projectStatus: DaemonProjectStatusResult;
  queue: DaemonQueueResult;
  watchList?: DaemonWatchListResult;
}

/**
 * Per-project incremental check, sourced from the daemon over gRPC instead of
 * the `wqm` CLI:
 *   - `wqm project status` / `wqm project check` → GetProjectStatus (the
 *     pending/in_progress/done counts are the "what needs indexing" signal)
 *   - `wqm queue stats`                          → GetQueueStats
 *   - `wqm watch list`                           → ListWatches
 * The daemon tenant id comes from the registry; when absent we resolve it by
 * matching the project root against the daemon's ListProjects.
 */
async function checkBranchesForProject(
  project: RegistryProject,
  daemonClient: DaemonClient | null | undefined,
  includeWatch: boolean
): Promise<IncrementalBranchResult[]> {
  // Resolve the daemon tenant id. The registry's projectId can be stale (e.g. a
  // `local_` id when the daemon tracks the repo under a git-remote tenant), so
  // prefer the daemon's own ListProjects: match by container-translated path
  // (disambiguates worktrees) then by project name, falling back to the
  // registry id only when the daemon has no match.
  let projectId = project.projectId ?? undefined;
  if (daemonClient) {
    try {
      const list = await daemonClient.listProjects({});
      const target = toAbs(project.root).toLowerCase();
      const match =
        list.projects.find((p) => toAbs(p.project_root).toLowerCase() === target) ??
        list.projects.find((p) => p.project_name === project.name);
      if (match?.project_id) projectId = match.project_id;
    } catch {
      // Keep the registry projectId — probeDaemonProjectStatus reports any gap.
    }
  }

  // project status + queue are independent; watch list only when requested.
  const [projectStatus, queue] = await Promise.all([
    probeDaemonProjectStatus(daemonClient, projectId),
    probeDaemonQueue(daemonClient),
  ]);
  const watchList = includeWatch ? await probeDaemonWatches(daemonClient, 'projects') : undefined;

  const results: IncrementalBranchResult[] = [];
  for (const b of project.branches ?? []) {
    const path = toAbs(b.path ?? project.root);
    const r: IncrementalBranchResult = {
      project: project.name,
      branch: b.name,
      path,
      projectStatus,
      queue,
    };
    if (watchList !== undefined) r.watchList = watchList;
    results.push(r);
  }
  return results;
}

export async function runIncrementalCheck(
  args: ProjectArgs,
  daemonClient: DaemonClient | null | undefined
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  let project: RegistryProject;
  let synthesized = false;
  try {
    project = findProject(registry, args);
  } catch (err) {
    // Synthesized/unregistered project: the daemon indexes it (ListProjects) but
    // it is absent from indexed-projects.json. Don't hard-fail with "Indexed
    // project not found" — mirror list_branches / project_status and synthesize a
    // project from the daemon so the daemon-backed incremental check still runs.
    const synth = daemonClient ? await synthesizeProjectFromDaemon(daemonClient, args) : null;
    if (!synth) throw err;
    project = synth;
    synthesized = true;
  }
  const results = await checkBranchesForProject(project, daemonClient, /* includeWatch */ true);
  // PS strips the redundant `project` field on the per-project variant.
  const stripped = results.map(({ project: _omit, ...rest }) => rest);
  return {
    success: true,
    action: 'incremental_check',
    project: project.name,
    ...(synthesized
      ? {
          source: 'indexed',
          note: 'Synthesized from daemon ListProjects; project is not registered in indexed-projects.json.',
        }
      : {}),
    results: stripped,
  };
}

/**
 * Build a minimal RegistryProject from the daemon's ListProjects for a project
 * the daemon indexes but that is absent from indexed-projects.json. Mirrors the
 * synthesized-project path in {@link runListBranches}. Returns null when the
 * daemon has no matching project (so the caller keeps the original error).
 */
async function synthesizeProjectFromDaemon(
  daemonClient: DaemonClient,
  sel: ProjectSelector
): Promise<RegistryProject | null> {
  const dp = await findDaemonProject(daemonClient, sel);
  if (!dp) return null;
  const root = toAbs(dp.project_root);
  const branchName = getCurrentBranch(root) ?? 'main';
  const now = utcNow();
  return {
    name: dp.project_name,
    projectId: dp.project_id,
    root,
    defaultBranch: branchName,
    tenantStrategy: 'project',
    enabled: true,
    branches: [
      {
        name: branchName,
        kind: 'primary',
        path: root,
        status: dp.is_active ? 'active' : 'inactive',
        createdAt: now,
        lastSeenAt: now,
        watchEnabled: true,
        indexed: true,
        note: 'Synthesized from daemon ListProjects.',
      },
    ],
  };
}

export async function runIncrementalCheckAll(
  args: BaseArgs,
  daemonClient: DaemonClient | null | undefined
): Promise<unknown> {
  const registry = readRegistry(args.registryPath);
  const enabled = registry.projects.map(normalizeProjectExport).filter((p) => p.enabled);
  const all: IncrementalBranchResult[] = [];
  for (const project of enabled) {
    all.push(...(await checkBranchesForProject(project, daemonClient, /* includeWatch */ false)));
  }
  return {
    success: true,
    action: 'incremental_check_all',
    results: all,
  };
}

// Re-export normalizeProject for the read-action handlers above. (The
// internal `normalizeProject` declared earlier is module-private.)
function normalizeProjectExport(p: RegistryProject): RegistryProject {
  return normalizeProject(p);
}

export function runStartAgentBranch(args: StartAgentBranchArgs): unknown {
  if (!args.branchName) throw new Error('branchName is required');

  const registry = readRegistry(args.registryPath);

  // Find project (may auto-create from worktreePath via sync semantics).
  let project: RegistryProject;
  try {
    project = findProject(registry, args);
  } catch (err) {
    // If projectDir was provided and we can detect a git repo there,
    // bootstrap a project entry. Otherwise propagate.
    if (args.projectDir && isGitRepository(args.projectDir)) {
      const root = resolveProjectRoot(args.projectDir);
      const remote = getGitRemoteUrl(root);
      const nameOpt = args.projectName ?? root.split(/[\\/]/).pop() ?? 'unknown';
      project = {
        name: nameOpt,
        root,
        projectId: null,
        defaultBranch: args.baseBranch ?? 'main',
        tenantStrategy: 'project',
        enabled: true,
        createdAt: utcNow(),
        updatedAt: utcNow(),
        branches: [],
      };
      // remote URL noted but not yet wired into registry schema
      void remote;
      upsertProject(registry, project);
    } else {
      throw err;
    }
  }

  const repo = toAbs(project.root);
  const baseBranch = args.baseBranch ?? project.defaultBranch ?? 'main';
  const useWt = args.useWorktree === true;
  const slug = safeBranchSlug(args.branchName);

  let branchPath: string;
  let baseCommit: string | null = null;
  const wtPath = args.worktreePath
    ? toAbs(args.worktreePath)
    : args.worktreeRoot
      ? join(toAbs(args.worktreeRoot), `${repo.split(/[\\/]/).pop()}-${slug}`)
      : join(dirname(repo), `${repo.split(/[\\/]/).pop()}-${slug}`);

  if (useWt) {
    // Worktree mode: create new worktree, OR adopt existing one (backfill).
    if (existsSync(wtPath)) {
      // Backfill: worktree already exists on disk. Just register it.
      if (!isGitRepository(wtPath)) {
        throw new Error(`worktreePath exists but is not a valid git worktree: ${wtPath}`);
      }
      baseCommit = gitRevParse(repo, baseBranch);
      branchPath = wtPath;
    } else {
      // Fresh worktree creation.
      baseCommit = gitRevParse(repo, baseBranch);
      if (!baseCommit) throw new Error(`baseBranch not found: ${baseBranch}`);
      if (branchExists(repo, args.branchName)) {
        execFileSync('git', ['-C', repo, 'worktree', 'add', wtPath, args.branchName], {
          stdio: 'inherit',
          timeout: 60000,
        });
      } else {
        execFileSync(
          'git',
          ['-C', repo, 'worktree', 'add', '-b', args.branchName, wtPath, baseBranch],
          { stdio: 'inherit', timeout: 60000 }
        );
      }
      branchPath = wtPath;
    }
  } else {
    // In-place checkout mode. Refuse on dirty tree (matches PS Assert-Clean).
    const status = execFileSync('git', ['-C', repo, 'status', '--porcelain'], {
      encoding: 'utf-8',
      timeout: 5000,
    });
    if (status.trim().length > 0) {
      throw new Error('working tree is not clean; commit or stash first');
    }
    execFileSync('git', ['-C', repo, 'checkout', baseBranch], {
      stdio: 'inherit',
      timeout: 15000,
    });
    baseCommit = getHeadCommit(repo);
    if (branchExists(repo, args.branchName)) {
      execFileSync('git', ['-C', repo, 'checkout', args.branchName], {
        stdio: 'inherit',
        timeout: 15000,
      });
    } else {
      execFileSync('git', ['-C', repo, 'checkout', '-b', args.branchName], {
        stdio: 'inherit',
        timeout: 15000,
      });
    }
    branchPath = repo;
  }

  const head = getHeadCommit(branchPath);
  const returnBranch = args.returnBranch ?? getCurrentBranch(repo) ?? baseBranch;

  const branch: RegistryBranch = {
    name: args.branchName,
    kind: 'agent',
    path: toAbs(branchPath),
    baseBranch,
    returnBranch,
    status: 'active',
    createdBy: args.createdBy ?? 'ai-agent',
    createdAt: utcNow(),
    lastSeenAt: utcNow(),
    baseCommit,
    headCommit: head,
    lastIndexedCommit: null,
    watchEnabled: true,
    indexed: false,
    purpose: args.purpose ?? 'agent change',
    useWorktree: useWt,
  };

  upsertBranch(registry, project.name, branch);
  writeRegistry(args.registryPath, registry);

  return {
    success: true,
    action: 'start_agent_branch',
    project: project.name,
    branch,
    message:
      'Agent branch registered. Daemon registration happens via git hook or sync_current_branch.',
  };
}

export function runFinishAgentBranch(args: BranchArgs): unknown {
  if (!args.branchName) throw new Error('branchName is required');
  const registry = readRegistry(args.registryPath);
  const project = findProject(registry, args);
  const branch = findBranch(project, args.branchName);
  if (!branch) throw new Error(`Branch not registered: ${args.branchName}`);

  const path = toAbs(branch.path);
  if (existsSync(path)) {
    const head = getHeadCommit(path);
    if (head) branch.headCommit = head;
  }
  branch.lastSeenAt = utcNow();
  branch.status = 'ready_for_review';
  branch.note = 'Ready for human review. Merge not performed.';

  upsertBranch(registry, project.name, branch);
  writeRegistry(args.registryPath, registry);

  return {
    success: true,
    action: 'finish_agent_branch',
    project: project.name,
    branch,
    message: 'Marked as ready_for_review without merge.',
  };
}

export function runAbandonAgentBranch(args: AbandonAgentBranchArgs): unknown {
  if (!args.branchName) throw new Error('branchName is required');
  const registry = readRegistry(args.registryPath);
  const project = findProject(registry, args);
  const branch = findBranch(project, args.branchName);
  if (!branch) throw new Error(`Branch not registered: ${args.branchName}`);

  branch.status = 'abandoned';
  branch.lastSeenAt = utcNow();
  branch.note = 'Abandoned in the registry. Worktree/branch not deleted automatically.';

  if (args.removeWorktree === true && branch.useWorktree) {
    try {
      execFileSync('git', ['-C', toAbs(project.root), 'worktree', 'remove', toAbs(branch.path)], {
        stdio: 'inherit',
        timeout: 30000,
      });
      branch.note = 'Abandoned and worktree removed by explicit request.';
    } catch (err) {
      branch.note = `Abandoned; worktree remove failed: ${(err as Error).message}`;
    }
  }

  upsertBranch(registry, project.name, branch);
  writeRegistry(args.registryPath, registry);

  return {
    success: true,
    action: 'abandon_agent_branch',
    project: project.name,
    branch,
  };
}
