// Native profile certification run (WI-514 / WI-705).
//
//   node scripts/release/certify.mjs [--out <dir>] [--only G06,G08]
//
// Runs every gate of docs/certification.md in order and records, per gate,
// PASS / FAIL / NOT RUN (with the missing prerequisite) - never a silent
// skip. Inputs come from the environment:
//
//   WRONGINDEX_TEST_ENGINE    prepared WrongStack engine (scripts/oracle)
//   WRONGINDEX_DIFF_PROJECTS  ';'-separated real Git checkouts
//   WRONGINDEX_BENCH_PROJECT  a large Git checkout (publication benchmark)
//   WRONGINDEX_SOAK_SECS      soak duration
//   WRONGINDEX_PARSER_CORPUS  parser known-answer corpus file
//   WRONGINDEX_VET_DIRS       ';'-separated checkouts for the Git containment vet
//   WRONGSTACK_CHECKOUT       WrongStack checkout with the adapter, built (dist)
//   PLAYWRIGHT_CORE           directory holding playwright-core's index.mjs
//   WRONGINDEX_INSTALLER      installer (default: target/release/bundle/nsis/*-setup.exe)
//
// Writes certification.json, certification.md and one log per gate to the
// output directory (default target/certification/<commit>-<time>). Exits
// non-zero when a gate fails. Logs hold command output only (no tokens:
// the live scripts never print theirs).
import { spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const argv = process.argv.slice(2);
const opt = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
};
const only = opt('--only')?.split(',');
const env = process.env;
const run = (cmd, args, extraEnv = {}, cwd = repo) =>
  spawnSync(cmd, args, {
    cwd,
    env: { ...env, ...extraEnv },
    encoding: 'utf8',
    maxBuffer: 512 * 1024 * 1024,
    shell: false,
    windowsHide: true,
  });
const first = (cmd, args) => {
  const r = run(cmd, args);
  return r.status === 0 ? (r.stdout || r.stderr).trim().split('\n')[0] : null;
};

const commit = first('git', ['rev-parse', 'HEAD']) ?? 'unknown';
const dirty = (run('git', ['status', '--porcelain']).stdout ?? '').trim().length > 0;
const stamp = new Date().toISOString().replace(/[:.]/g, '-');
const outDir = path.resolve(opt('--out') ?? path.join(repo, 'target', 'certification', `${commit.slice(0, 12)}-${stamp}`));
fs.mkdirSync(outDir, { recursive: true });
const release = path.join(env.CARGO_TARGET_DIR ?? path.join(repo, 'target'), 'release');
const engine = env.WRONGINDEX_TEST_ENGINE;
const wsCheckout = env.WRONGSTACK_CHECKOUT;
const installer =
  env.WRONGINDEX_INSTALLER ??
  (() => {
    const d = path.join(release, 'bundle', 'nsis');
    const f = fs.existsSync(d) ? fs.readdirSync(d).find((n) => n.endsWith('-setup.exe')) : undefined;
    return f ? path.join(d, f) : undefined;
  })();

const versions = {
  commit,
  dirtyWorktree: dirty,
  rustc: first('rustc', ['-V']),
  cargo: first('cargo', ['-V']),
  bun: first('bun', ['--version']),
  node: first('node', ['--version']),
  python: first('python', ['--version']),
  go: first('go', ['version']),
  git: first('git', ['--version']),
  wrongstackEngine: engine ? path.basename(engine) : null,
  wrongstackCheckout: wsCheckout ? first('git', ['-C', wsCheckout, 'rev-parse', 'HEAD']) : null,
  wrongstackBranch: wsCheckout ? first('git', ['-C', wsCheckout, 'branch', '--show-current']) : null,
};

