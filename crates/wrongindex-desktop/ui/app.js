// WrongIndex desktop UI. State always comes from the daemon through the Rust
// bridge; events only trigger re-reads. All text is set with textContent.
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const state = {
  view: 'overview',
  overview: null,
  selected: null,
  detail: null,
  activity: [],
  preview: null,
  // Search inspector: per-project query and last result.
  search: { projectId: null, query: '', result: null, busy: false },
  // Diagnostics export.
  diag: { redact: true, preview: null, busy: false },
  // Settings: sign-in start state as last read from the system.
  autostart: null,
  // Service settings as last applied by the daemon (never a local guess).
  service: null,
  serviceDraft: null,
  serviceBusy: false,
  // Updates: the last check (from GitHub, through the CLI) and the last
  // update started from a desktop, as recorded on disk.
  updates: { check: null, checking: false, last: null, starting: false },
};

// Per-viewer preferences in the WebView's own storage; the page works the
// same without it (storage can be unavailable).
const AUTO_CHECK_KEY = 'wi.updates.autoCheck';
const LAST_CHECK_KEY = 'wi.updates.lastCheckMs';
const SEEN_UPDATE_KEY = 'wi.updates.seenAtMs';
const DAY_MS = 24 * 3600 * 1000;
function load(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}
function store(key, value) {
  try { localStorage.setItem(key, value); } catch { /* not kept */ }
}

// Service settings shown in Settings, with their units and ranges (the
// daemon validates again and is the authority).
const SERVICE_FIELDS = [
  { key: 'parallelJobs', label: 'Index jobs at once', unit: '', min: 1, max: 8, note: 'One per project; more uses more CPU and memory.' },
  { key: 'nativeMemoryMb', label: 'Memory per indexing run', unit: 'MiB', min: 0, max: 1048576, note: '0 = automatic (a quarter of physical memory, at least 2 GiB).' },
  { key: 'watchDebounceMs', label: 'Wait after the last edit', unit: 'ms', min: 100, max: 10000 },
  { key: 'watchMaxWaitMs', label: 'Longest wait under continuous edits', unit: 'ms', min: 500, max: 60000 },
  { key: 'sessionGraceSeconds', label: 'Keep watching after the last client', unit: 's', min: 0, max: 3600 },
  { key: 'backupKeep', label: 'Index backups kept per project', unit: '', min: 1, max: 20 },
  { key: 'watchParkAfterFailures', label: 'Park a project after failed runs in a row', unit: '', min: 2, max: 100, note: 'A parked project is retried once every 15 minutes until a run succeeds.' },
];
const ACTIVITY_CAP = 200;

// ─── helpers ────────────────────────────────────────────────────────────────

function el(tag, attrs, ...children) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k.startsWith('on')) n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v === true ? '' : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    n.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return n;
}

const FRESHNESS = {
  current: '● Current',
  pending: '◐ Pending',
  degraded: '▲ Degraded',
  unavailable: '✕ Unavailable',
  unknown: '? Unknown',
};
const ENGINE = {
  auto: 'Automatic (follow the service)',
  native: 'Native engine only',
  legacy: 'Compatibility worker (WrongStack engine)',
};
const BACKEND = {
  native: 'WrongIndex native engine',
  legacy: 'Compatibility worker (WrongStack engine)',
  unsupported: 'Not written by WrongIndex yet',
};
const OWNERSHIP = {
  owned: 'Owned',
  unowned: 'Not owned',
  negotiating: 'Negotiating',
  conflict: 'Conflict',
  releasing: 'Releasing',
  recovery_required: 'Recovery required',
};
const POLICY = {
  pinned: 'Pinned (always watch)',
  session: 'Session (while a client is active)',
  manual: 'Manual (no watching)',
  disabled: 'Disabled',
};

/** Column header; an action column gets a name for screen readers only. */
function th(h) {
  return el('th', { scope: 'col' }, h || el('span', { class: 'sr-only' }, 'Actions'));
}

function badge(value, labels) {
  const v = String(value ?? 'unknown');
  return el('span', { class: `state ${v}` }, labels[v] ?? v);
}

function when(ms) {
  if (!ms) return '—';
  const d = new Date(Number(ms));
  return d.toLocaleString();
}

