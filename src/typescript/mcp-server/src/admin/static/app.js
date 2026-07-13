/**
 * workspace-qdrant admin UI client.
 *
 * Vanilla JS — no framework, no build step. Auth via Bearer token kept
 * in sessionStorage. Real-time refresh is plain polling (5s) so the
 * server stays single-protocol; no SSE / websocket complexity.
 */

const TOKEN_KEY = 'wqm.admin.token';
const REFRESH_MS = 5000;

const els = {
  loginPanel: document.getElementById('loginPanel'),
  loginForm: document.getElementById('loginForm'),
  tokenInput: document.getElementById('tokenInput'),
  loginError: document.getElementById('loginError'),
  appView: document.getElementById('appView'),
  connectionStatus: document.getElementById('connectionStatus'),
  lastUpdated: document.getElementById('lastUpdated'),
  statDaemon: document.getElementById('statDaemon'),
  statDaemonDetail: document.getElementById('statDaemonDetail'),
  statQueue: document.getElementById('statQueue'),
  statQueueDetail: document.getElementById('statQueueDetail'),
  statDocs: document.getElementById('statDocs'),
  statDocsDetail: document.getElementById('statDocsDetail'),
  healthHooks: document.getElementById('healthHooks'),
  healthHooksDetail: document.getElementById('healthHooksDetail'),
  healthQdrant: document.getElementById('healthQdrant'),
  healthQdrantDetail: document.getElementById('healthQdrantDetail'),
  healthMcp: document.getElementById('healthMcp'),
  healthMcpDetail: document.getElementById('healthMcpDetail'),
  reinstallHooksBtn: document.getElementById('reinstallHooksBtn'),
  hooksInstallLog: document.getElementById('hooksInstallLog'),
  settingsForm: document.getElementById('settingsForm'),
  devRootInput: document.getElementById('devRootInput'),
  scanDepthInput: document.getElementById('scanDepthInput'),
  scanBtn: document.getElementById('scanBtn'),
  settingsMsg: document.getElementById('settingsMsg'),
  candidatesTable: document.getElementById('candidatesTable'),
  candidatesBody: document.getElementById('candidatesBody'),
  candidatesEmpty: document.getElementById('candidatesEmpty'),
  candidatesMeta: document.getElementById('candidatesMeta'),
  registeredTable: document.getElementById('registeredTable'),
  registeredBody: document.getElementById('registeredBody'),
  registeredEmpty: document.getElementById('registeredEmpty'),
  registeredMeta: document.getElementById('registeredMeta'),
  debugRaw: document.getElementById('debugRaw'),
  toast: document.getElementById('toast'),
  globalIgnoreText: document.getElementById('globalIgnoreText'),
  saveIgnoreBtn: document.getElementById('saveIgnoreBtn'),
  reloadIgnoreBtn: document.getElementById('reloadIgnoreBtn'),
  ignoreMsg: document.getElementById('ignoreMsg'),
  largestFilesTable: document.getElementById('largestFilesTable'),
  largestFilesBody: document.getElementById('largestFilesBody'),
  largestFilesEmpty: document.getElementById('largestFilesEmpty'),
  largestFilesMeta: document.getElementById('largestFilesMeta'),
  largestFilesSkippedOnly: document.getElementById('largestFilesSkippedOnly'),
  reloadLargestFilesBtn: document.getElementById('reloadLargestFilesBtn'),
  reloadBranchCoverageBtn: document.getElementById('reloadBranchCoverageBtn'),
  branchCoverageMeta: document.getElementById('branchCoverageMeta'),
  branchCoverageEmpty: document.getElementById('branchCoverageEmpty'),
  branchCoverageTable: document.getElementById('branchCoverageTable'),
  branchCoverageBody: document.getElementById('branchCoverageBody'),
  showClaudeConfigBtn: document.getElementById('showClaudeConfigBtn'),
  showCodexConfigBtn: document.getElementById('showCodexConfigBtn'),
  configHint: document.getElementById('configHint'),
  configDisplay: document.getElementById('configDisplay'),
  configLabel: document.getElementById('configLabel'),
  configPre: document.getElementById('configPre'),
  copyConfigBtn: document.getElementById('copyConfigBtn'),
  copyMsg: document.getElementById('copyMsg'),
  logLinesSelect: document.getElementById('logLinesSelect'),
  loadMcpLogsBtn: document.getElementById('loadMcpLogsBtn'),
  clearLogsBtn: document.getElementById('clearLogsBtn'),
  logsEmpty: document.getElementById('logsEmpty'),
  logsMeta: document.getElementById('logsMeta'),
  logsTable: document.getElementById('logsTable'),
  logsBody: document.getElementById('logsBody'),
  checkDaemonMetricsBtn: document.getElementById('checkDaemonMetricsBtn'),
  daemonMetricsVal: document.getElementById('daemonMetricsVal'),
  daemonMetricsDetail: document.getElementById('daemonMetricsDetail'),
  forceReconcileBtn: document.getElementById('forceReconcileBtn'),
  stackActionsStatus: document.getElementById('stackActionsStatus'),
  stackActionsLog: document.getElementById('stackActionsLog'),
  refreshHealthBtn: document.getElementById('refreshHealthBtn'),
  adminPidVal: document.getElementById('adminPidVal'),
  rulesScopeSelect: document.getElementById('rulesScopeSelect'),
  rulesProjectField: document.getElementById('rulesProjectField'),
  rulesProjectSelect: document.getElementById('rulesProjectSelect'),
  reloadRulesBtn: document.getElementById('reloadRulesBtn'),
  rulesMeta: document.getElementById('rulesMeta'),
  rulesEmpty: document.getElementById('rulesEmpty'),
  rulesTable: document.getElementById('rulesTable'),
  rulesBody: document.getElementById('rulesBody'),
  ruleForm: document.getElementById('ruleForm'),
  ruleFormTitle: document.getElementById('ruleFormTitle'),
  ruleLabelInput: document.getElementById('ruleLabelInput'),
  ruleTitleInput: document.getElementById('ruleTitleInput'),
  rulePriorityInput: document.getElementById('rulePriorityInput'),
  ruleContentInput: document.getElementById('ruleContentInput'),
  ruleSubmitBtn: document.getElementById('ruleSubmitBtn'),
  ruleCancelEditBtn: document.getElementById('ruleCancelEditBtn'),
  ruleFormMsg: document.getElementById('ruleFormMsg'),
  reloadFailedBtn: document.getElementById('reloadFailedBtn'),
  retryAllFailedBtn: document.getElementById('retryAllFailedBtn'),
  cancelPendingBtn: document.getElementById('cancelPendingBtn'),
  failedMeta: document.getElementById('failedMeta'),
  failedEmpty: document.getElementById('failedEmpty'),
  failedTable: document.getElementById('failedTable'),
  failedBody: document.getElementById('failedBody'),
};

let token = sessionStorage.getItem(TOKEN_KEY) || '';
let pollTimer = null;
let lastCandidates = [];
let lastRegisteredPaths = new Set();
let lastRegistered = [];
let rulesEditingLabel = null;

// ── Networking ──────────────────────────────────────────────────────

async function api(path, opts = {}) {
  const init = {
    method: opts.method || 'GET',
    headers: {
      'Accept': 'application/json',
      'Authorization': `Bearer ${token}`,
      ...(opts.body ? { 'Content-Type': 'application/json' } : {}),
      ...(opts.headers || {}),
    },
  };
  if (opts.body) init.body = JSON.stringify(opts.body);
  const res = await fetch(path, init);
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* ignore */ }
  if (!res.ok) {
    const detail = json?.detail || json?.error || text || res.statusText;
    const err = new Error(`HTTP ${res.status}: ${detail}`);
    err.status = res.status;
    throw err;
  }
  return json;
}

// ── Toast ──────────────────────────────────────────────────────────

let toastTimer = null;
function toast(msg, kind = 'ok') {
  els.toast.textContent = msg;
  const variant = kind === 'error' ? ' error' : kind === 'info' ? ' info' : '';
  els.toast.className = `toast${variant}`;
  // Errors interrupt; routine confirmations wait their turn for SR users.
  els.toast.setAttribute('aria-live', kind === 'error' ? 'assertive' : 'polite');
  els.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { els.toast.hidden = true; }, 3200);
}

// ── Render ─────────────────────────────────────────────────────────

function pill(text, kind) {
  return `<span class="pill pill-${kind}">${escapeHtml(text)}</span>`;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[c]);
}

function fmtTime(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  const sec = Math.floor((Date.now() - d.getTime()) / 1000);
  if (sec < 60) return `${sec}s ago`;
  if (sec < 3600) return `${Math.floor(sec/60)}m ago`;
  if (sec < 86400) return `${Math.floor(sec/3600)}h ago`;
  return d.toISOString().slice(0, 16).replace('T', ' ');
}

function renderHealth(snap) {
  const d = snap.daemon || {};
  if (d.ok) {
    els.statDaemon.innerHTML = pill('healthy', 'ok');
    els.statDaemonDetail.textContent =
      `${(d.activeProjects || []).length} active · ${d.totalCollections || 0} collections`;
  } else {
    els.statDaemon.innerHTML = pill('unhealthy', 'err');
    els.statDaemonDetail.textContent = d.reason || 'unknown';
  }

  const q = snap.queue || {};
  els.statQueue.textContent = q.pending ?? 0;
  els.statQueueDetail.textContent =
    `${q.pending || 0} pending · ${q.in_progress || 0} in-progress · ${q.failed || 0} failed`;

  els.statDocs.textContent = (d.totalDocuments ?? 0).toLocaleString();
  els.statDocsDetail.textContent =
    `${d.totalCollections || 0} collections · ${snap.projects?.registeredCount || 0} watch folders`;
}

function renderSettings(snap) {
  if (document.activeElement !== els.devRootInput) {
    els.devRootInput.value = snap.settings?.devRoot || '';
  }
  if (document.activeElement !== els.scanDepthInput) {
    els.scanDepthInput.value = snap.settings?.scanDepth || 1;
  }
}

function renderCandidates() {
  const registered = lastRegisteredPaths;
  const cands = lastCandidates.filter((c) => !registered.has(c.path));
  els.candidatesMeta.textContent = `${cands.length} candidate(s)`;
  if (cands.length === 0) {
    els.candidatesTable.hidden = true;
    els.candidatesEmpty.hidden = false;
    return;
  }
  els.candidatesTable.hidden = false;
  els.candidatesEmpty.hidden = true;
  els.candidatesBody.innerHTML = cands.map((c) => `
    <tr>
      <td><span class="path">${escapeHtml(c.path)}</span>
          <span class="sub">${escapeHtml(c.name)}</span></td>
      <td>${escapeHtml(c.branch || '—')}</td>
      <td><span class="path">${escapeHtml(c.remoteUrl || '—')}</span></td>
      <td class="num">${c.depth}</td>
      <td>${c.isWorktree ? pill('worktree', 'warn') : pill('repo', 'muted')}</td>
      <td>
        <button class="primary small" data-action="register" data-path="${escapeHtml(c.path)}">Register</button>
      </td>
    </tr>
  `).join('');
}