/** A prerequisite: null when present, else what is missing. */
const need = (...checks) => checks.find((c) => c) ?? null;
const envVar = (n) => (env[n] ? null : `${n} not set`);
const file = (p, what) => (p && fs.existsSync(p) ? null : `${what} not found`);
const tool = (cmd, args = ['--version']) => (run(cmd, args).status === 0 ? null : `${cmd} not available`);
const cargoTest = (args, extraEnv) => ({ cmd: 'cargo', args: ['test', '--locked', ...args], env: extraEnv });
const engineEnv = engine ? { WRONGINDEX_TEST_ENGINE: engine } : {};

const gates = [
  { id: 'G01', name: 'Formatting', req: 'AGENTS.md checks', cmd: 'cargo', args: ['fmt', '--all', '--', '--check'] },
  { id: 'G02', name: 'Clippy, warnings denied', req: 'AGENTS.md checks', cmd: 'cargo', args: ['clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings'] },
  { id: 'G03', name: 'Workspace tests (oracle parity, native parity, daemon, registry, IPC, desktop bridge)', req: 'WI-108, WI-502..511, WI-601, WI-603', ...cargoTest(['--workspace']) },
  { id: 'G04', name: 'Daemon IPC/lifecycle tests, five consecutive runs', req: 'AGENTS.md (IPC/lifecycle)', repeat: 5, ...cargoTest(['-p', 'wrongindexd', '--tests']) },
  { id: 'G05', name: 'Release build (service, CLI, desktop)', req: 'WI-700', cmd: 'cargo', args: ['build', '--release', '--locked', '-p', 'wrongindexd', '-p', 'wrongindex-cli', '-p', 'wrongindex-desktop'] },
  { id: 'G06', name: 'Native writer vs WrongStack: generated projects, every language, incremental, targeted, enrichment, SAGE, graph decoration', req: 'WI-503..512, WI-515', needs: () => need(envVar('WRONGINDEX_TEST_ENGINE'), tool('bun'), tool('python'), tool('go', ['version'])), ...cargoTest(['--release', '-p', 'wrongindex-engine', '--test', 'differential', '--', '--ignored'], engineEnv) },
  { id: 'G07', name: 'Native writer vs WrongStack on real projects (full and incremental)', req: 'WI-514', needs: () => need(envVar('WRONGINDEX_TEST_ENGINE'), envVar('WRONGINDEX_DIFF_PROJECTS')), ...cargoTest(['--release', '-p', 'wrongindex-engine', '--test', 'differential', '--', '--ignored', 'on_listed_projects', '--nocapture'], engineEnv) },
  { id: 'G08', name: 'Query differential through the daemon (671 recorded operations)', req: 'WI-302, WI-511', needs: () => need(envVar('WRONGINDEX_TEST_ENGINE'), tool('bun')), ...cargoTest(['-p', 'wrongindexd', '--test', 'query_diff', '--', '--ignored'], engineEnv) },
  { id: 'G09', name: 'Compatibility worker paths (engine, watch, backend switch)', req: 'WI-300..302, WI-600', needs: () => need(envVar('WRONGINDEX_TEST_ENGINE'), tool('bun')), ...cargoTest(['-p', 'wrongindexd', '--test', 'engine', '--test', 'watch', '--test', 'backend_switch', '--', '--ignored'], engineEnv) },
  { id: 'G10', name: 'Large-run publication benchmark', req: 'WI-513', needs: () => envVar('WRONGINDEX_BENCH_PROJECT'), ...cargoTest(['--release', '-p', 'wrongindexd', '--test', 'publication_bench', '--', '--ignored', '--nocapture']) },
  { id: 'G11', name: 'Multi-project soak', req: 'WI-602', needs: () => envVar('WRONGINDEX_SOAK_SECS'), ...cargoTest(['--release', '-p', 'wrongindexd', '--test', 'soak', '--', '--ignored', '--nocapture']) },
  { id: 'G12', name: 'Parser corpus known answers', req: 'WI-501', needs: () => envVar('WRONGINDEX_PARSER_CORPUS'), ...cargoTest(['--release', '-p', 'wrongindex-engine', '--lib', '--', '--ignored', 'parser_ports_match_a_real_world_corpus']) },
  { id: 'G13', name: 'Git containment vet on real checkouts', req: 'WI-604', needs: () => envVar('WRONGINDEX_VET_DIRS'), ...cargoTest(['-p', 'wrongindex-storage', '--lib', '--', '--ignored', 'real_checkouts_pass']) },
  { id: 'G14', name: 'Discovery differential (1680-case matrix, fixtures)', req: 'WI-205', needs: () => need(envVar('WRONGINDEX_TEST_ENGINE'), tool('bun'), file(path.join(release, 'wrongindex.exe'), 'release CLI')), cmd: 'bun', args: ['scripts/oracle/discovery-diff.ts', '--wrongstack', engine ?? '', '--cli', path.join(release, 'wrongindex.exe')] },
  { id: 'G15', name: 'WrongStack adapter tests incl. end to end through the service', req: 'WI-303..309', needs: () => need(envVar('WRONGSTACK_CHECKOUT'), file(path.join(release, 'wrongindexd.exe'), 'release service')), cmd: 'npx', args: ['vitest', 'run', 'tests/wrongindex-adapter.test.ts'], cwd: wsCheckout ? path.join(wsCheckout, 'packages', 'tools') : undefined, env: { WRONGINDEX_TEST_DAEMON: path.join(release, 'wrongindexd.exe') }, shell: true },
  { id: 'G16', name: 'Live WrongStack MCP proof', req: 'WI-704', needs: () => need(envVar('WRONGSTACK_CHECKOUT'), file(path.join(release, 'wrongindexd.exe'), 'release service')), cmd: 'node', args: ['scripts/release/wrongstack-live.mjs', path.join(release, 'wrongindexd.exe'), path.join(release, 'wrongindex.exe'), wsCheckout ?? ''] },
  { id: 'G17', name: 'Live WrongStack CLI + WebUI proof', req: 'WI-704', needs: () => need(envVar('WRONGSTACK_CHECKOUT'), file(path.join(release, 'wrongindexd.exe'), 'release service')), cmd: 'node', args: ['scripts/release/wrongstack-live-webui.mjs', path.join(release, 'wrongindexd.exe'), path.join(release, 'wrongindex.exe'), wsCheckout ?? ''] },
  { id: 'G18', name: 'Desktop reconnect smoke (missed events, no false success)', req: 'WI-404', needs: () => need(envVar('PLAYWRIGHT_CORE'), file(path.join(release, 'wrongindex-desktop.exe'), 'release desktop')), cmd: 'node', args: ['scripts/release/smoke-reconnect.mjs', env.PLAYWRIGHT_CORE ?? '', release] },
  { id: 'G19', name: 'Installed package smoke (install, service, CLI, GUI, uninstall)', req: 'WI-408, WI-700, WI-701', needs: () => need(envVar('PLAYWRIGHT_CORE'), tool('pwsh', ['-NoProfile', '-Command', 'exit 0']), file(installer, 'installer (scripts/release/package-windows.ps1)')), cmd: 'pwsh', args: ['-NoProfile', '-File', 'scripts/release/smoke-installed.ps1', '-PlaywrightCore', env.PLAYWRIGHT_CORE ?? '', ...(installer ? ['-Installer', installer] : [])] },
];