function ago(ms) {
  if (!ms) return '—';
  const s = Math.max(0, Math.round((Date.now() - Number(ms)) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

function duration(ms) {
  const s = Math.round(Number(ms) / 1000);
  if (s < 120) return `${s}s`;
  if (s < 7200) return `${Math.round(s / 60)}m`;
  return `${Math.round(s / 3600)}h`;
}

let toastTimer;
function toast(message, bad = false) {
  const t = document.getElementById('toast');
  t.textContent = message;
  t.className = bad ? 'toast bad' : 'toast';
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, bad ? 8000 : 3500);
}

function errText(e) {
  if (e && typeof e === 'object' && 'code' in e) return `${e.code}: ${e.message}`;
  return String(e);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(errText(e), true);
    throw e;
  }
}

function confirmDialog(title, body, okLabel, danger = true) {
  const d = document.getElementById('confirm');
  document.getElementById('confirm-title').textContent = title;
  document.getElementById('confirm-body').textContent = body;
  const ok = document.getElementById('confirm-ok');
  ok.textContent = okLabel;
  ok.className = danger ? 'btn danger' : 'btn';
  return new Promise((resolve) => {
    d.addEventListener('close', () => resolve(d.returnValue === 'ok'), { once: true });
    d.showModal();
  });
}

// ─── data ───────────────────────────────────────────────────────────────────

let refreshTimer = null;
function scheduleRefresh() {
  if (refreshTimer) return;
  refreshTimer = setTimeout(async () => {
    refreshTimer = null;
    await refresh();
  }, 250);
}

async function refresh() {
  try {
    state.overview = await invoke('overview');
  } catch (e) {
    state.overview = { connected: false, reason: errText(e) };
  }
  if (state.selected && state.overview.connected) {
    try {
      state.detail = await invoke('project_detail', { projectId: state.selected });
    } catch (e) {
      if (e && e.code === 'PROJECT_NOT_FOUND') {
        state.selected = null;
        state.detail = null;
      }
    }
  }
  render();
}

function projects() {
  return state.overview?.snapshot?.projects ?? [];
}

// ─── header ─────────────────────────────────────────────────────────────────

function renderHeader() {
  const o = state.overview;
  const conn = document.getElementById('conn');
  const meta = document.getElementById('service-meta');
  const start = document.getElementById('btn-start');
  const stop = document.getElementById('btn-stop');
  const upd = document.getElementById('btn-update');
  const check = state.updates.check;
  upd.hidden = !(check && check.updateAvailable);
  if (!upd.hidden) upd.textContent = `Update available: ${check.latest}`;
  if (!o) {
    conn.textContent = 'Connecting…';
    conn.className = 'conn';
    return;
  }
  if (o.connected) {
    const h = o.health;
    conn.textContent = h.draining ? 'Service stopping' : h.paused ? 'Background indexing paused' : 'Service running';
    conn.className = h.draining ? 'conn down' : h.paused ? 'conn paused' : 'conn up';
    meta.textContent = `v${h.daemonVersion} · up ${duration(h.uptimeMs)} · ${h.queuedJobs} queued · ${h.runningJobs} running`;
    start.hidden = true;
    stop.hidden = false;
  } else {
    conn.textContent = 'Service not running';
    conn.className = 'conn down';
    meta.textContent = '';
    start.hidden = false;
    stop.hidden = true;
  }
}

// ─── views ──────────────────────────────────────────────────────────────────

function disconnectedView() {
  return el('div', { class: 'empty' },
    el('p', {}, 'The WrongIndex service is not running.'),
    el('p', { class: 'note' }, state.overview?.reason ?? ''),
    el('p', { class: 'note' }, 'No live values are shown while disconnected.'),
    el('button', { class: 'btn', onclick: startService }, 'Start Index Service'));
}

function overviewView() {
  if (!state.overview?.connected) return disconnectedView();
  const ps = projects();
  const count = (pred) => ps.filter(pred).length;
  const cards = el('div', { class: 'cards' },
    card('Projects', ps.length),
    card('Watching', count((p) => p.watchHealth && p.watchHealth !== 'off')),
    card('Current', count((p) => p.freshness === 'current')),
    card('Pending', count((p) => p.freshness === 'pending')),
    card('Degraded / unavailable', count((p) => p.freshness === 'degraded' || p.freshness === 'unavailable')),
    card('Queue', `${state.overview.health.queuedJobs} / ${state.overview.health.runningJobs} running`));
  const attention = ps.filter((p) => p.lastError);
  return el('section', {},
    el('h1', {}, 'Overview'),
    ...diskBanners(),
    cards,
    attention.length ? el('h2', {}, 'Needs attention') : null,
    attention.length ? projectTable(attention) : null,
    el('h2', {}, 'Projects'),
    ps.length ? projectTable(ps) : firstUse());
}

function megabytes(n) {
  const mb = n / 1024 / 1024;
  if (mb < 1) return `${Math.max(1, Math.round(n / 1024))} KB`;
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`;
}

const USAGE = [
  ['indexBytes', 'indexes'], ['backupBytes', 'backups'], ['walBytes', 'write-ahead logs'],
  ['stagingBytes', 'unfinished runs'], ['logBytes', 'service log'], ['serviceBytes', 'other service data'],
];

/** What WrongIndex keeps on a volume short of space, largest first. */
function usageLine(u) {
  if (!u) return null;
  const parts = USAGE.filter(([k]) => u[k] > 0).sort((a, b) => u[b[0]] - u[a[0]]).map(([k, label]) => `${label} ${megabytes(u[k])}`);
  if (!parts.length) return null;
  const hint = u.backupBytes > u.indexBytes ? ' Backups are in each index directory\'s .wrongindex-backups folder; "Index backups kept per project" (Settings) limits how many are kept.' : '';
  return el('p', { class: 'note' }, `WrongIndex keeps there: ${parts.join(', ')}.${hint}`);
}

/** Volumes short of space and large write-ahead logs (`resource.warning`, kept in the snapshot). */
function diskBanners() {
  const all = state.overview?.snapshot?.resources ?? [];
  const wals = all.filter((r) => r.resource === 'wal' && r.level === 'large').map((r) => {
    const p = projects().find((x) => x.project.projectId === r.projectId);
    const name = p ? p.project.displayName : r.projectId;
    const times = r.indexBytes > 0 ? ` (${(r.walBytes / r.indexBytes).toFixed(1)}× the index)` : '';
    return el('div', { class: 'banner', role: 'status' },
      el('p', {}, `The write-ahead log of ${name} is ${megabytes(r.walBytes)}${times}.`),
      el('p', { class: 'note' }, r.blockedByReader
        ? `A program has kept a read transaction open on this index since ${when(r.blockedSinceMs)} (a WrongStack session or another reader), so each run adds to the log. WrongIndex folds it into the index as soon as that transaction ends; it never closes other programs' connections.`
        : 'WrongIndex folds it into the index once no reader needs it.'));
  });
  return wals.concat(all.filter((r) => r.resource === 'disk' && r.level !== 'ok').map((r) => {
    const n = (r.projects ?? []).length;
    const which = n ? ` (${n} project index${n === 1 ? '' : 'es'})` : ' (the service data)';
    return r.level === 'full'
      ? el('div', { class: 'banner bad', role: 'alert' },
        el('p', {}, `Disk almost full on ${r.volume}: ${megabytes(r.freeBytes)} free${which}.`),
        el('p', { class: 'note' }, `Indexing there waits until at least ${megabytes(r.fullBytes)} is free (a full run also needs room for a copy of the index), then resumes by itself. WrongIndex deletes nothing to make room.`),
        usageLine(r.usage))
      : el('div', { class: 'banner', role: 'status' },
        el('p', {}, `Disk space is getting low on ${r.volume}: ${megabytes(r.freeBytes)} free${which}.`),
        el('p', { class: 'note' }, `Indexing stops there below ${megabytes(r.fullBytes)}.`),
        usageLine(r.usage));
  }));
}

function card(k, v) {
  return el('div', { class: 'card' }, el('div', { class: 'k' }, k), el('div', { class: 'v' }, v));
}

function firstUse() {
  return el('div', { class: 'empty' },
    el('p', {}, 'No projects are registered yet.'),
    el('button', { class: 'btn', onclick: () => go('projects') }, 'Add Project'));
}

