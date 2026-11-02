// Live WrongStack CLI + WebUI proof (WI-704): the WrongStack CLI, started
// with `--webui` and no model (`--provider wrongstack-setup --model
// no-api-key`), from a WrongStack checkout with the WrongIndex adapter, runs
// its codebase indexing through a running wrongindexd: the session-start
// index, the WebUI's explicit index request, the CodeMap graphs (with SAGE
// memory decoration) and the CLI's own file watcher (targeted runs).
//
//   node scripts/release/wrongstack-live-webui.mjs <wrongindexd.exe> <wrongindex.exe> <WrongStack checkout>
//
// Everything runs on throw-away directories (WrongIndex data root, WrongStack
// home, project copy). The WebUI session token is used for the requests only
// and never printed. Prints one JSON object; exits non-zero when a check fails.
import { spawn, execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as net from 'node:net';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const [daemonExe, cliExe, wsDir] = process.argv.slice(2);
if (!daemonExe || !cliExe || !wsDir) {
  console.error('usage: wrongstack-live-webui.mjs <wrongindexd.exe> <wrongindex.exe> <WrongStack checkout>');
  process.exit(2);
}
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const work = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'wi-live-webui-')));
const home = path.join(work, 'home');
const wsHome = path.join(work, 'wshome');
const proj = path.join(work, 'proj');
fs.mkdirSync(wsHome);
fs.cpSync(path.join(repo, 'tests', 'fixtures', 'projects', 'basic-ts'), proj, { recursive: true });

const out = { work };
const check = (name, ok, detail) => {
  out[name] = ok ? 'PASS' : `FAIL ${detail ?? ''}`.trim();
  if (!ok) process.exitCode = 1;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const cli = (...args) =>
  JSON.parse(execFileSync(cliExe, ['--data-root', home, '--json', ...args], { encoding: 'utf8' }));
const freePort = () =>
  new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
async function until(what, ms, probe) {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await probe();
    if (v) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(250);
  }
}
const killTree = (pid) => {
  try {
    execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' });
  } catch {}
};

