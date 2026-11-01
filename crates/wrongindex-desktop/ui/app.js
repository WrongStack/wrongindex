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
};
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

function confirmDialog(title, body, okLabel) {
  const d = document.getElementById('confirm');
  document.getElementById('confirm-title').textContent = title;
  document.getElementById('confirm-body').textContent = body;
  document.getElementById('confirm-ok').textContent = okLabel;
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
  if (!o) {
    conn.textContent = 'Connecting…';
    conn.className = 'conn';
    return;
  }
  if (o.connected) {
    const h = o.health;
    conn.textContent = h.draining ? 'Service stopping' : 'Service running';
    conn.className = h.draining ? 'conn down' : 'conn up';
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
    cards,
    attention.length ? el('h2', {}, 'Needs attention') : null,
    attention.length ? projectTable(attention) : null,
    el('h2', {}, 'Projects'),
    ps.length ? projectTable(ps) : firstUse());
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
    el('thead', {}, el('tr', {}, ...['Project', 'Policy', 'Freshness', 'Ownership', 'Activity', 'Watch', 'Generation', 'Pending', 'Last indexed', ''].map((h) => el('th', { scope: 'col' }, h)))),
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
  const journal = d.ownership?.journal ?? [];
  return el('section', {},
    el('button', { class: 'btn subtle back', onclick: () => { state.selected = null; state.detail = null; render(); } }, '← All projects'),
    el('h1', {}, p.displayName),
    el('div', { class: 'grid' },
      el('div', { class: 'k' }, 'Freshness'), el('div', {}, badge(s.freshness, FRESHNESS), s.reconcileReason ? ` (reconcile: ${s.reconcileReason})` : ''),
      el('div', { class: 'k' }, 'Ownership'), el('div', {}, badge(s.ownership, OWNERSHIP)),
      el('div', { class: 'k' }, 'Activity'), el('div', {}, s.activity),
      el('div', { class: 'k' }, 'Watch'), el('div', {}, `${s.watchHealth} · ${POLICY[p.watchPolicy] ?? p.watchPolicy} · ${s.sessions} session(s)`),
      el('div', { class: 'k' }, 'Generation'), el('div', {}, String(s.generation)),
      el('div', { class: 'k' }, 'Pending paths'), el('div', {}, String(s.pendingPaths)),
      el('div', { class: 'k' }, 'Last indexed'), el('div', {}, when(s.lastIndexedMs)),
      el('div', { class: 'k' }, 'Backend'), el('div', {}, p.backend),
      el('div', { class: 'k' }, 'Checkout'), el('div', { class: 'mono' }, p.checkoutRoot),
      el('div', { class: 'k' }, 'Index directory'), el('div', { class: 'mono' }, p.indexDir),
      el('div', { class: 'k' }, 'Configuration revision'), el('div', {}, String(p.configurationRevision)),
      s.lastError ? el('div', { class: 'k' }, 'Last error') : null,
      s.lastError ? el('div', { class: 'error' }, s.lastError) : null),
    el('h2', {}, 'Actions'),
    el('div', { class: 'row' },
      owned
        ? el('button', { class: 'btn subtle', onclick: act('release', null, 'Ownership released.') }, 'Release ownership')
        : el('button', { class: 'btn', onclick: act('claim', null, 'Ownership claimed.') }, 'Claim ownership'),
      el('button', { class: 'btn subtle', onclick: act('check', null, 'Health check queued.') }, 'Check index'),
      el('button', { class: 'btn subtle', disabled: !owned, title: owned ? null : 'Claim ownership first', onclick: act('reconcile', null, 'Reconciliation queued.') }, 'Reconcile'),
      el('button', { class: 'btn subtle', onclick: () => call('open_folder', { projectId: id }) }, 'Open folder'),
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
    searchPanel(p),
    el('h2', {}, 'Jobs'),
    jobsTable(d.jobs ?? []),
    journal.length ? el('h2', {}, 'Ownership history') : null,
    journal.length ? el('table', {},
      el('thead', {}, el('tr', {}, ...['When', 'Epoch', 'Phase', 'Previous owner', 'New owner'].map((h) => el('th', { scope: 'col' }, h)))),
      el('tbody', {}, journal.map((j) => el('tr', {},
        el('td', {}, when(j.atMs)), el('td', {}, String(j.epoch ?? '')),
        el('td', {}, j.phase ?? ''), el('td', { class: 'mono' }, j.oldOwner ?? '—'), el('td', { class: 'mono' }, j.newOwner ?? '—'))))) : null);
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
        el('thead', {}, el('tr', {}, ...['Symbol', 'Kind', 'Location', 'Signature'].map((h) => el('th', { scope: 'col' }, h)))),
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
    el('thead', {}, el('tr', {}, ...['Job', 'Kind', 'State', 'Admitted', 'Finished', 'Result', ''].map((h) => el('th', { scope: 'col' }, h)))),
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
  if (r.generation !== undefined) return `generation ${r.generation} · ${r.filesIndexed ?? '?'} files`;
  return '';
}

function activityView() {
  return el('section', {},
    el('h1', {}, 'Activity'),
    el('p', { class: 'note' }, `The last ${ACTIVITY_CAP} service events seen by this window. After a reconnect the views are re-read from the service; missed events are not replayed here.`),
    state.activity.length
      ? el('table', {},
        el('thead', {}, el('tr', {}, ...['#', 'Time', 'Event', 'Project', 'Details'].map((h) => el('th', { scope: 'col' }, h)))),
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

// ─── navigation ─────────────────────────────────────────────────────────────

function go(view) {
  state.view = view;
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
  const views = { overview: overviewView, projects: projectsView, activity: activityView, diagnostics: diagnosticsView };
  const main = document.getElementById('main');
  // Keep the scroll position across live refreshes of the same screen only.
  const screen = `${state.view}:${state.selected ?? ''}`;
  const scroll = screen === lastScreen ? main.scrollTop : 0;
  lastScreen = screen;
  main.replaceChildren(views[state.view]());
  main.scrollTop = scroll;
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

listen('wi://event', (e) => {
  state.activity.unshift(e.payload);
  if (state.activity.length > ACTIVITY_CAP) state.activity.length = ACTIVITY_CAP;
  scheduleRefresh();
});
listen('wi://resync', () => scheduleRefresh());
listen('wi://connection', () => scheduleRefresh());
listen('wi://confirm-stop', () => stopService());

// Relative times and uptime stay honest without events.
setInterval(() => { if (!document.hidden) scheduleRefresh(); }, 15000);
refresh();