function projectTable(ps) {
  const rows = ps.map((p) => {
    const open = () => openProject(p.project.projectId);
    return el('tr', {
      class: 'clickable', tabindex: 0, onclick: open,
      onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(); } },
    },
    el('td', {}, el('div', {}, p.project.displayName), el('div', { class: 'note mono' }, p.project.checkoutRoot)),
    el('td', {}, POLICY[p.project.watchPolicy]?.split(' ')[0] ?? p.project.watchPolicy),
    el('td', {}, badge(p.freshness, FRESHNESS)),
    el('td', {}, badge(p.ownership, OWNERSHIP)),
    el('td', {}, p.activity),
    el('td', {}, p.watchHealth),
    el('td', {}, String(p.generation)),
    el('td', {}, String(p.pendingPaths)),
    el('td', {}, ago(p.lastIndexedMs)),
    el('td', { class: p.lastError ? 'error' : '' }, p.lastError ? '⚠ error' : ''));
  });
  return el('table', {},
    el('thead', {}, el('tr', {}, ...['Project', 'Policy', 'Freshness', 'Ownership', 'Activity', 'Watch', 'Generation', 'Pending', 'Last indexed', ''].map(th))),
    el('tbody', {}, rows));
}

function projectsView() {
  if (!state.overview?.connected) return disconnectedView();
  if (state.selected && state.detail) return detailView();
  const input = el('input', { type: 'text', id: 'add-path', placeholder: 'C:\\path\\to\\checkout', 'aria-label': 'Project folder' });
  if (state.preview?.path) input.value = state.preview.path;
  const check = async (path) => {
    try {
      const r = await call('preview_project', { path });
      state.preview = { path, ...r };
    } catch { state.preview = { path }; }
    render(true);
  };
  const browseBtn = el('button', { class: 'btn', onclick: async () => {
    const r = await call('pick_folder');
    if (r.path) await check(r.path);
  } }, 'Browse…');
  const previewBtn = el('button', { class: 'btn subtle', onclick: () => check(input.value.trim()) }, 'Check folder');
  const ps = projects();
  return el('section', {},
    el('h1', {}, 'Projects'),
    el('h2', {}, 'Add project'),
    el('div', { class: 'row' }, browseBtn, input, previewBtn),
    el('p', { class: 'note' }, 'Choose a folder or paste its path. Checking is read-only: it resolves the WrongStack index location and inspects any existing index without changing it.'),
    state.preview?.resolved ? previewPanel() : null,
    el('h2', {}, 'Registered'),
    ps.length ? projectTable(ps) : el('div', { class: 'empty' }, 'No projects are registered yet.'));
}

function previewPanel() {
  const pv = state.preview;
  const ins = pv.inspection;
  const policy = el('select', { 'aria-label': 'Watch policy' },
    ...['manual', 'session', 'pinned'].map((p) => el('option', { value: p }, POLICY[p])));
  const existing = ins.verdict === 'missing'
    ? 'No index yet — the first index run will create it.'
    : `Existing index: ${ins.verdict}${ins.writeAdmissible ? ' (reusable)' : ' (not writable by this version)'}`;
  return el('div', {},
    el('div', { class: 'grid' },
      el('div', { class: 'k' }, 'Checkout'), el('div', { class: 'mono' }, pv.resolved.projectRoot),
      el('div', { class: 'k' }, 'Index directory'), el('div', { class: 'mono' }, pv.resolved.indexDir),
      el('div', { class: 'k' }, 'Linked worktree'), el('div', {}, pv.resolved.linkedWorktree ? 'yes' : 'no'),
      el('div', { class: 'k' }, 'Index'), el('div', { class: ins.writeAdmissible || ins.verdict === 'missing' ? '' : 'error' }, existing)),
    el('div', { class: 'row', style: 'margin-top:8px' },
      el('label', {}, 'Watch policy '), policy,
      el('button', { class: 'btn', onclick: async () => {
        const r = await call('add_project', { checkoutRoot: pv.resolved.projectRoot, indexDir: pv.resolved.indexDir, policy: policy.value });
        toast(r.created ? 'Project registered.' : 'Project was already registered.');
        state.preview = null;
        await openProject(r.project.projectId);
      } }, 'Register project')),
    el('p', { class: 'note' }, 'Registering does not index yet. Claim ownership and choose a policy in the project view; indexing work is shown as jobs.'));
}