/** Render an ETA in seconds as a coarse human-readable string ("3s",
 *  "12m", "2h 14m"). The daemon's rate-window resolution is 5 minutes,
 *  so any extra precision would be theatre. */
function formatEta(seconds) {
  const s = Number(seconds);
  if (!Number.isFinite(s) || s < 0) return 'unknown';
  if (s < 60) return `${Math.round(s)}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  const rem = m % 60;
  return rem === 0 ? `${h}h` : `${h}h ${rem}m`;
}

/**
 * Render the indexing-progress cell for one registered project.
 * Returns a small HTML snippet showing a progress bar + counts + ETA.
 * When the daemon couldn't report (`indexing == null`), shows a dim "—".
 */
function renderIndexingCell(indexing) {
  if (!indexing) return '<span class="dim small">—</span>';
  const pct = Math.max(0, Math.min(100, Number(indexing.percent ?? 0)));
  const inFlight = (indexing.pending ?? 0) + (indexing.in_progress ?? 0);
  const failed = indexing.failed ?? 0;
  const done = indexing.done ?? 0;
  const total = indexing.total ?? 0;
  // Three states: idle (queue drained), active (in-flight > 0), failed-some.
  let state = 'idle';
  if (inFlight > 0) state = 'active';
  else if (failed > 0) state = 'warn';
  const labelMain =
    inFlight > 0
      ? `${inFlight} in flight · ${done}/${total}`
      : `${done} indexed`;
  const labelFailed = failed > 0 ? ` · <span class="warn">${failed} failed</span>` : '';
  // ETA only renders when the queue is still draining — once idle the
  // value is uninformative ("0s") and noisy. "Warming up" reflects the
  // daemon's cold-start window.
  let etaLine = '';
  if (inFlight > 0) {
    const etaText =
      typeof indexing.eta_seconds === 'number'
        ? `ETA ~${formatEta(indexing.eta_seconds)}`
        : 'ETA — warming up';
    etaLine = `<div class="indexing-eta dim small">${escapeHtml(etaText)}</div>`;
  }
  return `
    <div class="indexing-cell">
      <div class="indexing-bar indexing-bar-${state}" role="progressbar"
           aria-valuenow="${pct.toFixed(1)}" aria-valuemin="0" aria-valuemax="100">
        <div class="indexing-bar-fill" style="width:${pct.toFixed(1)}%"></div>
      </div>
      <div class="indexing-meta dim small">
        ${escapeHtml(labelMain)}${labelFailed} · ${pct.toFixed(1)}%
      </div>
      ${etaLine}
    </div>
  `;
}

function renderRegistered(snap) {
  const registered = snap.projects?.registered || [];
  lastRegisteredPaths = new Set(registered.map((r) => r.path));
  lastRegistered = registered;
  populateRulesProjects();
  populatePlaygroundProjects();
  els.registeredMeta.textContent = `${registered.length} registered`;
  if (registered.length === 0) {
    els.registeredTable.hidden = true;
    els.registeredEmpty.hidden = false;
    return;
  }
  els.registeredTable.hidden = false;
  els.registeredEmpty.hidden = true;
  els.registeredBody.innerHTML = registered.map((r) => `
    <tr>
      <td><span class="path">${escapeHtml(r.path)}</span></td>
      <td><code>${escapeHtml(r.tenantId)}</code></td>
      <td>${r.isPaused ? pill('paused', 'warn') : r.isActive ? pill('active', 'ok') : pill('idle', 'muted')}</td>
      <td>${renderIndexingCell(r.indexing)}</td>
      <td class="dim">${escapeHtml(fmtTime(r.lastActivityAt))}</td>
      <td class="nowrap">
        ${r.isPaused
          ? `<button class="secondary small"
                data-action="watch-resume"
                data-watch-id="${escapeHtml(r.path)}"
                title="Set is_paused=0 for this watch folder">Resume</button>`
          : `<button class="secondary small"
                data-action="watch-pause"
                data-watch-id="${escapeHtml(r.path)}"
                title="Set is_paused=1 for this watch folder">Pause</button>`}
        <button class="secondary small"
                data-action="project-reindex"
                data-id="${escapeHtml(r.tenantId)}"
                title="Rebuild FTS5/tags/sparse/components for this project (no re-embed)">Reindex</button>
        <button class="secondary small"
                data-action="project-reembed"
                data-id="${escapeHtml(r.tenantId)}"
                title="Re-read & re-embed all of this project's files (regenerates vectors; runs in the queue)">Re-embed</button>
        <button class="danger small"
                data-action="deregister"
                data-id="${escapeHtml(r.tenantId)}"
                data-path="${escapeHtml(r.path)}">Deactivate</button>
      </td>
    </tr>
  `).join('');
}

function renderDebug(snap) {
  els.debugRaw.textContent = JSON.stringify(snap, null, 2);
}

function renderConnection(ok, err) {
  if (ok) {
    els.connectionStatus.className = 'pill pill-ok';
    els.connectionStatus.textContent = 'online';
  } else {
    els.connectionStatus.className = 'pill pill-err';
    els.connectionStatus.textContent = err || 'offline';
  }
  els.lastUpdated.textContent = `updated ${new Date().toLocaleTimeString()}`;
}

// ── Polling ────────────────────────────────────────────────────────

async function refresh() {
  try {
    const snap = await api('/admin/api/snapshot');
    renderHealth(snap);
    renderSettings(snap);
    renderRegistered(snap);
    renderCandidates();
    renderDebug(snap);
    renderConnection(true);
    loadBranchCoverage();
  } catch (e) {
    renderConnection(false, e.status === 401 ? 'auth failed' : 'offline');
    if (e.status === 401) {
      logout(e.message);
    }
  }
  // Host integrations health — refreshed on the same cadence but tolerant
  // of failures (the snapshot can still succeed even if /health hiccups).
  try {
    const health = await api('/admin/api/health');
    renderHostHealth(health);
    if (health.mcp) {
      els.adminPidVal.textContent = `pid ${health.mcp.pid ?? '—'} · up ${formatUptime(health.mcp.uptimeSeconds ?? 0)}`;
    }
  } catch {
    // Leave previous values in place; the snapshot path already drives
    // the "connecting…" pill so we don't double-flag.
  }
}

function renderHostHealth(h) {
  if (!h) return;
  const hooks = h.hooks || {};
  if (hooks.kind === 'posix' && hooks.ok) {
    els.healthHooks.innerHTML = pill('POSIX · OK', 'ok');
  } else if (hooks.kind === 'powershell') {
    els.healthHooks.innerHTML = pill('PowerShell (legacy)', 'warn');
  } else if (hooks.kind === 'mixed') {
    els.healthHooks.innerHTML = pill('Mixed PS+POSIX', 'warn');
  } else if (hooks.kind === 'posix' && !hooks.ok) {
    els.healthHooks.innerHTML = pill('POSIX · incomplete', 'warn');
  } else {
    els.healthHooks.innerHTML = pill('not installed', 'err');
  }
  const installedTxt = (hooks.installed || []).length + ' installed';
  const legacyTxt = (hooks.legacyArtifacts || []).length
    ? ` · ${(hooks.legacyArtifacts || []).length} legacy artifact(s)`
    : '';
  els.healthHooksDetail.textContent = `${installedTxt}${legacyTxt}`;

  const qdrant = h.qdrant || {};
  els.healthQdrant.innerHTML = qdrant.ok
    ? pill('reachable', 'ok')
    : pill('offline', 'err');
  els.healthQdrantDetail.textContent = qdrant.endpoint || qdrant.reason || '—';

  const mcp = h.mcp || {};
  els.healthMcp.innerHTML = pill(mcp.version || 'running', 'ok');
  const uptime = mcp.uptimeSeconds ? `${formatUptime(mcp.uptimeSeconds)} uptime` : '—';
  els.healthMcpDetail.textContent = `${mcp.mode || 'http'} · pid ${mcp.pid} · ${uptime}`;
}

function formatUptime(seconds) {
  if (seconds < 60) return `${seconds}s`;
  const mins = Math.floor(seconds / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  return `${hours}h${mins % 60}m`;
}

function startPolling() {
  if (pollTimer) clearInterval(pollTimer);
  refresh();
  pollTimer = setInterval(refresh, REFRESH_MS);
}

function stopPolling() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

// ── Auth ───────────────────────────────────────────────────────────

function showLogin() {
  els.loginPanel.hidden = false;
  els.appView.hidden = true;
  els.tokenInput.value = '';
  setTimeout(() => els.tokenInput.focus(), 50);
}

function showApp() {
  els.loginPanel.hidden = true;
  initTabs(); // tag sections + activate a tab while appView is still hidden (no flash)
  els.appView.hidden = false;
  initPlayground();
  loadGlobalIgnore();
  loadLargestFiles();
  loadBranchCoverage();
  loadRules();
  loadFailed();
}

function logout(reason) {
  stopPolling();
  sessionStorage.removeItem(TOKEN_KEY);
  token = '';
  els.loginError.textContent = reason || '';
  els.loginError.hidden = !reason;
  showLogin();
}

els.loginForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  token = els.tokenInput.value.trim();
  if (!token) return;
  try {
    await api('/admin/api/snapshot');
    sessionStorage.setItem(TOKEN_KEY, token);
    els.loginError.hidden = true;
    showApp();
    startPolling();
  } catch (e) {
    els.loginError.textContent = e.message || 'authentication failed';
    els.loginError.hidden = false;
  }
});

// ── Actions ────────────────────────────────────────────────────────

els.settingsForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  try {
    const next = await api('/admin/api/settings', {
      method: 'PUT',
      body: {
        devRoot: els.devRootInput.value.trim(),
        scanDepth: Number(els.scanDepthInput.value) || 1,
      },
    });
    els.settingsMsg.textContent = `Saved at ${new Date().toLocaleTimeString()}`;
    toast('Settings saved');
    refresh();
  } catch (e) {
    toast(e.message, 'error');
  }
});

els.reinstallHooksBtn.addEventListener('click', async () => {
  els.reinstallHooksBtn.disabled = true;
  const originalLabel = els.reinstallHooksBtn.textContent;
  els.reinstallHooksBtn.textContent = 'Installing…';
  els.hooksInstallLog.hidden = true;
  try {
    const result = await api('/admin/api/hooks/install', {
      method: 'POST',
      body: { force: true },
    });
    const lines = [
      `exitCode: ${result.exitCode}`,
      result.stdout ? `--- stdout ---\n${result.stdout}` : '',
      result.stderr ? `--- stderr ---\n${result.stderr}` : '',
    ].filter(Boolean);
    els.hooksInstallLog.textContent = lines.join('\n');
    els.hooksInstallLog.hidden = false;
    if (result.ok) {
      toast('Hooks reinstalled');
    } else {
      toast(`Install failed (exit ${result.exitCode})`, 'error');
    }
    refresh();
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.reinstallHooksBtn.disabled = false;
    els.reinstallHooksBtn.textContent = originalLabel;
  }
});

els.scanBtn.addEventListener('click', async () => {
  els.scanBtn.disabled = true;
  els.scanBtn.textContent = 'Scanning…';
  try {
    const result = await api('/admin/api/projects/scan', {
      method: 'POST',
      body: {
        devRoot: els.devRootInput.value.trim(),
        scanDepth: Number(els.scanDepthInput.value) || 1,
      },
    });
    lastCandidates = result.scan?.candidates || [];
    renderCandidates();
    toast(`Found ${lastCandidates.length} candidate(s) in ${result.scan?.visited || 0} dirs`);
    refresh();
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.scanBtn.disabled = false;
    els.scanBtn.textContent = 'Scan now';
  }
});

document.addEventListener('click', async (e) => {
  const btn = e.target.closest('button[data-action]');
  if (!btn) return;
  const action = btn.dataset.action;
  btn.disabled = true;
  try {
    if (action === 'register') {
      await api('/admin/api/projects/register', {
        method: 'POST',
        body: { path: btn.dataset.path },
      });
      toast(`Registered ${btn.dataset.path}`);
      refresh();
    } else if (action === 'deregister') {
      await api('/admin/api/projects/deregister', {
        method: 'POST',
        body: { projectId: btn.dataset.id, path: btn.dataset.path },
      });
      toast(`Deactivated ${btn.dataset.id}`);
      refresh();
    } else if (action === 'watch-pause') {
      const result = await api('/admin/api/watches/pause', {
        method: 'POST',
        body: { watchId: btn.dataset.watchId },
      });
      if (result.affectedCount > 0) {
        toast(`Paused watch: ${btn.dataset.watchId}`);
      } else {
        toast(`No change (already paused, disabled, or not found)`, 'info');
      }
      refresh();
    } else if (action === 'watch-resume') {
      const result = await api('/admin/api/watches/resume', {
        method: 'POST',
        body: { watchId: btn.dataset.watchId },
      });
      if (result.affectedCount > 0) {
        toast(`Resumed watch: ${btn.dataset.watchId}`);
      } else {
        toast(`No change (not currently paused, disabled, or not found)`, 'info');
      }
      refresh();
    } else if (action === 'project-reindex') {
      const result = await api('/admin/api/projects/reindex', {
        method: 'POST',
        body: { tenantId: btn.dataset.id },
      });
      if (result.ok) {
        const ms = result.durationMs ? ` (${result.durationMs}ms)` : '';
        toast(`Reindexed project ${btn.dataset.id}${ms}`);
      } else {
        toast(`Reindex reported failure: ${result.message || 'unknown'}`, 'error');
      }
      refresh();
    } else if (action === 'project-reembed') {
      if (!confirm(`Re-embed all files for project ${btn.dataset.id}? This force-re-reads, re-chunks and re-embeds the WHOLE project in the background (no unchanged-file skip) — a full embedding pass.`)) {
        return;
      }
      // force: the button promises the whole project; without it the daemon
      // skips files whose hash + chunker fingerprint are unchanged (the
      // repair mode the bulk reindex API uses).
      const result = await api('/admin/api/projects/reembed', {
        method: 'POST',
        body: { tenantId: btn.dataset.id, force: true },
      });
      toast(`Forced re-embed queued for ${btn.dataset.id}: ${result.filesEnqueued ?? 0} folder scan(s)`);
      refresh();
    } else if (action === 'rule-edit') {
      startRuleEdit(btn.dataset);
    } else if (action === 'rule-delete') {
      const scope = currentRuleScope();
      if (!confirm(`Delete rule "${btn.dataset.label}" from ${scope} scope?`)) return;
      const body = { label: btn.dataset.label, scope };
      if (scope === 'project') body.projectId = currentRuleProjectId();
      await api('/admin/api/rules', { method: 'DELETE', body });
      toast(`Deleted rule ${btn.dataset.label}`);
      if (rulesEditingLabel === btn.dataset.label) resetRuleForm();
      loadRules();
    } else if (action === 'queue-retry-item') {
      const r = await api('/admin/api/queue/retry', {
        method: 'POST',
        body: { queueId: btn.dataset.id },
      });
      if (r.found && r.reset) toast(`Requeued ${String(btn.dataset.id).slice(0, 8)}…`);
      else if (r.found) toast('Item is no longer in failed state', 'info');
      else toast('Item not found', 'error');
      loadFailed();
      refresh();
    }
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    btn.disabled = false;
  }
});

// ── Global ignore rules ────────────────────────────────────────────

async function loadGlobalIgnore() {
  try {
    const data = await api('/admin/api/ignore/global');
    // Don't overwrite if the user is actively editing
    if (document.activeElement !== els.globalIgnoreText) {
      els.globalIgnoreText.value = data.content || '';
    }
    els.ignoreMsg.textContent = `Loaded · ${data.path || ''}`;
  } catch (e) {
    els.ignoreMsg.textContent = `Load failed: ${e.message}`;
  }
}

els.saveIgnoreBtn.addEventListener('click', async () => {
  els.saveIgnoreBtn.disabled = true;
  const originalLabel = els.saveIgnoreBtn.textContent;
  els.saveIgnoreBtn.textContent = 'Saving…';
  try {
    const result = await api('/admin/api/ignore/global', {
      method: 'PUT',
      body: { content: els.globalIgnoreText.value },
    });
    els.ignoreMsg.textContent = `Saved ${result.bytes} bytes · ${new Date().toLocaleTimeString()}`;
    toast('Global ignore rules saved');
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.saveIgnoreBtn.disabled = false;
    els.saveIgnoreBtn.textContent = originalLabel;
  }
});

els.reloadIgnoreBtn.addEventListener('click', () => loadGlobalIgnore());

// ── Behavioral rules ───────────────────────────────────────────────

function currentRuleScope() {
  return els.rulesScopeSelect?.value === 'project' ? 'project' : 'global';
}

function currentRuleProjectId() {
  return els.rulesProjectSelect?.value || '';
}

/** Repopulate the project picker from the latest registered-projects list,
 *  preserving the current selection when it's still present. */
function populateRulesProjects() {
  const sel = els.rulesProjectSelect;
  if (!sel) return;
  const prev = sel.value;
  if (lastRegistered.length === 0) {
    sel.innerHTML = '<option value="">(no registered projects)</option>';
    return;
  }
  sel.innerHTML = lastRegistered
    .map((r) => `<option value="${escapeHtml(r.tenantId)}">${escapeHtml(r.path)} (${escapeHtml(r.tenantId)})</option>`)
    .join('');
  if (prev && lastRegistered.some((r) => r.tenantId === prev)) sel.value = prev;
}

function renderRules(rules) {
  if (!rules || rules.length === 0) {
    els.rulesEmpty.textContent = 'No rules in this scope yet.';
    els.rulesEmpty.hidden = false;
    els.rulesTable.hidden = true;
    return;
  }
  els.rulesEmpty.hidden = true;
  els.rulesTable.hidden = false;
  els.rulesBody.innerHTML = rules.map((r) => {
    const label = r.label || '';
    const content = r.content || '';
    const short = content.length > 120 ? content.slice(0, 118) + '…' : content;
    return `<tr>
      <td><code>${escapeHtml(label)}</code></td>
      <td>${escapeHtml(r.title || '—')}</td>
      <td class="num">${r.priority ?? '—'}</td>
      <td><span title="${escapeHtml(content)}">${escapeHtml(short)}</span></td>
      <td class="nowrap">
        <button class="secondary small" data-action="rule-edit"
                data-label="${escapeHtml(label)}"
                data-title="${escapeHtml(r.title || '')}"
                data-priority="${r.priority ?? ''}"
                data-content="${escapeHtml(content)}"
                ${label ? '' : 'disabled title="rule has no label — cannot edit"'}>Edit</button>
        <button class="danger small" data-action="rule-delete"
                data-label="${escapeHtml(label)}"
                ${label ? '' : 'disabled title="rule has no label — cannot delete"'}>Delete</button>
      </td>
    </tr>`;
  }).join('');
}

async function loadRules() {
  if (!els.rulesTable) return;
  const scope = currentRuleScope();
  let qs = `scope=${scope}`;
  if (scope === 'project') {
    const pid = currentRuleProjectId();
    if (!pid) {
      els.rulesEmpty.textContent = 'Select a registered project to view its rules.';
      els.rulesEmpty.hidden = false;
      els.rulesTable.hidden = true;
      els.rulesMeta.textContent = '';
      return;
    }
    qs += `&projectId=${encodeURIComponent(pid)}`;
    els.rulesTable.setAttribute('aria-label', `Project behavioral rules for ${currentRuleProjectId()}`);
  } else {
    els.rulesTable.setAttribute('aria-label', 'Global behavioral rules');
  }
  try {
    const data = await api(`/admin/api/rules?${qs}`);
    renderRules(data.rules || []);
    els.rulesMeta.textContent = data.message || '';
  } catch (e) {
    els.rulesEmpty.textContent = `Failed to load: ${e.message}`;
    els.rulesEmpty.hidden = false;
    els.rulesTable.hidden = true;
    els.rulesMeta.textContent = '';
  }
}

function startRuleEdit(ds) {
  rulesEditingLabel = ds.label;
  els.ruleLabelInput.value = ds.label || '';
  els.ruleLabelInput.readOnly = true;
  els.ruleTitleInput.value = ds.title || '';
  els.rulePriorityInput.value = ds.priority || '';
  els.ruleContentInput.value = ds.content || '';
  els.ruleFormTitle.textContent = `Edit rule: ${ds.label}`;
  els.ruleSubmitBtn.textContent = 'Update rule';
  els.ruleCancelEditBtn.hidden = false;
  els.ruleFormMsg.textContent = '';
  els.ruleContentInput.focus();
}

function resetRuleForm() {
  rulesEditingLabel = null;
  els.ruleForm.reset();
  els.ruleLabelInput.readOnly = false;
  els.ruleFormTitle.textContent = 'Add rule';
  els.ruleSubmitBtn.textContent = 'Add rule';
  els.ruleCancelEditBtn.hidden = true;
  els.ruleFormMsg.textContent = '';
  els.ruleFormMsg.className = 'dim small';
}

function onRulesScopeChange() {
  const isProject = currentRuleScope() === 'project';
  els.rulesProjectField.hidden = !isProject;
  resetRuleForm();
  loadRules();
}

els.rulesScopeSelect?.addEventListener('change', onRulesScopeChange);
els.rulesProjectSelect?.addEventListener('change', () => loadRules());
els.reloadRulesBtn?.addEventListener('click', () => loadRules());
els.ruleCancelEditBtn?.addEventListener('click', () => resetRuleForm());

els.ruleForm?.addEventListener('submit', async (e) => {
  e.preventDefault();
  const scope = currentRuleScope();
  const body = {
    label: els.ruleLabelInput.value.trim(),
    title: els.ruleTitleInput.value.trim(),
    content: els.ruleContentInput.value.trim(),
    scope,
  };
  const pr = els.rulePriorityInput.value.trim();
  if (pr !== '') body.priority = Number(pr);
  if (scope === 'project') {
    body.projectId = currentRuleProjectId();
    if (!body.projectId) { toast('Select a registered project first', 'error'); return; }
  }
  if (!body.label || !body.content) { toast('Label and content are required', 'error'); return; }

  const editing = !!rulesEditingLabel;
  els.ruleSubmitBtn.disabled = true;
  try {
    const result = await api('/admin/api/rules', { method: editing ? 'PUT' : 'POST', body });
    toast(editing ? `Updated rule ${body.label}` : `Added rule ${body.label}`);
    resetRuleForm();
    loadRules();
  } catch (e) {
    // The add path returns 409 with the duplicate message; show it inline too.
    els.ruleFormMsg.textContent = e.message;
    els.ruleFormMsg.className = 'error small';
    toast(e.message, 'error');
  } finally {
    els.ruleSubmitBtn.disabled = false;
  }
});

// ── Failed indexing items (unified_queue, status='failed') ─────────

function renderFailed(items, totalFailed) {
  if (!items || items.length === 0) {
    els.failedEmpty.textContent = 'No failed items.';
    els.failedEmpty.hidden = false;
    els.failedTable.hidden = true;
    els.failedMeta.textContent = '';
    return;
  }
  els.failedEmpty.hidden = true;
  els.failedTable.hidden = false;
  els.failedBody.innerHTML = items.map((it) => {
    // Prefer the file path; fall back to a collection/type/op descriptor for
    // non-file items (rules, library ingests, etc).
    const label = it.file_path && it.file_path.length
      ? it.file_path
      : `${it.collection || '?'} · ${it.item_type || '?'} · ${it.op || '?'}`;
    const shortLabel = label.length > 64 ? '…' + label.slice(-62) : label;
    const err = it.error_message || '';
    const shortErr = err.length > 90 ? err.slice(0, 88) + '…' : err;
    return `<tr>
      <td><code title="${escapeHtml(label)}">${escapeHtml(shortLabel)}</code>
          <span class="sub">${escapeHtml(it.branch || '')}</span></td>
      <td><span class="dim small">${escapeHtml(it.tenant_id || '')}</span></td>
      <td><span class="warn" title="${escapeHtml(err)}">${escapeHtml(shortErr || '—')}</span></td>
      <td class="num">${it.retry_count ?? 0}</td>
      <td class="dim small">${escapeHtml(fmtTime(it.last_error_at || it.updated_at))}</td>
      <td><button class="secondary small" data-action="queue-retry-item"
                  data-id="${escapeHtml(it.queue_id)}">Retry</button></td>
    </tr>`;
  }).join('');
  const shown = items.length;
  els.failedMeta.textContent =
    totalFailed > shown ? `· showing ${shown} of ${totalFailed}` : `· ${shown} item(s)`;
}

async function loadFailed() {
  if (!els.failedTable) return;
  try {
    const data = await api('/admin/api/queue/failed?limit=100');
    renderFailed(data.items || [], data.totalFailed ?? 0);
  } catch (e) {
    els.failedEmpty.textContent = `Failed to load: ${e.message}`;
    els.failedEmpty.hidden = false;
    els.failedTable.hidden = true;
    els.failedMeta.textContent = '';
  }
}

els.reloadFailedBtn?.addEventListener('click', () => loadFailed());

els.retryAllFailedBtn?.addEventListener('click', async () => {
  if (!confirm('Retry ALL failed items? They will be reset to pending and reprocessed.')) return;
  els.retryAllFailedBtn.disabled = true;
  try {
    const r = await api('/admin/api/queue/retry', { method: 'POST', body: {} });
    toast(`Requeued ${r.resetCount ?? 0} failed item(s)`);
    loadFailed();
    refresh();
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.retryAllFailedBtn.disabled = false;
  }
});

els.cancelPendingBtn?.addEventListener('click', async () => {
  const tenantId = (
    prompt('Cancel PENDING queue items for which tenant?\n\nEnter the tenant_id (e.g. 367157a01d98):') || ''
  ).trim();
  if (!tenantId) return;
  els.cancelPendingBtn.disabled = true;
  try {
    // Dry-run first to preview the blast radius before the destructive confirm.
    const dry = await api('/admin/api/queue/cancel', {
      method: 'POST',
      body: { tenantId, statuses: ['pending'], dryRun: true },
    });
    if (!dry.count) {
      toast(`No pending items to cancel for ${tenantId}`);
      return;
    }
    const ok = confirm(
      `Cancel ${dry.count} PENDING item(s) for ${dry.projectPath || tenantId}?\n\n` +
        'This has NO item_type filter — File items are removed too, so a reembed ' +
        'may be needed afterward to re-ingest them. in_progress items are never ' +
        'cancelled. This cannot be undone.'
    );
    if (!ok) return;
    const r = await api('/admin/api/queue/cancel', {
      method: 'POST',
      body: { tenantId, statuses: ['pending'], dryRun: false },
    });
    toast(`Cancelled ${r.count ?? 0} pending item(s) for ${tenantId}`);
    loadFailed();
    refresh();
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.cancelPendingBtn.disabled = false;
  }
});

// ── Branch coverage / consistency ─────────────────────────────────

function summarizeQueue(queue) {
  if (!queue) return '0';
  const pending = queue.pending || 0;
  const active = queue.in_progress || 0;
  const failed = queue.failed || 0;
  const parts = [];
  if (pending) parts.push(`${pending.toLocaleString()} pending`);
  if (active) parts.push(`${active.toLocaleString()} active`);
  if (failed) parts.push(`${failed.toLocaleString()} failed`);
  return parts.length ? parts.join(' · ') : '0';
}

// Collapsed branch-coverage groups, keyed by tenant id. Kept in a module var
// (not the DOM) so the 5s poll re-render preserves each group's open/closed
// state — the render bakes `hidden` into the branch rows from this set.
const bcCollapsed = new Set();

function renderBranchCoverage(data) {
  if (!els.branchCoverageTable) return;
  if (!data?.ok) {
    els.branchCoverageEmpty.textContent =
      data?.degraded?.message || 'Branch coverage unavailable.';
    els.branchCoverageEmpty.hidden = false;
    els.branchCoverageTable.hidden = true;
    els.branchCoverageMeta.textContent = '';
    return;
  }

  const projects = data.projects || [];
  let branchRowCount = 0;
  const html = [];

  projects.forEach((project, i) => {
    const key = project.tenantId || `p${i}`;
    const branches = (project.branches || []).length ? project.branches : [{ branch: '(none)' }];
    branchRowCount += branches.length;
    const collapsed = bcCollapsed.has(key);

    const projectLabel = project.path || project.tenantId || 'unknown';
    const shortProject = projectLabel.length > 72 ? '…' + projectLabel.slice(-70) : projectLabel;
    const current = project.currentBranch || '—';
    const warnings = project.warnings || [];
    const offBranch = branches.filter((b) => {
      const n = b.branch || '(none)';
      return project.currentBranch && n !== project.currentBranch && n !== '(none)';
    }).length;

    // ── Project group header (colspan across all 5 columns) ──
    const meta =
      `· current <code>${escapeHtml(current)}</code> · ${branches.length} branch${branches.length !== 1 ? 'es' : ''}` +
      (offBranch ? ` · ${offBranch} off-branch` : '');
    const warnPill = warnings.length
      ? `<span title="${escapeHtml(warnings.join(' · '))}">${pill(`${warnings.length} warning${warnings.length > 1 ? 's' : ''}`, 'warn')}</span>`
      : '';
    html.push(
      `<tr class="bc-group${warnings.length ? ' bc-group-warn' : ''}" data-bc-key="${escapeHtml(key)}" role="button" aria-expanded="${collapsed ? 'false' : 'true'}">
        <td colspan="5">
          <span class="bc-caret">${collapsed ? '▸' : '▾'}</span>
          <span class="path" title="${escapeHtml(projectLabel)}">${escapeHtml(shortProject)}</span>
          <code class="bc-tenant">${escapeHtml(project.tenantId || '')}</code>
          <span class="dim small">${meta}</span>
          ${warnPill}
        </td>
      </tr>`
    );

    // ── Branch rows (nested under the group; project/current not repeated) ──
    for (const branch of branches) {
      const branchName = branch.branch || '(none)';
      const isCurrent = project.currentBranch && branchName === project.currentBranch;
      const nonCurrent =
        project.currentBranch && branchName !== project.currentBranch && branchName !== '(none)';
      const trackedFiles = Number(branch.trackedFiles || 0);
      const trackedChunks = Number(branch.trackedChunks || 0);
      const ftsFiles = Number(branch.ftsFiles || 0);
      const ftsBytes = Number(branch.ftsBytes || 0);
      const signal = nonCurrent
        ? pill('non-current', 'warn')
        : isCurrent
          ? pill('current', 'ok')
          : pill('other', 'muted');
      html.push(
        `<tr class="bc-branch${nonCurrent ? ' branch-warn-row' : ''}${isCurrent ? ' bc-current' : ''}" data-bc-parent="${escapeHtml(key)}"${collapsed ? ' hidden' : ''}>
          <td class="bc-branch-name"><code>${escapeHtml(branchName)}</code></td>
          <td class="num">${escapeHtml(summarizeQueue(branch.queue))}</td>
          <td class="num">${trackedFiles.toLocaleString()} files · ${trackedChunks.toLocaleString()} chunks</td>
          <td class="num">${ftsFiles.toLocaleString()} files · ${formatBytes(ftsBytes)}</td>
          <td>${signal}</td>
        </tr>`
      );
    }
  });

  if (branchRowCount === 0) {
    els.branchCoverageEmpty.textContent = 'No branch coverage data yet.';
    els.branchCoverageEmpty.hidden = false;
    els.branchCoverageTable.hidden = true;
    els.branchCoverageMeta.textContent = '';
    return;
  }

  els.branchCoverageEmpty.hidden = true;
  els.branchCoverageTable.hidden = false;
  els.branchCoverageBody.innerHTML = html.join('');

  const warningCount = projects.reduce((n, p) => n + ((p.warnings || []).length ? 1 : 0), 0);
  const source = data.source?.searchDb ? ` · search.db: ${data.source.searchDb}` : '';
  els.branchCoverageMeta.textContent = `· ${projects.length} project(s) · ${branchRowCount} branch row(s) · ${warningCount} warning(s)${source}`;
}

// Collapse/expand a project group (delegated; survives the 5s re-render via bcCollapsed).
els.branchCoverageBody?.addEventListener('click', (e) => {
  const group = e.target.closest('.bc-group');
  if (!group) return;
  const key = group.dataset.bcKey;
  const collapsed = !bcCollapsed.has(key);
  if (collapsed) bcCollapsed.add(key);
  else bcCollapsed.delete(key);
  group.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
  const caret = group.querySelector('.bc-caret');
  if (caret) caret.textContent = collapsed ? '▸' : '▾';
  els.branchCoverageBody
    .querySelectorAll(`.bc-branch[data-bc-parent="${key}"]`)
    .forEach((r) => {
      r.hidden = collapsed;
    });
});

async function loadBranchCoverage() {
  if (!els.branchCoverageTable) return;
  try {
    const data = await api('/admin/api/branches/coverage');
    renderBranchCoverage(data);
  } catch (err) {
    els.branchCoverageEmpty.textContent = `Failed to load: ${err.message}`;
    els.branchCoverageEmpty.hidden = false;
    els.branchCoverageTable.hidden = true;
    els.branchCoverageMeta.textContent = '';
  }
}

els.reloadBranchCoverageBtn?.addEventListener('click', () => loadBranchCoverage());

// ── Largest files (search.db file_metadata) ────────────────────────

function formatBytes(n) {
  if (n === null || n === undefined) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

async function loadLargestFiles() {
  if (!els.largestFilesTable) return; // safety: section not in DOM
  const skipped = els.largestFilesSkippedOnly?.checked ? '&skipped=1' : '';
  try {
    const data = await api(`/admin/api/files/large?limit=20${skipped}`);
    const files = data?.files ?? [];
    if (data?.degraded) {
      els.largestFilesEmpty.textContent = `search.db unavailable: ${data.degraded.message}`;
      els.largestFilesEmpty.hidden = false;
      els.largestFilesTable.hidden = true;
      els.largestFilesMeta.textContent = '';
      return;
    }
    if (files.length === 0) {
      els.largestFilesEmpty.textContent = skipped
        ? 'No files currently marked fts5_skipped=1.'
        : 'No files indexed yet — daemon may still be walking.';
      els.largestFilesEmpty.hidden = false;
      els.largestFilesTable.hidden = true;
      els.largestFilesMeta.textContent = '';
      return;
    }
    els.largestFilesEmpty.hidden = true;
    els.largestFilesTable.hidden = false;
    els.largestFilesBody.innerHTML = files.map((f) => {
      const skippedBadge = f.fts5_skipped
        ? '<span class="pill pill-warn">skipped</span>'
        : '<span class="dim small">indexed</span>';
      // Show the trailing portion of the path to keep rows compact;
      // full path is in the title attribute on hover.
      const shortPath = f.file_path.length > 80
        ? '…' + f.file_path.slice(-78)
        : f.file_path;
      return `<tr>
        <td><code title="${escapeHtml(f.file_path)}">${escapeHtml(shortPath)}</code></td>
        <td><span class="dim small">${escapeHtml(f.tenant_id)}</span></td>
        <td><span class="dim small">${escapeHtml(f.branch)}</span></td>
        <td class="right"><strong>${formatBytes(f.size_bytes)}</strong></td>
        <td>${skippedBadge}</td>
      </tr>`;
    }).join('');
    els.largestFilesMeta.textContent = `· ${files.length} rows · source: ${data.source ?? 'search.db'}`;
  } catch (err) {
    els.largestFilesEmpty.textContent = `Failed to load: ${err.message}`;
    els.largestFilesEmpty.hidden = false;
    els.largestFilesTable.hidden = true;
  }
}

els.reloadLargestFilesBtn?.addEventListener('click', () => loadLargestFiles());
els.largestFilesSkippedOnly?.addEventListener('change', () => loadLargestFiles());

// ── Client configs ─────────────────────────────────────────────────

let clientConfigs = null;

async function loadClientConfigs() {
  if (!clientConfigs) {
    clientConfigs = await api('/admin/api/config/clients');
  }
  return clientConfigs;
}

function showConfig(label, text) {
  els.configHint.hidden = true;
  els.configDisplay.hidden = false;
  els.configLabel.textContent = label;
  els.configPre.textContent = text;
  els.copyMsg.textContent = '';
}

els.showClaudeConfigBtn.addEventListener('click', async () => {
  try {
    const cfg = await loadClientConfigs();
    showConfig('Paste into claude_desktop_config.json → mcpServers', cfg.claudeDesktop.mcp_remote);
  } catch (e) { toast(e.message, 'error'); }
});

els.showCodexConfigBtn.addEventListener('click', async () => {
  try {
    const cfg = await loadClientConfigs();
    showConfig('Paste into ~/.codex/config.toml', cfg.codex);
  } catch (e) { toast(e.message, 'error'); }
});

els.copyConfigBtn.addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(els.configPre.textContent);
    els.copyMsg.textContent = 'Copied!';
    setTimeout(() => { els.copyMsg.textContent = ''; }, 2000);
  } catch { els.copyMsg.textContent = 'Copy failed'; }
});

// ── Logs ────────────────────────────────────────────────────────────

const LOG_LEVEL_CLASSES = { trace: 'muted', debug: 'muted', info: '', warn: 'warn', error: 'err', fatal: 'err' };

function fmtLogTime(ts) {
  if (!ts) return '—';
  try {
    const d = new Date(ts);
    return d.toISOString().slice(11, 23); // HH:MM:SS.mmm
  } catch { return String(ts); }
}

function renderLogs(data) {
  const rows = data.lines ?? [];
  if (rows.length === 0) {
    els.logsEmpty.hidden = false;
    els.logsTable.hidden = true;
    els.logsMeta.hidden = true;
    return;
  }
  els.logsEmpty.hidden = true;
  els.logsMeta.hidden = false;
  els.logsMeta.textContent = `${rows.length} lines · ${data.file ?? ''}`;
  els.logsTable.hidden = false;
  els.logsBody.innerHTML = rows.map(row => {
    const level = row.level ?? row.severity ?? '';
    const levelName = typeof level === 'number'
      ? (level >= 50 ? 'error' : level >= 40 ? 'warn' : level >= 30 ? 'info' : level >= 20 ? 'debug' : 'trace')
      : String(level).toLowerCase();
    const cls = LOG_LEVEL_CLASSES[levelName] ?? '';
    const msg = escapeHtml(row.msg ?? row.message ?? JSON.stringify(row));
    const ctx = Object.entries(row)
      .filter(([k]) => !['level','time','msg','name','pid','hostname','component','v'].includes(k))
      .map(([k,v]) => `<span class="dim">${escapeHtml(k)}=</span>${escapeHtml(String(v))}`)
      .join(' ');
    return `<tr class="${cls ? 'log-' + cls : ''}">
      <td class="mono dim nowrap">${escapeHtml(fmtLogTime(row.time))}</td>
      <td class="log-level ${cls ? 'pill-' + cls : ''}">${escapeHtml(levelName)}</td>
      <td>${msg}${ctx ? '<br><span class="dim small mono">' + ctx + '</span>' : ''}</td>
    </tr>`;
  }).join('');
}

els.loadMcpLogsBtn.addEventListener('click', async () => {
  els.loadMcpLogsBtn.disabled = true;
  els.loadMcpLogsBtn.textContent = 'Loading…';
  try {
    const lines = els.logLinesSelect.value;
    const data = await api(`/admin/api/logs/mcp?lines=${lines}`);
    renderLogs(data);
    if (data.note) toast(data.note, 'error');
  } catch (e) { toast(e.message, 'error'); }
  finally {
    els.loadMcpLogsBtn.disabled = false;
    els.loadMcpLogsBtn.textContent = 'MCP logs';
  }
});

els.clearLogsBtn.addEventListener('click', () => {
  els.logsEmpty.hidden = false;
  els.logsTable.hidden = true;
  els.logsMeta.hidden = true;
  els.logsEmpty.textContent = 'Click "MCP logs" to load recent server log entries.';
});

// ── Stack actions ───────────────────────────────────────────────────

els.checkDaemonMetricsBtn.addEventListener('click', async () => {
  els.checkDaemonMetricsBtn.disabled = true;
  try {
    const data = await api('/admin/api/daemon/raw-health');
    els.daemonMetricsVal.innerHTML = data.ok ? pill('healthy', 'ok') : pill('unreachable', 'err');
    els.daemonMetricsDetail.textContent = data.body ?? data.reason ?? '—';
  } catch (e) {
    els.daemonMetricsVal.innerHTML = pill('error', 'err');
    els.daemonMetricsDetail.textContent = e.message;
  } finally {
    els.checkDaemonMetricsBtn.disabled = false;
  }
});

els.forceReconcileBtn.addEventListener('click', async () => {
  els.forceReconcileBtn.disabled = true;
  const originalLabel = els.forceReconcileBtn.textContent;
  els.forceReconcileBtn.textContent = 'Reapplying…';
  try {
    const result = await api('/admin/api/ignore/reapply', { method: 'POST' });
    els.stackActionsStatus.textContent = `Ignore reapplied · ${new Date().toLocaleTimeString()}`;
    els.stackActionsLog.hidden = false;
    els.stackActionsLog.textContent =
      `Projects processed : ${result.projectsProcessed}\n` +
      `Stale deletes      : ${result.staleDeleted}\n` +
      `Missing adds       : ${result.missingAdded}\n` +
      `\nEnqueued items drain through the normal queue processor.`;
    toast(
      result.staleDeleted + result.missingAdded > 0
        ? `Reapplied ignore: ${result.staleDeleted} deletes, ${result.missingAdded} adds`
        : 'Reapplied ignore — no changes',
    );
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.forceReconcileBtn.disabled = false;
    els.forceReconcileBtn.textContent = originalLabel;
  }
});

els.refreshHealthBtn.addEventListener('click', async () => {
  els.refreshHealthBtn.disabled = true;
  try {
    const h = await api('/admin/api/health');
    renderHostHealth(h);
    els.adminPidVal.textContent = `pid ${h.mcp?.pid ?? '—'} · up ${formatUptime(h.mcp?.uptimeSeconds ?? 0)}`;
    toast('Health refreshed');
  } catch (e) {
    toast(e.message, 'error');
  } finally {
    els.refreshHealthBtn.disabled = false;
  }
});

// ── Boot ───────────────────────────────────────────────────────────

/**
 * Try to bootstrap the auth token from `/admin/init`. The server returns
 * the configured MCP_HTTP_TOKEN ONLY when the request comes from a
 * loopback peer (127.0.0.1 / ::1). Anywhere else it 403s and we fall
 * back to the manual login prompt.
 *
 * When the server runs with `MCP_HTTP_TRUST_LOCALHOST=1`, the bearer
 * check is also bypassed for loopback clients, so we can skip straight
 * to the app even if the init endpoint hadn't existed.
 */
async function tryAutoInit() {
  try {
    const resp = await fetch('/admin/init', { method: 'GET' });
    if (!resp.ok) return false;
    const data = await resp.json().catch(() => null);
    if (!data) return false;
    if (typeof data.token === 'string' && data.token.length > 0) {
      token = data.token;
      sessionStorage.setItem(TOKEN_KEY, token);
      return true;
    }
    if (data.trustLocalhost === true) {
      // Token may legitimately be empty in trust-localhost setups; the
      // server will accept the requests without an Authorization header.
      token = '';
      return true;
    }
    return false;
  } catch {
    return false;
  }
}

// ══ Tabs ═════════════════════════════════════════════════════════════
//
// The admin used to be one long scroll (14 sections). They're grouped into
// 5 tabs; the health KPI row stays pinned above the tab bar (untagged →
// visible on every tab). Section → tab is assigned here by heading so the
// HTML markup stays declarative, and the active tab deep-links via the URL
// hash (#playground) and is remembered across reloads.

els.tabNav = document.getElementById('tabNav');

const TAB_ORDER = ['overview', 'playground', 'projects', 'indexing', 'config'];
const TAB_OF = {
  'Host integrations': 'overview',
  'Stack actions': 'overview',
  'Discovery settings': 'projects',
  'Discovered candidates': 'projects',
  'Registered projects': 'projects',
  'Branch coverage': 'projects',
  'Failed indexing items': 'indexing',
  'Largest indexed files': 'indexing',
  'Global index exclusions': 'config',
  'Behavioral rules': 'config',
  'Client configuration': 'config',
  Logs: 'config',
};

function tagSectionsWithTabs() {
  els.appView.querySelectorAll(':scope > section').forEach((sec) => {
    if (sec.dataset.tab) return;
    if (sec.id === 'playgroundCard') {
      sec.dataset.tab = 'playground';
      return;
    }
    const h2 = sec.querySelector('.card-head h2, h2');
    const key = h2 ? h2.textContent.trim() : '';
    if (TAB_OF[key]) {
      sec.dataset.tab = TAB_OF[key];
      return;
    }
    if (sec.querySelector('#debugRaw')) {
      sec.dataset.tab = 'overview';
      return;
    }
    // Untagged (the health stat grid) stays visible on every tab.
  });
}

function readStoredTab() {
  try {
    return sessionStorage.getItem('wqm.admin.tab');
  } catch {
    return null;
  }
}

function showTab(name) {
  if (!TAB_ORDER.includes(name)) name = 'overview';
  els.appView.querySelectorAll('[data-tab]').forEach((sec) => {
    sec.hidden = sec.dataset.tab !== name;
  });
  els.tabNav.querySelectorAll('[data-tab-btn]').forEach((btn) => {
    const on = btn.dataset.tabBtn === name;
    btn.classList.toggle('active', on);
    btn.setAttribute('aria-selected', on ? 'true' : 'false');
  });
  try {
    sessionStorage.setItem('wqm.admin.tab', name);
  } catch {
    /* ignore */
  }
  if (location.hash.slice(1) !== name) history.replaceState(null, '', '#' + name);
}

function initTabs() {
  tagSectionsWithTabs();
  const fromHash = location.hash.slice(1);
  const fromStore = readStoredTab();
  const initial = TAB_ORDER.includes(fromHash)
    ? fromHash
    : fromStore && TAB_ORDER.includes(fromStore)
      ? fromStore
      : 'overview';
  showTab(initial);
}

els.tabNav.addEventListener('click', (e) => {
  const btn = e.target.closest('[data-tab-btn]');
  if (btn) showTab(btn.dataset.tabBtn);
});
window.addEventListener('hashchange', () => {
  const h = location.hash.slice(1);
  if (TAB_ORDER.includes(h)) showTab(h);
});

// ══ Tools playground ═════════════════════════════════════════════════
//
// Invoke any of the 12 MCP tools through the SAME server-side routeTool
// path the MCP client uses (POST /admin/api/tools/invoke) and render the
// raw result — so what you see here is byte-for-byte what an agent gets.
// Forms are generated from PG_TOOLS: one typed field per tool parameter,
// with the less-common ones tucked under an "Advanced" disclosure.
//   type: text | textarea | number | bool | enum | tags | json
//   req: required · adv: goes under Advanced · ph: placeholder

els.pgTool = document.getElementById('pgTool');
els.pgProject = document.getElementById('pgProject');
els.pgToolHint = document.getElementById('pgToolHint');
els.pgForm = document.getElementById('pgForm');
els.pgRunBtn = document.getElementById('pgRunBtn');
els.pgClearBtn = document.getElementById('pgClearBtn');
els.pgStatus = document.getElementById('pgStatus');
els.pgResult = document.getElementById('pgResult');
els.pgResultHead = document.getElementById('pgResultHead');
els.pgHits = document.getElementById('pgHits');
els.pgRaw = document.getElementById('pgRaw');
els.pgPresetRow = document.getElementById('pgPresetRow');
els.pgPresets = document.getElementById('pgPresets');

const F = (k, type, opts = {}) => ({ k, type, ...opts });

const PG_TOOLS = [
  { name: 'search', mutates: false, note: 'semantic / hybrid ranked search', fields: [
    F('query', 'textarea', { req: true, ph: 'search text — write it in English' }),
    F('cwd', 'text', { ph: '/abs/repo — scopes to a project' }),
    F('mode', 'enum', { enum: ['semantic', 'hybrid'] }),
    F('exact', 'bool'),
    F('fileType', 'enum', { enum: ['code', 'text', 'config', 'data', 'docs', 'web', 'slides', 'build'] }),
    F('pathGlob', 'text', { ph: '**/*.rs' }),
    F('pathExclude', 'text', { ph: 'old_project/**' }),
    F('limit', 'number'),
    F('scope', 'enum', { enum: ['project', 'global', 'all'] }),
    F('branch', 'text'),
    F('collection', 'enum', { enum: ['projects', 'libraries', 'rules', 'scratchpad'], adv: true }),
    F('component', 'text', { adv: true }),
    F('tag', 'text', { adv: true }),
    F('tags', 'tags', { adv: true }),
    F('excludeTests', 'bool', { adv: true }),
    F('includeLibraries', 'bool', { adv: true }),
    F('includeScratchpad', 'bool', { adv: true }),
    F('includeGraphContext', 'bool', { adv: true }),
    F('scoreThreshold', 'number', { adv: true }),
    F('responseFormat', 'enum', { enum: ['concise', 'detailed', 'packed'], adv: true }),
    F('summary', 'bool', { adv: true }),
    F('offset', 'number', { adv: true }),
    F('maxBytesPerHit', 'number', { adv: true }),
    F('maxResponseBytes', 'number', { adv: true }),
    F('contextLines', 'number', { adv: true }),
    F('libraryName', 'text', { adv: true }),
    F('projectId', 'text', { adv: true }),
  ] },
  { name: 'grep', mutates: false, note: 'exact / regex FTS substring search', fields: [
    F('pattern', 'text', { req: true, ph: 'literal or regex' }),
    F('cwd', 'text'),
    F('regex', 'bool'),
    F('caseSensitive', 'bool'),
    F('contextLines', 'number'),
    F('pathGlob', 'text'),
    F('pathExclude', 'text'),
    F('branch', 'text'),
    F('scope', 'enum', { enum: ['project', 'all'] }),
    F('maxResults', 'number'),
    F('offset', 'number', { adv: true, ph: 'next_offset from previous page' }),
    F('maxBytesPerLine', 'number', { adv: true }),
    F('maxResponseBytes', 'number', { adv: true }),
    F('projectId', 'text', { adv: true }),
  ] },
  { name: 'list', mutates: false, note: 'indexed file/folder structure', fields: [
    F('cwd', 'text'),
    F('path', 'text', { ph: 'subfolder relative to root' }),
    F('format', 'enum', { enum: ['tree', 'summary', 'flat'] }),
    F('depth', 'number'),
    F('pattern', 'text', { ph: '**/*.ts (floats)' }),
    F('pathExclude', 'text'),
    F('extension', 'text', { ph: 'rs' }),
    F('fileType', 'enum', { enum: ['code', 'text', 'data', 'config', 'build', 'web'] }),
    F('language', 'text'),
    F('component', 'text', { adv: true }),
    F('branch', 'text'),
    F('includeTests', 'bool', { adv: true }),
    F('maxResponseBytes', 'number', { adv: true }),
    F('limit', 'number', { adv: true }),
    F('pageSize', 'number', { adv: true }),
    F('cursor', 'text', { adv: true }),
    F('projectId', 'text', { adv: true }),
  ] },
  { name: 'retrieve', mutates: false, note: 'fetch documents by id / locator / filter', fields: [
    F('documentId', 'text', { ph: 'point id from a search/list hit' }),
    F('filePath', 'text'),
    F('lineNumber', 'number'),
    F('collection', 'enum', { enum: ['projects', 'libraries', 'rules', 'scratchpad'] }),
    F('branch', 'text'),
    F('cwd', 'text'),
    F('filter', 'json', { adv: true, ph: '{ "document_id": "..." }' }),
    F('libraryName', 'text', { adv: true }),
    F('limit', 'number', { adv: true }),
    F('offset', 'number', { adv: true }),
    F('projectId', 'text', { adv: true }),
  ] },
  { name: 'graph', mutates: false, note: 'code-relationship graph', fields: [
    F('action', 'enum', { req: true, enum: ['stats', 'relations', 'impact', 'usages', 'hotspots', 'bridges', 'modules'] }),
    F('symbol', 'text', { ph: 'required for relations/impact/usages' }),
    F('filePath', 'text', { ph: 'required for relations' }),
    F('symbolType', 'enum', { enum: ['function', 'async_function', 'method', 'struct', 'class', 'enum', 'interface', 'trait', 'type_alias', 'constant', 'module', 'macro', 'impl'] }),
    F('cwd', 'text'),
    F('edgeTypes', 'tags', { adv: true, ph: 'CALLS,IMPORTS,CONTAINS' }),
    F('maxHops', 'number', { adv: true }),
    F('minConfidence', 'number', { adv: true }),
    F('topK', 'number', { adv: true }),
    F('memberLimit', 'number', { adv: true }),
    F('minSize', 'number', { adv: true }),
    F('maxSamples', 'number', { adv: true }),
    F('projectId', 'text', { adv: true }),
  ] },
  { name: 'store', mutates: true, note: 'create note / library / project / url', fields: [
    F('type', 'enum', { enum: ['scratchpad', 'library', 'url', 'project'] }),
    F('content', 'textarea'),
    F('cwd', 'text'),
    F('projectId', 'text'),
    F('libraryName', 'text'),
    F('forProject', 'bool'),
    F('path', 'text', { ph: 'for type=project' }),
    F('name', 'text'),
    F('title', 'text'),
    F('url', 'text', { ph: 'for type=url' }),
    F('filePath', 'text', { adv: true }),
    F('tags', 'tags', { adv: true }),
    F('sourceType', 'enum', { enum: ['user_input', 'web', 'file', 'scratchbook', 'note'], adv: true }),
    F('metadata', 'json', { adv: true, ph: '{ "k": "v" }' }),
  ] },
  { name: 'scratchpad', mutates: true, note: 'list / update / delete notes', fields: [
    F('action', 'enum', { req: true, enum: ['list', 'update', 'delete'] }),
    F('content', 'textarea', { ph: 'VERBATIM current note text (update/delete)' }),
    F('newContent', 'textarea', { ph: 'replacement text (update)' }),
    F('title', 'text'),
    F('tags', 'tags'),
    F('projectId', 'text'),
    F('cwd', 'text'),
    F('limit', 'number', { adv: true }),
    F('summary', 'bool', { adv: true }),
    F('maxResponseBytes', 'number', { adv: true }),
    F('cursor', 'text', { adv: true, ph: 'next_cursor from a previous list' }),
  ] },
  { name: 'rules', mutates: true, note: 'behavioral rules CRUD', fields: [
    F('action', 'enum', { req: true, enum: ['list', 'add', 'update', 'remove'] }),
    F('label', 'text', { ph: 'word-word-word, max 15' }),
    F('content', 'textarea'),
    F('scope', 'enum', { enum: ['project', 'global'] }),
    F('projectId', 'text'),
    F('cwd', 'text'),
    F('title', 'text'),
    F('priority', 'number'),
    F('tags', 'tags', { adv: true }),
    F('limit', 'number', { adv: true }),
  ] },
  { name: 'workspace_index', mutates: true, note: 'indexing registry & agent branches (mutating actions need allowMutation + env)', fields: [
    F('action', 'enum', { req: true, enum: ['indexing_status', 'list_projects', 'project_status', 'status_all', 'list_branches', 'agent_branch_status', 'observe_project', 'observe_all', 'incremental_check', 'incremental_check_all', 'init', 'add_project', 'start_agent_branch', 'finish_agent_branch', 'abandon_agent_branch', 'register_wqm', 'register_all_wqm', 'cleanup_orphans', 'sync_current_branch'] }),
    F('cwd', 'text'),
    F('projectId', 'text'),
    F('projectPath', 'text'),
    F('allowMutation', 'bool'),
    F('projectName', 'text', { adv: true }),
    F('branchName', 'text', { adv: true }),
    F('branch', 'text', { adv: true }),
    F('baseBranch', 'text', { adv: true }),
    F('returnBranch', 'text', { adv: true }),
    F('worktreePath', 'text', { adv: true }),
    F('worktreeRoot', 'text', { adv: true }),
    F('useWorktree', 'bool', { adv: true }),
    F('purpose', 'text', { adv: true }),
    F('createdBy', 'text', { adv: true }),
    F('repoDir', 'text', { adv: true }),
    F('currentBranch', 'text', { adv: true }),
    F('commitHash', 'text', { adv: true }),
    F('isWorktree', 'bool', { adv: true }),
    F('gitRemote', 'text', { adv: true }),
    F('hookName', 'text', { adv: true }),
    F('payload', 'json', { adv: true }),
  ] },
  { name: 'embedding', mutates: false, note: 'report active embedding provider (no parameters)', fields: [] },
  { name: 'search_eval', mutates: false, note: 'benchmark search quality (hit@k / recall / MRR)', fields: [
    F('cwd', 'text'),
    F('projectId', 'text'),
    F('scope', 'enum', { enum: ['project', 'global', 'all'] }),
    F('limit', 'number'),
    F('topK', 'number'),
    F('rerank', 'bool'),
    F('rerankWeight', 'number'),
    F('includeTopPaths', 'bool'),
    F('summary', 'bool'),
    F('cases', 'json', { adv: true, ph: '[{ "query": "...", "expectedFiles": ["src/x.ts"] }]' }),
  ] },
];

// One-click example arg sets per tool — project-agnostic (the project picker
// supplies cwd), using real symbols/paths from THIS repo so graph/relations
// presets resolve when run against the workspace-qdrant project. Applying a
// preset resets the form, then fills the named fields.
const PG_PRESETS = {
  search: [
    { label: 'Semantic · code', args: { query: 'recover stale queue leases', mode: 'semantic', fileType: 'code' } },
    { label: 'Hybrid', args: { query: 'idempotency key sha256', mode: 'hybrid', fileType: 'code' } },
    { label: 'Drop legacy dir', args: { query: 'reference schedule template', fileType: 'code', pathExclude: 'old_project/**' } },
    { label: 'Packed read', args: { query: 'branch dedup base_point', fileType: 'code', responseFormat: 'packed' } },
  ],
  grep: [
    { label: 'Literal', args: { pattern: 'routeTool' } },
    { label: 'Regex alternation', args: { pattern: 'search|grep|graph', regex: true } },
    { label: 'In a filetype', args: { pattern: 'service', pathGlob: '**/*.proto' } },
    { label: 'Exclude tests', args: { pattern: 'TODO', pathExclude: '**/test/**' } },
  ],
  list: [
    { label: 'Summary', args: { format: 'summary' } },
    { label: 'Rust files', args: { format: 'flat', extension: 'rs', limit: 100 } },
    { label: 'Migrations (any depth)', args: { pattern: 'V*.sql' } },
    { label: 'Tree of src', args: { path: 'src', format: 'tree', depth: 2 } },
  ],
  retrieve: [
    { label: 'By file + line', args: { filePath: 'src/typescript/mcp-server/src/tool-dispatcher.ts', lineNumber: 87 } },
    { label: 'By filter (document_id)', args: { collection: 'projects', filter: { document_id: 'PASTE_HASH' } } },
  ],
  graph: [
    { label: 'Stats', args: { action: 'stats' } },
    { label: 'Impact', args: { action: 'impact', symbol: 'routeTool' } },
    { label: 'Callers (usages)', args: { action: 'usages', symbol: 'routeTool' } },
    { label: 'Relations (precise)', args: { action: 'relations', symbol: 'diagnoseEmptyResult', filePath: 'src/typescript/mcp-server/src/tools/empty-diagnosis.ts', minConfidence: 0.5 } },
    { label: 'Hotspots', args: { action: 'hotspots', topK: 20 } },
  ],
  store: [
    { label: 'Scratchpad note', args: { type: 'scratchpad', content: 'playground test note', title: 'test' } },
    { label: 'Library doc', args: { type: 'library', libraryName: 'tokio', title: 'Tokio', content: 'Async runtime for Rust.' } },
    { label: 'Register project', args: { type: 'project', path: '/home/alkmimm/respositorios/<repo>' } },
  ],
  scratchpad: [
    { label: 'List', args: { action: 'list' } },
    { label: 'List (full bodies)', args: { action: 'list', summary: false } },
    { label: 'Update (by id)', args: { action: 'update', id: '<point id from list>', newContent: '<new text>' } },
    { label: 'Update (by content)', args: { action: 'update', content: '<verbatim old text>', newContent: '<new text>' } },
    { label: 'Delete (by id)', args: { action: 'delete', id: '<point id from list>' } },
    { label: 'Delete (by content)', args: { action: 'delete', content: '<verbatim note text>' } },
  ],
  rules: [
    { label: 'List', args: { action: 'list' } },
    { label: 'Add (project)', args: { action: 'add', label: 'prefer-uv', title: 'Use uv', content: 'Use uv for Python deps.', scope: 'project' } },
    { label: 'Remove', args: { action: 'remove', label: 'prefer-uv' } },
  ],
  workspace_index: [
    { label: 'Indexing status', args: { action: 'indexing_status' } },
    { label: 'List projects', args: { action: 'list_projects' } },
    { label: 'Observe all', args: { action: 'observe_all' } },
  ],
  search_eval: [
    { label: 'Bundled dataset', args: { limit: 10, topK: 10 } },
    { label: 'Rerank off (A/B)', args: { rerank: false } },
    { label: 'Ad-hoc cases', args: { cases: [{ query: 'where is the tool dispatcher', expectedFiles: ['src/typescript/mcp-server/src/tool-dispatcher.ts'] }] } },
  ],
  // embedding: no parameters → no presets
};

function pgSpec() {
  return PG_TOOLS.find((t) => t.name === els.pgTool.value);
}

function pgPresetsFor(name) {
  return PG_PRESETS[name] || [];
}

function pgRenderPresets() {
  const spec = pgSpec();
  const presets = spec ? pgPresetsFor(spec.name) : [];
  if (!presets.length) {
    els.pgPresetRow.hidden = true;
    els.pgPresets.innerHTML = '';
    return;
  }
  els.pgPresetRow.hidden = false;
  els.pgPresets.innerHTML = presets
    .map((p, i) => `<button type="button" class="secondary small" data-preset="${i}">${escapeHtml(p.label)}</button>`)
    .join('');
}

// Inverse of pgCollect: write an args object back into the rendered controls.
function pgApplyArgs(spec, args) {
  let touchedAdv = false;
  for (const f of spec.fields) {
    if (!(f.k in args)) continue;
    const el = document.getElementById(`pg_${f.k}`);
    if (!el) continue;
    const v = args[f.k];
    if (f.type === 'bool') el.value = v === true ? 'true' : v === false ? 'false' : '';
    else if (f.type === 'tags') el.value = Array.isArray(v) ? v.join(', ') : String(v);
    else if (f.type === 'json') el.value = typeof v === 'string' ? v : JSON.stringify(v, null, 2);
    else el.value = String(v);
    if (f.adv) touchedAdv = true;
  }
  if (touchedAdv) {
    const d = els.pgForm.querySelector('details');
    if (d) d.open = true;
  }
}

function pgApplyPreset(preset) {
  const spec = pgSpec();
  if (!spec) return;
  pgRenderForm(); // clean slate + project cwd/projectId
  pgApplyArgs(spec, preset.args || {});
  els.pgStatus.textContent = `preset: ${preset.label}`;
  els.pgStatus.className = 'dim small';
}

function pgControl(f) {
  const id = `pg_${f.k}`;
  const attrs = `id="${id}" data-k="${f.k}" data-type="${f.type}"`;
  const ph = escapeHtml(f.ph || '');
  if (f.type === 'textarea') return `<textarea ${attrs} rows="3" placeholder="${ph}"></textarea>`;
  if (f.type === 'json') return `<textarea ${attrs} rows="3" placeholder="${ph}" style="font-family:ui-monospace,monospace;font-size:var(--text-xs)"></textarea>`;
  if (f.type === 'number') return `<input ${attrs} type="number" placeholder="${ph}" />`;
  if (f.type === 'bool') return `<select ${attrs}><option value="">(default)</option><option value="true">true</option><option value="false">false</option></select>`;
  if (f.type === 'enum') {
    const head = f.req ? '' : '<option value="">(default)</option>';
    const opts = f.enum.map((e) => `<option value="${escapeHtml(e)}">${escapeHtml(e)}</option>`).join('');
    return `<select ${attrs}>${head}${opts}</select>`;
  }
  return `<input ${attrs} type="text" placeholder="${ph}" />`;
}

function pgFieldRow(f) {
  const label = escapeHtml(f.label || f.k) + (f.req ? ' <span class="error">*</span>' : '');
  // Textareas / JSON editors take their own full-width row so they never sit in
  // a flex row beside short inputs — that mismatch bottom-aligned the short
  // inputs against the tall textarea and misaligned every label in the row.
  const isFull = f.type === 'textarea' || f.type === 'json';
  const style = isFull ? 'flex-basis:100%; min-width:100%' : 'min-width:220px';
  return `<div class="field grow" style="${style}"><label for="pg_${f.k}">${label}</label>${pgControl(f)}</div>`;
}

function pgRenderForm() {
  const spec = pgSpec();
  if (!spec) { els.pgForm.innerHTML = ''; return; }
  els.pgToolHint.textContent = (spec.mutates ? '⚠ mutates data · ' : '') + (spec.note || '');
  els.pgToolHint.className = spec.mutates ? 'error small' : 'dim small';
  const primary = spec.fields.filter((f) => !f.adv);
  const adv = spec.fields.filter((f) => f.adv);
  let html = '';
  if (!spec.fields.length) html += '<p class="dim small">No parameters — just Run.</p>';
  if (primary.length) html += `<div class="row" style="flex-wrap:wrap; gap:var(--space-4); align-items:flex-start">${primary.map(pgFieldRow).join('')}</div>`;
  if (adv.length) {
    html += `<details style="margin-top:var(--space-4)"><summary class="dim small">Advanced (${adv.length})</summary>` +
      `<div class="row" style="flex-wrap:wrap; gap:var(--space-4); align-items:flex-start; margin-top:var(--space-3)">${adv.map(pgFieldRow).join('')}</div></details>`;
  }
  els.pgForm.innerHTML = html;
  pgApplyProject(false);
  pgRenderPresets();
}

function pgCollect(spec) {
  const args = {};
  for (const f of spec.fields) {
    const el = document.getElementById(`pg_${f.k}`);
    if (!el) continue;
    const raw = el.value;
    if (f.type === 'bool') { if (raw !== '') args[f.k] = raw === 'true'; continue; }
    if (f.type === 'enum') { if (raw !== '') args[f.k] = raw; continue; }
    const v = raw.trim();
    if (v === '') continue;
    if (f.type === 'number') {
      const n = Number(v);
      if (Number.isNaN(n)) throw new Error(`${f.k}: "${v}" is not a number`);
      args[f.k] = n;
    } else if (f.type === 'tags') {
      const arr = v.split(',').map((s) => s.trim()).filter(Boolean);
      if (arr.length) args[f.k] = arr;
    } else if (f.type === 'json') {
      try { args[f.k] = JSON.parse(v); } catch (e) { throw new Error(`${f.k}: invalid JSON — ${e.message}`); }
    } else {
      args[f.k] = v;
    }
  }
  for (const f of spec.fields) {
    if (f.req && !(f.k in args)) throw new Error(`${f.k} is required`);
  }
  return args;
}

function pgHitsTable(tool, result) {
  if (!result || typeof result !== 'object') return '';
  if (tool === 'search' && Array.isArray(result.results)) {
    if (!result.results.length) return '<p class="dim small">0 hits.</p>';
    const rows = result.results.slice(0, 25).map((r) => {
      const loc = r.location || (r.metadata && r.metadata.relative_path) || '';
      const score = r.rerankScore != null ? `r ${Number(r.rerankScore).toFixed(3)}`
        : (r.score != null ? Number(r.score).toFixed(3) : '');
      const snip = String(r.content || '').replace(/\s+/g, ' ').slice(0, 160);
      return `<tr><td><span class="path">${escapeHtml(loc)}</span></td><td class="num">${escapeHtml(score)}</td><td class="dim small">${escapeHtml(snip)}</td></tr>`;
    }).join('');
    return `<table class="data"><thead><tr><th>Location</th><th class="num">Score</th><th>Snippet</th></tr></thead><tbody>${rows}</tbody></table>`;
  }
  if (tool === 'grep' && Array.isArray(result.matches)) {
    if (!result.matches.length) return '<p class="dim small">0 matches.</p>';
    const rows = result.matches.slice(0, 40).map((m) => {
      const loc = `${m.file || m.relative_path || m.file_path || ''}:${m.line != null ? m.line : (m.line_number != null ? m.line_number : '')}`;
      return `<tr><td><span class="path">${escapeHtml(loc)}</span></td><td class="dim small">${escapeHtml(String(m.content || '').slice(0, 200))}</td></tr>`;
    }).join('');
    return `<table class="data"><thead><tr><th>File:Line</th><th>Match</th></tr></thead><tbody>${rows}</tbody></table>`;
  }
  return '';
}

function pgRenderResult(resp) {
  els.pgResult.hidden = false;
  const verdict = resp.ok ? pill('ok', 'ok') : pill('error', 'warn');
  const ms = resp.latencyMs != null ? `${resp.latencyMs} ms` : '';
  els.pgResultHead.innerHTML = `<code>${escapeHtml(resp.tool || '')}</code> ${verdict} <span class="dim small">${escapeHtml(ms)}</span>`;
  els.pgHits.innerHTML = resp.ok
    ? pgHitsTable(resp.tool, resp.result)
    : `<pre class="debug error" style="white-space:pre-wrap">${escapeHtml(resp.error || 'error')}</pre>`;
  els.pgRaw.textContent = JSON.stringify(resp.ok ? resp.result : resp, null, 2);
}

async function pgRun() {
  const spec = pgSpec();
  if (!spec) return;
  let args;
  try {
    args = pgCollect(spec);
  } catch (e) {
    els.pgStatus.textContent = e.message;
    els.pgStatus.className = 'error small';
    return;
  }
  if (spec.mutates && !window.confirm(`"${spec.name}" can MUTATE indexed state. Run it?`)) return;
  els.pgStatus.textContent = 'running…';
  els.pgStatus.className = 'dim small';
  els.pgRunBtn.disabled = true;
  try {
    const resp = await api('/admin/api/tools/invoke', { method: 'POST', body: { tool: spec.name, args } });
    pgRenderResult(resp);
    els.pgStatus.textContent = '';
  } catch (e) {
    // Malformed request (4xx) — api() throws. Render it in the results pane.
    pgRenderResult({ ok: false, tool: spec.name, error: e.message });
    els.pgStatus.textContent = '';
  } finally {
    els.pgRunBtn.disabled = false;
  }
}

function populatePlaygroundProjects() {
  if (!els.pgProject) return;
  const cur = els.pgProject.value;
  const opts = ['<option value="">— none —</option>'].concat(
    (lastRegistered || []).map((r) =>
      `<option value="${escapeHtml(r.path)}" data-tenant="${escapeHtml(r.tenantId || '')}">${escapeHtml(r.path)}</option>`)
  );
  els.pgProject.innerHTML = opts.join('');
  if (cur) els.pgProject.value = cur;
}

function pgApplyProject(force) {
  const sel = els.pgProject;
  if (!sel || !sel.value) return;
  const tenant = (sel.selectedOptions[0] && sel.selectedOptions[0].dataset.tenant) || '';
  const cwdEl = document.getElementById('pg_cwd');
  if (cwdEl && (force || !cwdEl.value)) cwdEl.value = sel.value;
  const pidEl = document.getElementById('pg_projectId');
  if (pidEl && tenant && (force || !pidEl.value)) pidEl.value = tenant;
}

function initPlayground() {
  if (els.pgTool.options.length === 0) {
    els.pgTool.innerHTML = PG_TOOLS.map((t) =>
      `<option value="${t.name}">${t.name}${t.mutates ? ' ⚠' : ''}</option>`).join('');
  }
  populatePlaygroundProjects();
  pgRenderForm();
}

els.pgTool.addEventListener('change', pgRenderForm);
els.pgProject.addEventListener('change', () => pgApplyProject(true));
els.pgPresets.addEventListener('click', (e) => {
  const btn = e.target.closest('button[data-preset]');
  if (!btn) return;
  const spec = pgSpec();
  const preset = spec && pgPresetsFor(spec.name)[Number(btn.dataset.preset)];
  if (preset) pgApplyPreset(preset);
});
els.pgRunBtn.addEventListener('click', pgRun);
els.pgForm.addEventListener('submit', (e) => { e.preventDefault(); pgRun(); });
els.pgClearBtn.addEventListener('click', () => {
  pgRenderForm();
  els.pgResult.hidden = true;
  els.pgStatus.textContent = '';
});

(async () => {
  if (!token) {
    const ok = await tryAutoInit();
    if (!ok) {
      showLogin();
      return;
    }
  }
  showApp();
  startPolling();
})();

document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && token !== undefined) refresh();
});
