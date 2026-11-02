// Drives the INSTALLED desktop's WebView2 over CDP (WI-408).
//
//   node scripts/release/smoke-gui.mjs <playwright-core dir> <cdp port> <project id> <screenshot dir>
//
// The desktop must run with
//   WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>
// Prints one JSON line with the observations; exits non-zero on a failed check.
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';

const [pwDir, port, projectId, shots] = process.argv.slice(2);
const { chromium } = await import(pathToFileURL(join(pwDir, 'index.mjs')).href);

const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
const pages = browser.contexts().flatMap((c) => c.pages());
const page = pages.find((p) => p.url().includes('tauri.localhost')) ?? pages.find((p) => !p.url().startsWith('devtools'));
if (!page) throw new Error('no WebView page');
const out = { url: page.url() };
const check = (name, ok, detail) => {
  out[name] = ok ? 'PASS' : `FAIL ${detail ?? ''}`;
  if (!ok) process.exitCode = 1;
};

// The installed GUI talks to the installed service.
await page.waitForFunction(() => /Service running|Background indexing paused/.test(document.getElementById('conn')?.textContent ?? ''), null, { timeout: 30000 });
out.header = await page.textContent('#conn');
out.meta = await page.textContent('#service-meta');
check('connected', /Service running/.test(out.header), out.header);

// The registered project is listed, and its detail opens.
await page.click('.nav[data-view=projects]');
await page.waitForSelector('tbody tr', { timeout: 15000 });
out.projectRows = await page.locator('tbody tr').count();
check('projectListed', out.projectRows >= 1, out.projectRows);
await page.locator('tbody tr').first().click();
await page.waitForSelector('button.back', { timeout: 15000 });
out.detailTitle = await page.textContent('main h1');
out.writtenBy = await page.locator('.grid').textContent();
check('detailOpened', out.detailTitle === 'proj', out.detailTitle);
check('writtenByNative', /WrongIndex native engine/.test(out.writtenBy), 'backend label');
delete out.writtenBy;
await page.screenshot({ path: join(shots, 'installed-detail.png') });

// Pause through the GUI; the service applies it (checked again by the CLI).
await page.click('.nav[data-view=settings]');
await page.waitForSelector('#svc-paused', { timeout: 15000 });
await page.check('#svc-paused');
await page.waitForFunction(() => /Revision [1-9]/.test(document.querySelector('main')?.textContent ?? ''), null, { timeout: 15000 });
await page.waitForFunction(() => /Background indexing paused/.test(document.getElementById('conn')?.textContent ?? ''), null, { timeout: 30000 });
out.headerAfterPause = await page.textContent('#conn');
check('pausedFromGui', /paused/.test(out.headerAfterPause), out.headerAfterPause);

// A value the service refuses is not shown as applied.
await page.fill('#svc-parallelJobs', '99');
await page.locator('#svc-parallelJobs').dispatchEvent('change');
await page.click('button:has-text("Apply")');
await page.waitForSelector('.toast.bad', { timeout: 15000 });
out.refusal = await page.textContent('.toast.bad');
await page.waitForTimeout(500);
out.parallelShown = await page.inputValue('#svc-parallelJobs');
check('refusedNotApplied', out.parallelShown === '2', out.parallelShown);
await page.screenshot({ path: join(shots, 'installed-settings.png') });

// Resume again.
await page.uncheck('#svc-paused');
await page.waitForFunction(() => /Service running/.test(document.getElementById('conn')?.textContent ?? ''), null, { timeout: 30000 });
out.projectId = projectId;
console.log(JSON.stringify(out));
await browser.close();