function detailView() {
  const d = state.detail;
  const s = d.status;
  const p = s.project;
  const id = p.projectId;
  const act = (action, arg, msg) => async () => {
    await call('project_action', { projectId: id, action, arg });
    toast(msg);
    await refresh();
  };
  const policy = el('select', { 'aria-label': 'Watch policy' },
    ...['manual', 'session', 'pinned', 'disabled'].map((v) => el('option', { value: v, selected: v === p.watchPolicy }, POLICY[v])));
  const owned = s.ownership === 'owned';
  const engine = el('select', { id: 'engine-choice' },
    ...['auto', 'native', 'legacy'].map((v) => el('option', { value: v, selected: v === (p.backendPreference ?? 'auto') }, ENGINE[v])));
  const journal = d.ownership?.journal ?? [];
  return el('section', {},
    el('button', { class: 'btn subtle back', onclick: () => { state.selected = null; state.detail = null; render(); } }, '← All projects'),
    el('h1', {}, p.displayName),
    el('div', { class: 'grid' },
      el('div', { class: 'k' }, 'Freshness'), el('div', {}, badge(s.freshness, FRESHNESS), s.reconcileReason ? ` (reconcile: ${s.reconcileReason})` : ''),
      el('div', { class: 'k' }, 'Ownership'), el('div', {}, badge(s.ownership, OWNERSHIP)),
      el('div', { class: 'k' }, 'Activity'), el('div', {}, s.activity),
      el('div', { class: 'k' }, 'Watch'), el('div', {}, `${s.watchHealth} · ${POLICY[p.watchPolicy] ?? p.watchPolicy} · ${s.sessions} session(s)`,
        retryNote(s) ? el('div', { class: s.parked || s.watchHealth === 'disk_full' ? 'error' : 'note', role: 'status' }, retryNote(s)) : null),
      el('div', { class: 'k' }, 'Generation'), el('div', {}, String(s.generation)),
      el('div', { class: 'k' }, 'Pending paths'), el('div', {}, String(s.pendingPaths)),
      el('div', { class: 'k' }, 'Last indexed'), el('div', {}, when(s.lastIndexedMs)),
      el('div', { class: 'k' }, 'Written by'), el('div', {}, BACKEND[p.backend] ?? p.backend),
      el('div', { class: 'k' }, 'Checkout'), el('div', { class: 'mono' }, p.checkoutRoot),
      el('div', { class: 'k' }, 'Index directory'), el('div', { class: 'mono' }, p.indexDir),
      el('div', { class: 'k' }, 'Configuration revision'), el('div', {}, String(p.configurationRevision)),
      s.lastError ? el('div', { class: 'k' }, 'Last error') : null,
      s.lastError ? el('div', { class: 'error' }, s.lastError, el('div', { class: 'note' }, nextAction(s.lastError))) : null,
      s.progress ? el('div', { class: 'k' }, 'Running now') : null,
      s.progress ? el('div', { role: 'status' }, progressText(s.progress)) : null),
    el('h2', {}, 'Actions'),
    el('div', { class: 'row' },
      owned
        ? el('button', { class: 'btn subtle', onclick: act('release', null, 'Ownership released.') }, 'Release ownership')
        : el('button', { class: 'btn', onclick: act('claim', null, 'Ownership claimed.') }, 'Claim ownership'),
      el('button', { class: 'btn subtle', onclick: act('check', null, 'Health check queued.') }, 'Check index'),
      el('button', { class: 'btn subtle', disabled: !owned, title: owned ? null : 'Claim ownership first', onclick: act('reconcile', null, 'Reconciliation queued.') }, 'Reconcile'),
      el('button', { class: 'btn subtle', onclick: () => call('open_folder', { projectId: id }) }, 'Open folder'),
      el('button', { class: 'btn subtle', onclick: async () => {
        const ok = await confirmDialog(
          'Rebuild index?',
          `A fresh index of ${p.displayName} is written from the source files and replaces the current one, even if it is unreadable, from another WrongStack version, or carries extra data. The current file is kept next to it as index.db.wrongindex-backup-<time> in ${p.indexDir}; nothing is deleted. Use this when regular indexing refuses the existing index.`,
          'Rebuild');
        if (!ok) return;
        await call('project_action', { projectId: id, action: 'rebuild' });
        toast('Maintenance rebuild queued; the previous index will be kept as a backup.');
        await refresh();
      } }, 'Rebuild index…'),
      el('button', { class: 'btn subtle', onclick: async () => {
        const ok = await confirmDialog('Remove registration?', `${p.displayName} will no longer be watched or indexed by WrongIndex. The source folder and the index at ${p.indexDir} are kept.`, 'Remove');
        if (!ok) return;
        await call('project_action', { projectId: id, action: 'remove' });
        state.selected = null; state.detail = null;
        toast('Registration removed; source and index preserved.');
        await refresh();
      } }, 'Remove…')),
    el('div', { class: 'row', style: 'margin-top:8px' },
      el('label', {}, 'Watch policy '), policy,
      el('button', { class: 'btn subtle', onclick: async () => {
        await call('project_action', { projectId: id, action: 'policy', arg: policy.value });
        toast(`Policy applied by the service: ${policy.value}.`);
        await refresh();
      } }, 'Apply')),
    el('div', { class: 'row', style: 'margin-top:8px' },
      el('label', { for: 'engine-choice' }, 'Engine '), engine,
      el('button', { class: 'btn subtle', onclick: async () => {
        if (engine.value === (p.backendPreference ?? 'auto')) return;
        await call('project_action', { projectId: id, action: 'backend', arg: engine.value });
        toast(`Engine choice applied by the service: ${ENGINE[engine.value]}. ${owned ? 'A reconciliation with it is queued.' : ''}`);
        await refresh();
      } }, 'Apply')),
    filesAndErrors(d, id),
    backupsPanel(d, p),
    searchPanel(p),
    el('h2', {}, 'Jobs'),
    jobsTable(d.jobs ?? []),
    journal.length ? el('h2', {}, 'Ownership history') : null,
    journal.length ? el('table', {},
      el('thead', {}, el('tr', {}, ...['When', 'Epoch', 'Phase', 'Previous owner', 'New owner'].map(th))),
      el('tbody', {}, journal.map((j) => el('tr', {},
        el('td', {}, when(j.atMs)), el('td', {}, String(j.epoch ?? '')),
        el('td', {}, j.phase ?? ''), el('td', { class: 'mono' }, j.oldOwner ?? '—'), el('td', { class: 'mono' }, j.newOwner ?? '—'))))) : null);
}

const PHASES = {
  starting: 'Starting',
  discover: 'Finding files',
  parse: 'Parsing',
  write: 'Writing the index',
};

function progressText(pr) {
  const base = String(pr.phase || '').split(':')[0];
  const label = PHASES[pr.phase] ?? PHASES[base] ?? pr.phase;
  const files = pr.files ? ` (${pr.files} files)` : '';
  const done = pr.done ? ` — finished in ${pr.ms} ms` : '…';
  return `${label}${files}${done} · ${ago(pr.atMs)}`;
}

/** A plain next step for an error message (compatibility §, plan 2.4). */
// Watcher retry state (WI-208): failures back automatic runs off; after the
// configured number in a row the project is parked.
function retryNote(s) {
  if (s.watchHealth === 'disk_full') return 'Waiting for disk space: pending changes are indexed by themselves once the volume has room.';
  const n = s.consecutiveFailures ?? 0;
  if (!n) return '';
  const next = s.nextRetryMs ? ` Next automatic attempt ${when(s.nextRetryMs)}.` : '';
  return s.parked
    ? `Parked after ${n} failed runs in a row.${next} Fix the error, then use Reconcile or Retry.`
    : `${n} failed run(s) in a row; retrying with a growing delay.${next}`;
}

function nextAction(message) {
  const m = String(message);
  if (/checkout root .*unavailable|root is unavailable/.test(m)) return 'Restore or reconnect the project folder; watching resumes by itself.';
  if (/owned by|OWNER_CONFLICT|ownership/.test(m)) return 'Another process owns this index. Release it there, or claim it here.';
  if (/newer WrongStack|schema version|PROFILE_UNSUPPORTED/.test(m)) return 'Update WrongIndex, or use Rebuild index (the current file is kept).';
  if (/enrichment data|extra schema|outside the baseline/.test(m)) return 'This index carries data only WrongStack maintains; the compatibility worker handles it.';
  if (/disk|space|DISK_FULL/i.test(m)) return 'Free disk space on that drive. Watched projects resume by themselves; otherwise retry.';
  if (/access|denied|permission/i.test(m)) return 'Fix access to the folder, then retry.';
  return 'Retry the job; if it keeps failing, export diagnostics.';
}