const results = [];
for (const g of gates) {
  if (only && !only.includes(g.id)) continue;
  const missing = g.needs?.() ?? null;
  const log = path.join(outDir, `${g.id}.log`);
  if (missing) {
    results.push({ id: g.id, name: g.name, req: g.req, status: 'NOT RUN', reason: missing });
    console.error(`${g.id} NOT RUN (${missing})`);
    continue;
  }
  const t = Date.now();
  let status = 'PASS';
  let detail = '';
  let text = '';
  for (let i = 0; i < (g.repeat ?? 1); i++) {
    const r = spawnSync(g.cmd, g.args, {
      cwd: g.cwd ?? repo,
      env: { ...env, ...(g.env ?? {}) },
      encoding: 'utf8',
      maxBuffer: 512 * 1024 * 1024,
      shell: g.shell ?? false,
      windowsHide: true,
    });
    const out = `${r.stdout ?? ''}${r.stderr ?? ''}`;
    text += `\n=== run ${i + 1}: ${g.cmd} ${g.args.join(' ')} (exit ${r.status}) ===\n${out}`;
    const tests = [...out.matchAll(/test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored/g)];
    if (tests.length) {
      const passed = tests.reduce((a, m) => a + Number(m[1]), 0);
      const failed = tests.reduce((a, m) => a + Number(m[2]), 0);
      detail += `${detail ? '; ' : ''}${g.repeat ? `run ${i + 1}: ` : ''}${passed} passed, ${failed} failed`;
    }
    if (r.status !== 0) {
      status = 'FAIL';
      detail += `${detail ? '; ' : ''}exit ${r.status ?? r.error?.message}`;
      break;
    }
    // A gate that ran but reported its own prerequisite missing did not run.
    if (/^NOT RUN/m.test(out)) {
      status = 'NOT RUN';
      detail += `${detail ? '; ' : ''}${out.match(/^NOT RUN.*$/m)[0]}`;
    }
  }
  fs.writeFileSync(log, text);
  const secs = Math.round((Date.now() - t) / 100) / 10;
  results.push({ id: g.id, name: g.name, req: g.req, status, detail, seconds: secs, log: path.basename(log) });
  console.error(`${g.id} ${status} ${detail} (${secs} s)`);
}

