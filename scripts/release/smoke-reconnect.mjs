// Reconnect smoke (WI-404): the desktop follows a service that stops and
// comes back while the project's state changed underneath it (its checkout
// root vanished). After reconnecting, the GUI shows the new truth
// (unavailable) and never the stale "current" - missed events never turn
// into false success.
//
//   node scripts/release/smoke-reconnect.mjs <playwright-core dir> <target dir> [cdp port]
//
// <target dir> holds wrongindexd.exe, wrongindex.exe and wrongindex-desktop.exe
// (e.g. target/release). Throw-away data root and project; the desktop
// window opens for the duration of the run. Prints one JSON object; exits
// non-zero when a check fails.
import { spawn, execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const [pwDir, targetDir, portArg] = process.argv.slice(2);
if (!pwDir || !targetDir) {
  console.error('usage: smoke-reconnect.mjs <playwright-core dir> <target dir> [cdp port]');
  process.exit(2);
}
const port = Number(portArg ?? 9341);
const { chromium } = await import(pathToFileURL(path.join(pwDir, 'index.mjs')).href);
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const work = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'wi-reconnect-')));
const home = path.join(work, 'home');
const proj = path.join(work, 'proj');
fs.cpSync(path.join(repo, 'tests', 'fixtures', 'projects', 'config-only'), proj, { recursive: true });
const exe = (n) => path.join(targetDir, n);
const cli = (...a) => execFileSync(exe('wrongindex.exe'), ['--data-root', home, ...a], { encoding: 'utf8' });
const cliJson = (...a) => JSON.parse(cli('--json', ...a));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const out = { work };
const check = (name, ok, detail) => {
  out[name] = ok ? 'PASS' : `FAIL ${detail ?? ''}`.trim();
  if (!ok) process.exitCode = 1;
};
async function until(what, ms, probe) {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await probe();
    if (v) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(200);
  }
}

let desktop;
let browser;
try {
  cli('daemon', 'start', '--wait-seconds', '20');
  const pid = cliJson('project', 'add', proj, '--index-dir', path.join(work, 'idx'), '--watch', 'pinned').project.projectId;
  await until('the project to become current', 90000, () => cliJson('project', 'status', pid).freshness === 'current');

  desktop = spawn(exe('wrongindex-desktop.exe'), [], {
    env: {
      ...process.env,
      WRONGINDEX_HOME: home,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}`,
    },
    stdio: 'ignore',
  });
  browser = await until('the WebView', 30000, async () => {
    try {
      return await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
    } catch {
      return null;
    }
  });
  const page = await until('the page', 15000, () =>
    browser.contexts().flatMap((c) => c.pages()).find((p) => !p.url().startsWith('devtools')),
  );
  const conn = () => page.textContent('#conn');
  const row = async () => (await page.locator('tbody tr').first().textContent().catch(() => '')) ?? '';

  await page.waitForFunction(() => /Service running/.test(document.getElementById('conn')?.textContent ?? ''), null, { timeout: 30000 });
  await page.click('.nav[data-view=projects]');
  await page.waitForSelector('tbody tr', { timeout: 15000 });
  out.rowBefore = (await row()).replace(/\s+/g, ' ').trim();
  check('currentBefore', /Current/.test(out.rowBefore), out.rowBefore);

  // The service goes away: the GUI says so.
  cli('daemon', 'stop', '--wait-seconds', '30');
  await until('the GUI to see the service stopped', 30000, async () => /Service not running/.test(await conn()));
  check('stoppedShown', true);

  // While nobody watches, the checkout vanishes; the service comes back
  // (a new instance: everything before is history the GUI never saw).
  fs.rmSync(proj, { recursive: true, force: true });
  cli('daemon', 'start', '--wait-seconds', '20');
  await until('the GUI to reconnect', 30000, async () => /Service running/.test(await conn()));
  const seen = [];
  let unavailable = false;
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    const r = (await row()).replace(/\s+/g, ' ').trim();
    seen.push(r);
    if (/Unavailable/.test(r)) {
      unavailable = true;
      break;
    }
    await sleep(200);
  }
  // Keep watching a little: the state must not flip back.
  for (let i = 0; i < 10; i++) {
    seen.push((await row()).replace(/\s+/g, ' ').trim());
    await sleep(200);
  }
  out.rowAfter = seen.at(-1);
  check('unavailableAfterReconnect', unavailable, out.rowAfter);
  check('neverCurrentAfterReconnect', !seen.some((r) => /Current/.test(r)), seen.find((r) => /Current/.test(r)));
  const st = cliJson('project', 'status', pid);
  out.serviceFreshness = st.freshness;
  check('serviceAgrees', st.freshness === 'unavailable', st.freshness);
  await page.screenshot({ path: path.join(work, 'reconnect-after.png') });
} catch (e) {
  out.error = String(e?.stack ?? e);
  process.exitCode = 1;
} finally {
  await browser?.close().catch(() => {});
  if (desktop) {
    try {
      execFileSync('taskkill', ['/PID', String(desktop.pid), '/T', '/F'], { stdio: 'ignore' });
    } catch {}
  }
  try {
    cli('daemon', 'stop', '--wait-seconds', '30');
  } catch {}
}
console.log(JSON.stringify(out, null, 2));
process.exit();