function filesAndErrors(d, id) {
  const jobs = d.jobs ?? [];
  const lastWrite = jobs.find((j) => ['full', 'incremental', 'reconcile', 'rebuild'].includes(j.kind));
  const oversized = jobs
    .map((j) => j.result?.native?.oversizedFiles ?? [])
    .find((l) => l.length) ?? [];
  const failed = jobs.filter((j) => (j.state === 'failed' || j.state === 'canceled') && ['full', 'incremental', 'reconcile'].includes(j.kind));
  // Identical failures count once.
  const groups = new Map();
  for (const j of failed) {
    const key = j.error ? `${j.error.code}: ${j.error.message}` : j.state;
    const g = groups.get(key) ?? { key, count: 0, latest: j };
    g.count += 1;
    groups.set(key, g);
  }
  const recovered = lastWrite && lastWrite.state === 'succeeded';
  const rows = [...groups.values()];
  return el('div', {},
    el('h2', {}, 'Files and errors'),
    !rows.length && !oversized.length
      ? el('div', { class: 'empty' }, 'No file or job errors in the recent jobs.')
      : null,
    oversized.length
      ? el('div', {},
        el('p', {}, `${oversized.length} file(s) over the 5 MiB read limit were left out (as WrongStack does). Shrinking or splitting them brings them back on the next run.`),
        el('ul', { class: 'mono' }, oversized.slice(0, 50).map((f) => el('li', {}, f))))
      : null,
    rows.length
      ? el('table', {},
        el('thead', {}, el('tr', {}, ...['Error', 'Times', 'Latest', 'Next step', ''].map(th))),
        el('tbody', {}, rows.map((g) => el('tr', {},
          el('td', { class: recovered ? '' : 'error' }, g.key, recovered ? el('div', { class: 'note' }, 'Recovered: a later run succeeded.') : null),
          el('td', {}, String(g.count)),
          el('td', {}, when(g.latest.completedAtMs)),
          el('td', { class: 'note' }, nextAction(g.key)),
          el('td', {}, el('button', { class: 'btn subtle', onclick: async () => {
            await call('project_action', { projectId: id, action: 'retry', arg: g.latest.jobId });
            toast('Job queued again.');
            await refresh();
          } }, 'Retry'))))))
      : null);
}

function backupsPanel(d, p) {
  const list = d.backups ?? [];
  if (!list.length) return null;
  return el('div', {},
    el('h2', {}, 'Index backups'),
    el('p', { class: 'note' }, 'Taken each time the service took ownership of this index.'),
    el('table', {},
      el('thead', {}, el('tr', {}, ...['Backup', 'Taken', 'Size', ''].map(th))),
      el('tbody', {}, list.map((b) => el('tr', {},
        el('td', { class: 'mono' }, b.name),
        el('td', {}, when(b.modifiedAtMs)),
        el('td', {}, `${(b.bytes / 1024 / 1024).toFixed(1)} MiB`),
        el('td', {}, el('button', { class: 'btn subtle', disabled: d.status.ownership !== 'owned', title: d.status.ownership === 'owned' ? null : 'Claim ownership first', onclick: async () => {
          const ok = await confirmDialog(
            'Restore this backup?',
            `The index of ${p.displayName} is replaced with ${b.name}. The current index is first saved as another backup, so nothing is lost. Clients see the restored generation.`,
            'Restore');
          if (!ok) return;
          await call('project_action', { projectId: p.projectId, action: 'restore', arg: b.name });
          toast('Restore queued; the current index is backed up first.');
          await refresh();
        } }, 'Restore…')))))));
}

function searchPanel(p) {
  const sr = state.search.projectId === p.projectId ? state.search : { query: '', result: null };
  const input = el('input', {
    type: 'text', 'aria-label': 'Search symbols', placeholder: 'Symbol name or text',
    oninput: (e) => { state.search.query = e.target.value; state.search.projectId = p.projectId; },
    onkeydown: (e) => { if (e.key === 'Enter') run(); },
  });
  input.value = sr.query ?? '';
  async function run() {
    const query = input.value.trim();
    if (!query) return;
    state.search = { projectId: p.projectId, query, result: null, busy: true };
    render(true);
    try {
      state.search.result = await call('search', { projectId: p.projectId, query });
    } catch { /* toast already shown */ }
    state.search.busy = false;
    render(true);
  }
  const rel = (file) => {
    const root = p.checkoutRoot.replace(/[\\/]+$/, '');
    return file.toLowerCase().startsWith(root.toLowerCase()) ? file.slice(root.length + 1) : file;
  };
  const r = sr.result;
  let body = null;
  if (sr.busy) body = el('p', { class: 'note' }, 'Searching…');
  else if (r) {
    const rows = r.result?.results ?? [];
    body = rows.length
      ? el('table', {},
        el('thead', {}, el('tr', {}, ...['Symbol', 'Kind', 'Location', 'Signature'].map(th))),
        el('tbody', {}, rows.map((x) => el('tr', {},
          el('td', {}, x.name),
          el('td', {}, x.kind),
          el('td', { class: 'mono' }, `${rel(x.file)}:${x.line}`),
          el('td', { class: 'mono' }, x.signature)))))
      : el('div', { class: 'empty' }, 'No matches.');
  }
  return el('div', {},
    el('h2', {}, 'Search inspector'),
    el('div', { class: 'row' }, input, el('button', { class: 'btn subtle', onclick: run }, 'Search')),
    el('p', { class: 'note' }, r
      ? `${r.result?.total ?? (r.result?.results ?? []).length} match(es) in generation ${r.generation} · served by the ${r.backend} reader. Read-only.`
      : 'Searches the published index read-only, the way WrongStack clients do.'),
    body);
}

