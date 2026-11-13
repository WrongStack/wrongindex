// Live WrongStack proof (WI-704): a real WrongStack surface - its
// codebase-index MCP server, built from a WrongStack checkout with the
// WrongIndex adapter - indexes and answers through a running wrongindexd.
//
//   node scripts/release/wrongstack-live.mjs <wrongindexd.exe> <wrongindex.exe> <WrongStack checkout>
//
// Everything runs on throw-away directories (data root, project copy, index).
// Prints one JSON object; exits non-zero when a check fails.
import { spawn, execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const [daemonExe, cliExe, wsDir] = process.argv.slice(2);
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'wi-live-'));
const home = path.join(work, 'home');
const proj = path.join(work, 'proj');
const idx = path.join(work, 'idx');
fs.cpSync(path.join(repo, 'tests', 'fixtures', 'projects', 'basic-ts'), proj, { recursive: true });

const out = { work };
const check = (name, ok, detail) => {
  out[name] = ok ? 'PASS' : `FAIL ${detail ?? ''}`;
  if (!ok) process.exitCode = 1;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const cli = (...args) =>
  JSON.parse(execFileSync(cliExe, ['--data-root', home, '--json', ...args], { encoding: 'utf8' }));

const daemon = spawn(daemonExe, ['--data-root', home, '--backend', 'native'], { stdio: 'ignore' });
let mcp;
try {
  const endpoint = path.join(home, 'runtime', 'endpoint.json');
  for (let i = 0; i < 300 && !fs.existsSync(endpoint); i++) await sleep(50);

  // WrongStack's MCP server, told to use the service.
  mcp = spawn(
    process.execPath,
    [path.join(wsDir, 'packages', 'codebase-index-mcp', 'dist', 'cli.js'), '--project-root', proj, '--index-dir', idx, '--writable'],
    {
      env: { ...process.env, WRONGSTACK_INDEX_SERVICE: 'wrongindex', WRONGINDEX_HOME: home },
      stdio: ['pipe', 'pipe', 'pipe'],
    },
  );
  let buf = '';
  const waiting = new Map();
  mcp.stdout.on('data', (d) => {
    buf += d.toString();
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      waiting.get(msg.id)?.(msg);
    }
  });
  let stderr = '';
  mcp.stderr.on('data', (d) => (stderr += d.toString()));
  let id = 0;
  const rpc = (method, params) =>
    new Promise((resolve, reject) => {
      const n = ++id;
      const t = setTimeout(() => reject(new Error(`${method} timed out; stderr: ${stderr.slice(-400)}`)), 120000);
      waiting.set(n, (m) => {
        clearTimeout(t);
        resolve(m);
      });
      mcp.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id: n, method, params })}\n`);
    });

  const init = await rpc('initialize', {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'wrongindex-live-proof', version: '1' },
  });
  out.server = init.result?.serverInfo;
  mcp.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
  const tools = await rpc('tools/list', {});
  out.tools = tools.result?.tools?.length;

  const text = (r) => (r.result?.content ?? []).map((c) => c.text ?? '').join('\n');
  const indexed = await rpc('tools/call', { name: 'codebase_index', arguments: {} });
  out.indexReply = text(indexed).slice(0, 200);
  check('indexedThroughWrongStack', !indexed.result?.isError, out.indexReply);

  // The service, not WrongStack, owns and wrote the index.
  const projects = cli('project', 'list').projects;
  const p = projects.find((x) => path.resolve(x.checkoutRoot).toLowerCase() === proj.toLowerCase());
  check('registeredByWrongStack', Boolean(p), JSON.stringify(projects));
  const status = cli('project', 'status', p.projectId);
  out.ownership = status.ownership;
  out.writtenBy = status.project.backend;
  out.generation = status.generation;
  check('ownedByService', status.ownership === 'owned', status.ownership);
  check('writtenByNative', status.project.backend === 'native', status.project.backend);
  const jobs = cli('job', 'list', '--project', p.projectId).jobs;
  out.jobs = jobs.map((j) => `${j.kind}:${j.state}`);
  check('jobRanInService', jobs.some((j) => j.state === 'succeeded' && j.kind !== 'check'), out.jobs.join(','));
  let held = false;
  try {
    fs.closeSync(fs.openSync(path.join(idx, '.wrongindex-owner.lock'), 'r'));
  } catch (e) {
    held = ['EBUSY', 'EPERM', 'EACCES'].includes(e.code);
  }
  check('lockHeld', held);

  // Reads through WrongStack are served by the service.
  const search = await rpc('tools/call', { name: 'codebase_search', arguments: { query: 'greet', limit: 5 } });
  out.searchReply = text(search).slice(0, 300);
  check('searchThroughWrongStack', /greet/.test(out.searchReply), out.searchReply);
  const publications = search.result?._meta?.['wrongindex/publications'];
  check('publicationMetadataThroughWrongStack', Array.isArray(publications) && publications.some((x) => x.projectId === p.projectId && x.generation >= 1 && x.daemonInstanceId), 'missing per-call publication metadata');
  const stats = await rpc('tools/call', { name: 'codebase_stats', arguments: {} });
  out.statsReply = text(stats).slice(0, 200);
  check('statsThroughWrongStack', !stats.result?.isError && /ts/.test(out.statsReply), out.statsReply);
  const metrics = cli('metrics');
  out.serviceJobs = metrics.jobs;
} catch (e) {
  out.error = String(e?.stack ?? e);
  process.exitCode = 1;
} finally {
  mcp?.kill();
  try {
    execFileSync(cliExe, ['--data-root', home, 'daemon', 'stop', '--wait-seconds', '20'], { stdio: 'ignore' });
  } catch {}
  daemon.kill();
}

// Phase 2: WrongIndex gone. The same WrongStack surface, still configured
// for the service, works on the same index with its own engine (the lock
// file the service left behind is stale and does not count).
try {
  await sleep(1500);
  out.staleLockPresent = fs.existsSync(path.join(idx, '.wrongindex-owner.lock'));
  fs.appendFileSync(path.join(proj, 'src', 'greet.ts'), '\nexport function afterService(): number { return 7; }\n');
  const r = await mcpSession(
    { WRONGSTACK_INDEX_SERVICE: 'wrongindex', WRONGINDEX_HOME: home },
    async (rpc, text) => {
      const indexed = await rpc('tools/call', { name: 'codebase_index', arguments: {} });
      const search = await rpc('tools/call', { name: 'codebase_search', arguments: { query: 'afterService', limit: 5 } });
      return { index: text(indexed).slice(0, 160), search: text(search).slice(0, 200), isError: indexed.result?.isError };
    },
  );
  out.withoutService = r;
  check('worksWithoutWrongIndex', !r.isError && /afterService/.test(r.search), JSON.stringify(r));
} catch (e) {
  out.error2 = String(e?.stack ?? e);
  process.exitCode = 1;
}
console.log(JSON.stringify(out, null, 2));
// The local engine may leave its detached per-project server running.
process.exit();

async function mcpSession(env, body) {
  const child = spawn(
    process.execPath,
    [path.join(wsDir, 'packages', 'codebase-index-mcp', 'dist', 'cli.js'), '--project-root', proj, '--index-dir', idx, '--writable'],
    { env: { ...process.env, ...env }, stdio: ['pipe', 'pipe', 'pipe'] },
  );
  let b = '';
  let err = '';
  const waiting = new Map();
  child.stdout.on('data', (d) => {
    b += d.toString();
    let nl;
    while ((nl = b.indexOf('\n')) >= 0) {
      const line = b.slice(0, nl).trim();
      b = b.slice(nl + 1);
      if (line) {
        const m = JSON.parse(line);
        waiting.get(m.id)?.(m);
      }
    }
  });
  child.stderr.on('data', (d) => (err += d.toString()));
  let n = 0;
  const rpc = (method, params) =>
    new Promise((resolve, reject) => {
      const k = ++n;
      const t = setTimeout(() => reject(new Error(`${method} timed out; stderr: ${err.slice(-400)}`)), 120000);
      waiting.set(k, (m) => {
        clearTimeout(t);
        resolve(m);
      });
      child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id: k, method, params })}\n`);
    });
  const text = (r) => (r.result?.content ?? []).map((c) => c.text ?? '').join('\n');
  try {
    await rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'wrongindex-live-proof', version: '1' } });
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
    return await body(rpc, text);
  } finally {
    child.kill();
  }
}