const daemon = spawn(daemonExe, ['--data-root', home, '--backend', 'native'], { stdio: 'ignore' });
let ws;
try {
  await until('the service', 15000, () => fs.existsSync(path.join(home, 'runtime', 'endpoint.json')));
  const port = await freePort();
  ws = spawn(
    process.execPath,
    [
      path.join(wsDir, 'packages', 'cli', 'dist', 'index.js'),
      '--webui',
      '--port',
      String(port),
      '--cwd',
      proj,
      '--provider',
      'wrongstack-setup',
      '--model',
      'no-api-key',
    ],
    {
      cwd: proj,
      env: {
        ...process.env,
        WRONGSTACK_HOME: wsHome,
        WRONGSTACK_INDEX_SERVICE: 'wrongindex',
        WRONGINDEX_HOME: home,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  let log = '';
  ws.stdout.on('data', (d) => (log += d.toString()));
  ws.stderr.on('data', (d) => (log += d.toString()));
  const token = await until('the WebUI', 90000, () => /WebUI running[^\n]*token=([0-9a-f]+)/.exec(log)?.[1]);
  const base = `http://127.0.0.1:${port}`;
  const api = async (p, init = {}) => {
    const r = await fetch(`${base}${p}`, { ...init, headers: { 'X-WS-Token': token, ...(init.headers ?? {}) } });
    return { status: r.status, body: await r.json().catch(() => null) };
  };

  // 1. The CLI registered the project with the service; the session-start
  //    index ran there, natively, and the service holds the index lock.
  const project = await until('registration', 60000, () =>
    cli('project', 'list').projects.find((x) => path.resolve(x.checkoutRoot).toLowerCase() === proj.toLowerCase()),
  );
  const pid = project.projectId;
  const jobs = () => cli('job', 'list', '--project', pid).jobs;
  const first = await until('the session-start index', 60000, () =>
    jobs().find((j) => j.state === 'succeeded' && j.kind === 'full'),
  );
  check('sessionStartIndexInService', first.result?.backend === 'native', JSON.stringify(first.result));
  const status = cli('project', 'status', pid);
  check('ownedByService', status.ownership === 'owned', status.ownership);
  let held = false;
  try {
    fs.closeSync(fs.openSync(path.join(project.indexDir, '.wrongindex-owner.lock'), 'r'));
  } catch (e) {
    held = ['EBUSY', 'EPERM', 'EACCES'].includes(e.code);
  }
  check('lockHeld', held);

  // 2. The WebUI's explicit index request goes to the service.
  const before = jobs().length;
  const started = await api('/api/codemap/index', { method: 'POST' });
  check('webuiIndexAccepted', started.status === 202, String(started.status));
  const done = await until('the WebUI index', 60000, async () => {
    const s = await api('/api/codemap/status');
    return s.body?.status && s.body.status !== 'indexing' ? s.body : null;
  });
  out.webuiIndexResult = done.result;
  check('webuiIndexReady', done.status === 'ready', JSON.stringify(done).slice(0, 300));
  check('webuiIndexRanInService', jobs().length > before, `${before} -> ${jobs().length}`);
  check(
    'noOpReportsSymbols',
    done.result?.symbolsIndexed > 0 && done.result?.filesIndexed === 0,
    JSON.stringify(done.result),
  );

  // 3. CodeMap graphs, read from the service-owned index, decorated with a
  //    SAGE memory anchored to a file (the CLI's own SAGE store is there too).
  const mem = path.join(proj, '.wrongstack', 'memories');
  fs.mkdirSync(mem, { recursive: true });
  fs.appendFileSync(
    path.join(mem, 'memories.jsonl'),
    `${JSON.stringify({ id: 'live-1', kind: 'fact', text: 'greet builds the greeting', anchors: [{ path: 'src/greet.ts', symbol: 'greet' }] })}\n`,
  );
  const packages = await api('/api/codemap/packages');
  check('packageGraph', packages.status === 200 && packages.body?.nodes?.length > 0, JSON.stringify(packages).slice(0, 200));
  const pkg = packages.body?.nodes?.[0]?.package;
  const files = await api(`/api/codemap/files?package=${encodeURIComponent(pkg ?? '')}`);
  check('fileGraph', files.status === 200 && files.body?.nodes?.length > 0, JSON.stringify(files).slice(0, 300));
  const greetFile = files.body?.nodes?.find((n) => n.label === 'greet.ts');
  check('fileGraphMemory', greetFile?.memoryCount === 1, JSON.stringify(greetFile));
  const symbols = await api(`/api/codemap/symbols?file=${encodeURIComponent(path.join(proj, 'src', 'greet.ts'))}`);
  check('symbolGraph', symbols.status === 200 && symbols.body?.nodes?.length > 0, JSON.stringify(symbols).slice(0, 300));
  const greetSym = symbols.body?.nodes?.find((n) => n.label === 'greet');
  check('symbolGraphMemory', greetSym?.memoryCount === 1, JSON.stringify(greetSym));

  // 4. An edit: the CLI's watcher sends the path; the service runs a
  //    targeted native update and the graph shows the new symbol.
  const jobsBefore = jobs().length;
  fs.appendFileSync(path.join(proj, 'src', 'greet.ts'), '\nexport function liveAddition(): number { return 42; }\n');
  const edit = await until('the watcher job', 60000, () =>
    jobs()
      .slice(0, Math.max(0, jobs().length - jobsBefore))
      .find((j) => j.state === 'succeeded' && j.kind === 'incremental'),
  );
  out.watcherJob = { paths: edit.params?.paths?.length, native: edit.result?.native };
  check('watcherJobTargeted', edit.result?.native?.targets >= 1 && edit.result?.native?.outcome === 'updated', JSON.stringify(out.watcherJob));
  const after = await until('the new symbol', 30000, async () => {
    const s = await api(`/api/codemap/symbols?file=${encodeURIComponent(path.join(proj, 'src', 'greet.ts'))}`);
    return s.body?.nodes?.some((n) => n.label === 'liveAddition') ? s : null;
  });
  check('editVisibleInWebUI', Boolean(after));
  out.serviceJobs = cli('metrics').jobs;
} catch (e) {
  out.error = String(e?.stack ?? e);
  process.exitCode = 1;
} finally {
  if (ws) killTree(ws.pid);
  try {
    execFileSync(cliExe, ['--data-root', home, 'daemon', 'stop', '--wait-seconds', '20'], { stdio: 'ignore' });
  } catch {}
  daemon.kill();
}
console.log(JSON.stringify(out, null, 2));
process.exit();