function jobsTable(jobs) {
  if (!jobs.length) return el('div', { class: 'empty' }, 'No jobs yet.');
  return el('table', {},
    el('thead', {}, el('tr', {}, ...['Job', 'Kind', 'State', 'Admitted', 'Finished', 'Result', ''].map(th))),
    el('tbody', {}, jobs.map((j) => el('tr', {},
      el('td', { class: 'mono' }, j.jobId),
      el('td', {}, j.kind),
      el('td', {}, badge(j.state, {})),
      el('td', {}, when(j.admittedAtMs)),
      el('td', {}, when(j.completedAtMs)),
      el('td', { class: j.error ? 'error' : '' }, j.error ? `${j.error.code}: ${j.error.message}` : jobSummary(j)),
      el('td', {}, j.state === 'queued'
        ? el('button', { class: 'btn subtle', onclick: async () => {
          const r = await call('cancel_job', { jobId: j.jobId });
          toast(r.canceled ? 'Job canceled.' : `Not canceled: ${r.reason}`);
          await refresh();
        } }, 'Cancel')
        : null)))));
}

function jobSummary(j) {
  const r = j.result;
  if (!r) return '';
  if (r.verdict) return `verdict ${r.verdict}`;
  if (r.restoredFrom) return `restored · generation ${r.generation}`;
  if (r.native) {
    const n = r.native;
    const what = n.outcome === 'unchanged' ? 'unchanged'
      : n.outcome === 'updated' ? `${n.changedFiles ?? 0} changed file(s)`
        : `${n.filesIndexed ?? 0} files`;
    return `generation ${r.generation} · ${what}${n.durationMs ? ` · ${duration(n.durationMs)}` : ''}`;
  }
  if (r.generation !== undefined) return `generation ${r.generation} · ${r.filesIndexed ?? '?'} files`;
  return '';
}

function activityView() {
  return el('section', {},
    el('h1', {}, 'Activity'),
    el('p', { class: 'note' }, `The last ${ACTIVITY_CAP} service events seen by this window. After a reconnect the views are re-read from the service; missed events are not replayed here.`),
    state.activity.length
      ? el('table', {},
        el('thead', {}, el('tr', {}, ...['#', 'Time', 'Event', 'Project', 'Details'].map(th))),
        el('tbody', {}, state.activity.map((e) => el('tr', {},
          el('td', {}, String(e.seq ?? '')),
          el('td', {}, when(e.atMs)),
          el('td', {}, e.kind),
          el('td', { class: 'mono' }, projectName(e.projectId)),
          el('td', { class: 'mono' }, e.data ? JSON.stringify(e.data) : '')))))
      : el('div', { class: 'empty' }, 'No events yet.'));
}

function projectName(id) {
  if (!id) return '—';
  const p = projects().find((x) => x.project.projectId === id);
  return p ? p.project.displayName : id;
}

function diagnosticsView() {
  const o = state.overview;
  if (!o?.connected) return disconnectedView();
  const h = o.health;
  return el('section', {},
    el('h1', {}, 'Diagnostics'),
    el('div', { class: 'grid' },
      el('div', { class: 'k' }, 'Instance'), el('div', { class: 'mono' }, h.daemonInstanceId),
      el('div', { class: 'k' }, 'Version'), el('div', {}, h.daemonVersion),
      el('div', { class: 'k' }, 'Process'), el('div', {}, `pid ${h.pid}, started ${when(h.startedAtMs)}`),
      el('div', { class: 'k' }, 'SQLite'), el('div', {}, h.sqliteVersion),
      el('div', { class: 'k' }, 'Data root'), el('div', { class: 'mono' }, o.dataRoot),
      el('div', { class: 'k' }, 'Event position'), el('div', {}, `seq ${o.snapshot.seq}`)),
    el('h2', {}, 'Backends'),
    el('ul', {}, h.backendInventory.map((b) => el('li', { class: 'mono' }, b))),
    el('p', { class: 'note' }, 'This view never shows the service credential.'),
    diagnosticsExport());
}

function diagnosticsExport() {
  const dg = state.diag;
  const redact = el('input', { type: 'checkbox', id: 'redact', checked: dg.redact,
    onchange: (e) => { dg.redact = e.target.checked; dg.preview = null; render(true); } });
  const preview = async () => {
    dg.busy = true; render(true);
    try { dg.preview = await call('diagnostics_preview', { redactPaths: dg.redact }); } catch { /* toast shown */ }
    dg.busy = false; render(true);
  };
  const exportIt = async () => {
    dg.busy = true; render(true);
    try {
      const r = await call('export_diagnostics', { redactPaths: dg.redact });
      if (r.saved) toast(`Diagnostics saved to ${r.saved}`);
    } catch { /* toast shown */ }
    dg.busy = false; render(true);
  };
  const cats = dg.preview?.categories ?? [];
  return el('div', {},
    el('h2', {}, 'Export diagnostics'),
    el('p', { class: 'note' }, 'One bounded JSON bundle: versions, service health, project state, recent jobs, ownership history, read-only index inspection and the tail of the service log. Never included: the service credential, source code, index contents. Your home folder is always shown as ~.'),
    el('div', { class: 'row' },
      redact, el('label', { for: 'redact' }, 'Also replace project paths and names with short hashes'),
      el('button', { class: 'btn subtle', disabled: dg.busy, onclick: preview }, 'Preview contents'),
      el('button', { class: 'btn', disabled: dg.busy, onclick: exportIt }, 'Export…')),
    cats.length ? el('table', { style: 'margin-top:8px' },
      el('thead', {}, el('tr', {}, el('th', { scope: 'col' }, 'Category'), el('th', { scope: 'col' }, 'Size'))),
      el('tbody', {}, cats.map((c) => el('tr', {}, el('td', {}, c.category), el('td', {}, `${(c.bytes / 1024).toFixed(1)} KiB`))))) : null);
}

