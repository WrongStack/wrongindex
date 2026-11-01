/**
 * WI-008: baseline timings of the WrongStack indexer (clean oracle).
 *
 * Each iteration runs in a fresh Bun process (`--iteration`) so module
 * loading and caches start cold, against a fresh copy of the corpus:
 *
 *   cold_full        empty index, full run
 *   noop_full        immediate second full run, nothing changed
 *   noop_targeted    targeted run of one unchanged file
 *   edit_1           one file edited, targeted run
 *   edit_10          ten files edited, targeted run
 *
 * The corpus is a Git repository (realistic discovery and blob trust).
 * Reports per-scenario distributions over N iterations plus machine metadata.
 * These are measurements of the BASELINE, not WrongIndex targets.
 *
 * Usage: bun scripts/bench/baseline.ts --wrongstack <clean-oracle-dir> [--iterations 5]
 */
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { join, resolve } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
const oracle = resolve(arg('--wrongstack') ?? '');
const repo = resolve(import.meta.dir, '..', '..');
const workRoot = process.platform === 'win32' ? 'C:/wrongindex-oracle/bench' : '/tmp/wrongindex-oracle/bench';
const corpusSource = join(oracle, 'packages');
const PACKAGES = ['agent-catalog', 'contracts', 'coordination', 'core', 'kanban', 'persistence', 'primitives', 'tools'];

const git = (args: string[], cwd: string) => {
  const r = Bun.spawnSync(['git', '-c', 'user.name=bench', '-c', 'user.email=b@b', ...args], { cwd, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0) throw new Error(`git ${args.join(' ')}: ${r.stderr.toString()}`);
};

function listFiles(dir: string, out: string[] = []): string[] {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) listFiles(p, out);
    else out.push(p);
  }
  return out;
}

if (process.argv.includes('--iteration')) {
  // ── one isolated iteration ──────────────────────────────────────────────
  const corpus = arg('--corpus')!;
  const indexDir = arg('--index')!;
  const base = join(oracle, 'packages/tools/src/codebase-index');
  let peakRss = process.memoryUsage().rss;
  const sampler = setInterval(() => {
    peakRss = Math.max(peakRss, process.memoryUsage().rss);
  }, 25);
  const { IndexStore } = await import(`${base}/writer.ts`);
  const { runIndexerWithStore } = await import(`${base}/indexer.ts`);
  const run = async (files?: string[]) => {
    const store = new IndexStore(corpus, { indexDir });
    const t0 = performance.now();
    const r = await runIndexerWithStore(store, { projectRoot: corpus, indexDir, ...(files ? { files } : {}) });
    const ms = performance.now() - t0;
    store.close();
    return { ms, filesIndexed: r.filesIndexed, symbolsIndexed: r.symbolsIndexed, errors: r.errors.length };
  };
  const tsFiles = listFiles(join(corpus, 'tools', 'src')).filter((f) => f.endsWith('.ts')).sort();
  const edit = (f: string, i: number) => writeFileSync(f, `${readFileSync(f, 'utf8')}\nexport function __benchEdit${i}(): number { return ${i}; }\n`);
  const result: Record<string, unknown> = {};
  result.cold_full = await run();
  result.noop_full = await run();
  result.noop_targeted = await run([tsFiles[0]!]);
  edit(tsFiles[1]!, 1);
  result.edit_1 = await run([tsFiles[1]!]);
  const ten = tsFiles.slice(2, 12);
  ten.forEach((f, i) => edit(f, 100 + i));
  result.edit_10 = await run(ten);
  clearInterval(sampler);
  result.peakRssMiB = Math.round(peakRss / 1024 / 1024);
  result.indexBytes = readFileSync(join(indexDir, 'index.db')).length;
  console.log(JSON.stringify(result));
  process.exit(0);
}