const manual = [
  { id: 'M01', name: 'Screen reader pass over every desktop screen', req: 'WI-407', status: 'NOT RUN', reason: 'manual; needs a person with NVDA/Narrator' },
  { id: 'M02', name: 'Native per-monitor DPI pass on a mixed-DPI setup', req: 'WI-407', status: 'NOT RUN', reason: 'manual; needs mixed-DPI hardware' },
  { id: 'M03', name: 'Code signing of the installer and binaries', req: 'WI-703', status: 'NOT RUN', reason: 'needs the publisher certificate' },
];
const summary = {
  generatedAt: new Date().toISOString(),
  versions,
  gates: results,
  manual,
  counts: Object.fromEntries(['PASS', 'FAIL', 'NOT RUN'].map((s) => [s, results.filter((r) => r.status === s).length])),
};
fs.writeFileSync(path.join(outDir, 'certification.json'), `${JSON.stringify(summary, null, 2)}\n`);
const md = [
  `# WrongIndex certification run`,
  '',
  `Generated ${summary.generatedAt} from \`${commit}\`${dirty ? ' (working tree not clean)' : ''}.`,
  '',
  '| Tool | Version |',
  '|---|---|',
  ...Object.entries(versions).map(([k, v]) => `| ${k} | ${v ?? 'not available'} |`),
  '',
  '| Gate | Requirement | Result | Detail | Seconds |',
  '|---|---|---|---|---:|',
  ...results.map((r) => `| ${r.id} ${r.name} | ${r.req} | ${r.status} | ${(r.detail || r.reason || '').replace(/\|/g, '/')} | ${r.seconds ?? ''} |`),
  ...manual.map((m) => `| ${m.id} ${m.name} | ${m.req} | ${m.status} | ${m.reason} | |`),
  '',
  `PASS ${summary.counts.PASS} · FAIL ${summary.counts.FAIL} · NOT RUN ${summary.counts['NOT RUN']} (automated) · manual NOT RUN ${manual.length}`,
  '',
].join('\n');
fs.writeFileSync(path.join(outDir, 'certification.md'), md);
console.log(outDir);
process.exit(summary.counts.FAIL > 0 ? 1 : 0);