function serviceSettingsSection() {
  const sv = state.service;
  if (!sv) {
    invoke('service_settings')
      .then((r) => { state.service = r; state.serviceDraft = { ...r.settings }; render(true); })
      .catch((e) => { state.service = { error: errText(e) }; render(true); });
    return el('p', { class: 'note' }, 'Reading service settings…');
  }
  if (sv.error) return el('p', { class: 'error' }, `Service settings are unavailable: ${sv.error}`);
  const d = state.serviceDraft;
  const changed = Object.keys(d).filter((k) => d[k] !== sv.settings[k]);
  const save = async (patch) => {
    state.serviceBusy = true;
    render(true);
    try {
      // Only the daemon's reply is shown: a refused change keeps the
      // applied values on screen.
      const r = await call('set_service_settings', { patch, expectedRevision: sv.revision });
      state.service = r;
      state.serviceDraft = { ...r.settings };
      toast('Settings applied.');
    } catch {
      state.service = null;
    } finally {
      state.serviceBusy = false;
      render(true);
    }
  };
  const pause = el('input', {
    type: 'checkbox', id: 'svc-paused', checked: !!sv.settings.paused, disabled: state.serviceBusy,
    onchange: (e) => save({ paused: e.target.checked }),
  });
  const rows = SERVICE_FIELDS.map((f) => {
    const id = `svc-${f.key}`;
    const input = el('input', {
      type: 'number', id, min: f.min, max: f.max, step: 1, value: d[f.key],
      'aria-describedby': f.note ? `${id}-note` : null,
      oninput: (e) => { state.serviceDraft[f.key] = e.target.value === '' ? '' : Number(e.target.value); },
      onchange: () => render(true),
    });
    const isDefault = sv.settings[f.key] === sv.defaults[f.key];
    return el('div', { class: 'field' },
      el('label', { for: id }, f.label),
      el('div', { class: 'row' }, input, f.unit ? el('span', { class: 'note' }, f.unit) : null,
        isDefault ? null : el('span', { class: 'note' }, `default ${sv.defaults[f.key]}`)),
      f.note ? el('p', { class: 'note', id: `${id}-note` }, f.note) : null);
  });
  return el('div', {},
    el('div', { class: 'row' }, pause, el('label', { for: 'svc-paused' }, 'Pause background indexing')),
    el('p', { class: 'note' }, 'While paused, edits wait and projects are not shown as current. Searches and explicit index requests still run.'),
    el('div', { class: 'fields' }, rows),
    el('div', { class: 'row' },
      el('button', {
        class: 'btn', disabled: state.serviceBusy || changed.length === 0,
        onclick: () => save(Object.fromEntries(changed.map((k) => [k, state.serviceDraft[k]]))),
      }, 'Apply'),
      el('button', {
        class: 'btn subtle', disabled: state.serviceBusy || changed.length === 0,
        onclick: () => { state.serviceDraft = { ...sv.settings }; render(true); },
      }, 'Discard changes'),
      el('span', { class: 'note' }, `Revision ${sv.revision}`)));
}

function settingsView() {
  const a = state.autostart;
  if (!a) {
    invoke('autostart_status')
      .then((r) => { state.autostart = r; render(true); })
      .catch((e) => { state.autostart = { error: errText(e) }; render(true); });
    return el('section', {}, el('h1', {}, 'Settings'), el('p', { class: 'note' }, 'Reading settings…'));
  }
  const box = el('input', {
    type: 'checkbox', id: 'autostart', checked: !!a.enabled, disabled: !!a.error,
    onchange: async (e) => {
      try {
        state.autostart = await call('set_autostart', { enabled: e.target.checked });
        toast(state.autostart.enabled ? 'The index service will start when you sign in.' : 'The index service will no longer start at sign-in.');
      } catch { state.autostart = null; }
      render(true);
    },
  });
  return el('section', {},
    el('h1', {}, 'Settings'),
    el('h2', {}, 'Startup'),
    el('div', { class: 'row' }, box, el('label', { for: 'autostart' }, 'Start the index service when I sign in')),
    el('p', { class: 'note' }, 'Only the background service starts; this window does not open. Closing or quitting the window never stops the service.'),
    a.error ? el('p', { class: 'error' }, a.error) : null,
    a.enabled && !a.matchesThisInstall
      ? el('p', { class: 'error' }, `The sign-in entry points to another copy: ${a.command}. Turn the option off and on to use this one.`)
      : null,
    el('h2', {}, 'Updates'),
    updatesSection(),
    el('h2', {}, 'Index service'),
    serviceSettingsSection());
}

function updatesSection() {
  const u = state.updates;
  if (!u.last) {
    invoke('update_last')
      .then((r) => { u.last = r; render(true); })
      .catch(() => { u.last = {}; render(true); });
  }
  const c = u.check;
  const l = u.last ?? {};
  const busy = u.checking || u.starting || !!l.running;
  const auto = el('input', {
    type: 'checkbox', id: 'auto-update-check', checked: load(AUTO_CHECK_KEY) === '1',
    onchange: (e) => {
      store(AUTO_CHECK_KEY, e.target.checked ? '1' : '0');
      if (e.target.checked) autoCheck(true);
      render(true);
    },
  });
  let status = null;
  if (u.checking) status = el('p', { class: 'note', role: 'status' }, 'Checking GitHub for a newer release…');
  else if (c?.error) status = el('p', { class: 'error' }, `The update check failed: ${c.error}`);
  else if (c && c.updateAvailable) {
    status = el('div', {},
      el('p', { role: 'status' }, `WrongIndex ${c.latest} is available (this copy: ${c.current}).`),
      el('p', { class: 'note mono' }, c.release),
      c.installed
        ? el('button', { class: 'btn', disabled: busy, onclick: startUpdate }, `Update to ${c.latest}…`)
        : el('p', { class: 'note' }, 'This copy was not installed by the WrongIndex installer, so it cannot update itself. Install the release from GitHub instead.'));
  } else if (c) status = el('p', { role: 'status' }, `This is the latest release (${c.current}).`);
  let lastLine = null;
  if (l.running || u.starting) lastLine = el('p', { class: 'note', role: 'status' }, 'Updating… This window closes while the new version is installed and opens again afterwards.');
  else if (l.atMs && l.result) {
    const r = l.result;
    lastLine = el('p', { class: 'note' }, r.updated
      ? `Last update ${when(l.atMs)}: ${r.from} → ${r.to}${r.serviceRestarted ? ', service restarted' : ''}.`
      : `Last update ${when(l.atMs)}: already the latest release (${r.current}).`);
  } else if (l.atMs && l.error) lastLine = el('p', { class: 'error' }, `The update started ${when(l.atMs)} failed: ${l.error}`);
  else if (l.atMs && l.pending) lastLine = el('p', { class: 'note' }, `An update started ${when(l.atMs)} has no result yet; it may still be running.`);
  return el('div', {},
    el('p', {}, `This copy: WrongIndex ${l.desktopVersion ?? ''}`),
    el('div', { class: 'row' },
      el('button', { class: 'btn subtle', disabled: busy, onclick: checkUpdates }, 'Check for updates')),
    status,
    lastLine,
    el('div', { class: 'row' }, auto, el('label', { for: 'auto-update-check' }, 'Check for updates once a day')),
    el('p', { class: 'note' }, 'Checks ask GitHub (api.github.com) for the latest WrongIndex release; nothing else is sent. Off unless you turn it on. Updates are verified against the release\'s SHA-256 before anything is installed.'));
}