// ── driver ──────────────────────────────────────────────────────────────────
if (!existsSync(join(oracle, '.wrongindex-oracle.json'))) {
  console.error('usage: bun scripts/bench/baseline.ts --wrongstack <clean-oracle-dir> [--iterations 5]');
  process.exit(2);
}
const provenance = JSON.parse(readFileSync(join(oracle, '.wrongindex-oracle.json'), 'utf8')) as { commit: string };
const iterations = Number(arg('--iterations') ?? 5);
const template = join(workRoot, 'corpus-template');
rmSync(workRoot, { recursive: true, force: true });
mkdirSync(template, { recursive: true });
for (const p of PACKAGES) {
  cpSync(join(corpusSource, p, 'src'), join(template, p, 'src'), { recursive: true });
  cpSync(join(corpusSource, p, 'package.json'), join(template, p, 'package.json'));
}
writeFileSync(join(template, 'package.json'), JSON.stringify({ name: 'bench-corpus', private: true, workspaces: ['*'] }));
git(['init', '-q', '-b', 'main'], template);
git(['add', '.'], template);
git(['commit', '-qm', 'corpus'], template);
const files = listFiles(template).filter((f) => !f.includes(`${join(template, '.git')}`));
const corpusBytes = files.reduce((n, f) => n + readFileSync(f).length, 0);

const runs: Array<Record<string, { ms: number } | number>> = [];
for (let i = 0; i < iterations; i++) {
  const corpus = join(workRoot, `corpus-${i}`);
  const index = join(workRoot, `index-${i}`);
  cpSync(template, corpus, { recursive: true });
  const r = Bun.spawnSync(
    [process.execPath, import.meta.path, '--iteration', '--wrongstack', oracle, '--corpus', corpus, '--index', index],
    { stdout: 'pipe', stderr: 'pipe' },
  );
  if (r.exitCode !== 0) throw new Error(r.stderr.toString());
  const line = r.stdout.toString().trim().split('\n').pop()!;
  runs.push(JSON.parse(line));
  console.error(`iteration ${i + 1}/${iterations}: cold ${Math.round((runs[i]!.cold_full as { ms: number }).ms)} ms`);
  rmSync(corpus, { recursive: true, force: true });
  rmSync(index, { recursive: true, force: true });
}

const scenarios = ['cold_full', 'noop_full', 'noop_targeted', 'edit_1', 'edit_10'];
const stats = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  const median = s.length % 2 ? s[(s.length - 1) / 2]! : (s[s.length / 2 - 1]! + s[s.length / 2]!) / 2;
  return { min: Math.round(s[0]!), median: Math.round(median), max: Math.round(s[s.length - 1]!), samples: s.map(Math.round) };
};
const sqliteVersion = (new DatabaseSync(':memory:').prepare('select sqlite_version() v').get() as { v: string }).v;
const report = {
  schema: 'wrongindex.bench-baseline/1',
  subject: 'WrongStack indexer (clean oracle), in-process runIndexerWithStore',
  wrongstackCommit: provenance.commit,
  measuredAt: new Date().toISOString(),
  corpus: {
    description: 'src/ of 8 WrongStack workspace packages at the baseline commit, as a Git repository',
    files: files.length,
    bytes: corpusBytes,
    filesIndexed: (runs[0]!.cold_full as unknown as { filesIndexed: number }).filesIndexed,
    symbolsIndexed: (runs[0]!.cold_full as unknown as { symbolsIndexed: number }).symbolsIndexed,
  },
  machine: {
    platform: `${os.platform()} ${os.release()}`,
    arch: os.arch(),
    cpu: os.cpus()[0]?.model.trim(),
    logicalCpus: os.cpus().length,
    totalMemGiB: Math.round(os.totalmem() / 1024 ** 3),
    bun: Bun.version,
    sqlite: sqliteVersion,
    note: 'Developer workstation with normal background load and antivirus; not the M0 reference machine.',
  },
  iterations,
  wallMs: Object.fromEntries(scenarios.map((k) => [k, stats(runs.map((r) => (r[k] as { ms: number }).ms))])),
  peakRssMiB: stats(runs.map((r) => r.peakRssMiB as number)),
  indexBytes: runs[0]!.indexBytes,
  errorsPerRun: runs.map((r) => scenarios.map((k) => (r[k] as unknown as { errors: number }).errors)),
};
mkdirSync(join(repo, 'benchmarks/results'), { recursive: true });
const out = join(repo, `benchmarks/results/baseline-wrongstack-${provenance.commit.slice(0, 12)}.json`);
writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
rmSync(workRoot, { recursive: true, force: true });
console.log(JSON.stringify({ wallMs: Object.fromEntries(Object.entries(report.wallMs).map(([k, v]) => [k, v.median])), peakRssMiB: report.peakRssMiB.median, corpus: report.corpus }, null, 2));