async function checkUpdates() {
  const u = state.updates;
  u.checking = true;
  render(true);
  try {
    u.check = await invoke('update_check');
    store(LAST_CHECK_KEY, String(Date.now()));
  } catch (e) {
    u.check = { error: errText(e) };
  }
  u.checking = false;
  render(true);
}

/** The daily check, when turned on. Quiet: only the header shows a result. */
async function autoCheck(force = false) {
  if (load(AUTO_CHECK_KEY) !== '1') return;
  if (!force && Date.now() - Number(load(LAST_CHECK_KEY) || 0) < DAY_MS) return;
  store(LAST_CHECK_KEY, String(Date.now()));
  try {
    state.updates.check = await invoke('update_check');
    render();
  } catch { /* shown on the next manual check */ }
}

async function startUpdate() {
  const c = state.updates.check;
  const ok = await confirmDialog(
    `Update to WrongIndex ${c.latest}?`,
    'The release is downloaded from GitHub and its SHA-256 checked before anything is installed. The index service stops while the files are replaced and starts again if it was running. This window closes and opens again.',
    'Update', false);
  if (!ok) return;
  state.updates.starting = true;
  render(true);
  try {
    await call('update_start');
    followUpdate();
  } catch {
    state.updates.starting = false;
    render(true);
  }
}

/** Until the window closes for the install, or the update fails before. */
function followUpdate() {
  setTimeout(async () => {
    try { state.updates.last = await invoke('update_last'); } catch { /* keep waiting */ }
    const l = state.updates.last;
    if (l && l.running) { followUpdate(); return; }
    state.updates.starting = false;
    if (l && l.error) toast(`The update failed: ${l.error}`, true);
    render(true);
  }, 2000);
}

/** Once after a window opens: the outcome of an update it has not shown. */
async function announceLastUpdate() {
  let l;
  try { l = await invoke('update_last'); } catch { return; }
  state.updates.last = l;
  if (!l.atMs || l.running || l.pending || String(l.atMs) === load(SEEN_UPDATE_KEY)) return;
  if (Date.now() - l.atMs > DAY_MS) return;
  store(SEEN_UPDATE_KEY, String(l.atMs));
  if (l.result && l.result.updated) toast(`WrongIndex was updated to ${l.result.to}.`);
  else if (l.error) toast(`The last update failed: ${l.error}`, true);
}

// ─── navigation ─────────────────────────────────────────────────────────────

function go(view) {
  state.view = view;
  if (view === 'settings') { state.autostart = null; state.service = null; state.updates.last = null; }
  if (view !== 'projects') { state.selected = null; state.detail = null; }
  render();
  document.getElementById('main').focus();
}

async function openProject(id) {
  state.selected = id;
  state.view = 'projects';
  await refresh();
}

let lastScreen = '';
/** `force`: re-render even while the user is typing in this screen. */
function render(force = false) {
  renderHeader();
  const active = document.activeElement;
  const typing = active && active.closest('#main') && (active.tagName === 'INPUT' || active.tagName === 'SELECT');
  const screenNow = `${state.view}:${state.selected ?? ''}`;
  // A live refresh must not wipe what the user is typing or choosing.
  if (!force && typing && screenNow === lastScreen) return;
  for (const b of document.querySelectorAll('.nav')) {
    if (b.dataset.view === state.view) b.setAttribute('aria-current', 'page');
    else b.removeAttribute('aria-current');
  }
  const views = { overview: overviewView, projects: projectsView, activity: activityView, diagnostics: diagnosticsView, settings: settingsView };
  const main = document.getElementById('main');
  // Keep the scroll position across live refreshes of the same screen only.
  const screen = `${state.view}:${state.selected ?? ''}`;
  const scroll = screen === lastScreen ? main.scrollTop : 0;
  lastScreen = screen;
  main.replaceChildren(views[state.view]());
  main.scrollTop = scroll;
  // A table that scrolls sideways (narrow window, large text) must be
  // reachable and scrollable from the keyboard.
  for (const t of main.querySelectorAll('table')) {
    if (t.scrollWidth > t.clientWidth) t.tabIndex = 0;
  }
}

async function startService() {
  toast('Starting the index service…');
  const r = await call('start_service');
  toast(r.started ? 'Index service started.' : 'Index service was already running.');
  await refresh();
}

async function stopService() {
  const ok = await confirmDialog(
    'Stop the index service?',
    'Indexing and watching stop for all projects after the current work drains. Other clients lose access until it is started again. Registrations and indexes are kept.',
    'Stop service');
  if (!ok) return;
  await call('stop_service');
  toast('Index service is stopping.');
  setTimeout(refresh, 800);
}

// ─── wiring ─────────────────────────────────────────────────────────────────

for (const b of document.querySelectorAll('.nav')) b.addEventListener('click', () => go(b.dataset.view));
document.getElementById('btn-start').addEventListener('click', startService);
document.getElementById('btn-stop').addEventListener('click', stopService);
document.getElementById('btn-update').addEventListener('click', () => go('settings'));

listen('wi://event', (e) => {
  state.activity.unshift(e.payload);
  if (state.activity.length > ACTIVITY_CAP) state.activity.length = ACTIVITY_CAP;
  scheduleRefresh();
});
listen('wi://resync', () => scheduleRefresh());
listen('wi://connection', () => scheduleRefresh());
listen('wi://confirm-stop', () => stopService());
listen('wi://check-updates', () => { go('settings'); checkUpdates(); });
listen('wi://settings', (e) => {
  if (e.payload && e.payload.error) { toast(errText(e.payload.error), true); return; }
  state.service = e.payload;
  state.serviceDraft = e.payload ? { ...e.payload.settings } : null;
  toast(e.payload && e.payload.settings.paused ? 'Background indexing paused.' : 'Background indexing resumed.');
  scheduleRefresh();
});

// Relative times and uptime stay honest without events.
setInterval(() => { if (!document.hidden) scheduleRefresh(); }, 15000);
refresh();
announceLastUpdate();
autoCheck();
setInterval(() => autoCheck(), 3600 * 1000);
