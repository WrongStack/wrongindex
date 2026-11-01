/**
 * Differential check of WI-205 discovery against WrongStack (clean oracle).
 *
 *  1. Gitignore matrix: pattern sets x paths through WrongStack's
 *     compileGitignore vs `wrongindex debug gitignore`.
 *  2. Full discovery: WrongStack findSourceFiles vs `wrongindex debug discover`
 *     on fixture projects (walk), the same projects as Git repositories
 *     (dirty/untracked/deleted state), a junction/symlink tree, and any extra
 *     real checkouts passed with --repo (read-only).
 *
 * Compared: file list (order and set), complete, snapshot key, clean blobs,
 * dirty hashes.
 *
 * Usage: bun scripts/oracle/discovery-diff.ts --wrongstack <clean-oracle-dir>
 *          [--cli <wrongindex.exe>] [--repo <dir>]...
 */
import { cpSync, mkdirSync, mkdtempSync, readdirSync, rmSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const argv = process.argv;
const arg = (n: string) => (argv.indexOf(n) >= 0 ? argv[argv.indexOf(n) + 1] : undefined);
const oracle = arg('--wrongstack');
if (!oracle) {
  console.error('usage: bun scripts/oracle/discovery-diff.ts --wrongstack <clean-oracle-dir> [--cli <path>] [--repo <dir>]...');
  process.exit(2);
}
const repoRoot = resolve(import.meta.dir, '..', '..');
const cli = resolve(arg('--cli') ?? join(repoRoot, 'target/debug/wrongindex.exe'));
const extraRepos = argv.flatMap((a, i) => (a === '--repo' ? [argv[i + 1]!] : []));
const ci = join(oracle, 'packages/tools/src/codebase-index');
const { compileGitignore, loadGitignoreMatcher } = await import(join(ci, 'gitignore.ts'));
const { findSourceFiles } = await import(join(ci, 'index-source-files.ts'));

let failures = 0;
const report = (ok: boolean, name: string, detail = '') => {
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `\n      ${detail}`}`);
};

// ── 1. gitignore matrix ─────────────────────────────────────────────────────
const patternSets = [
  'node_modules\n*.log\n',
  '/build\nout/\ndocs/*.md\n',
  '**/gen/**\n',
  '*.ts\n!keep.ts\n',
  'vendor/\n!vendor/keep.ts\n',
  'file[0-9].ts\n[!a]x.js\n\\#hash\n\\!bang\n',
  'a/**/b\na/**/**/c\n**/deep/**/x.ts\n',
  '# comment\n\n  \nfoo   \n/bar/\n!/bar/keep/\n',
  'src/*/tmp\n*.d.ts\n!src/types.d.ts\n',
  'abc?\n[a-c]*.py\n**\n',
  'logs\n!logs/important.log\nlogs/*.tmp\n',
  'dir/\n!dir/\n',
  '*.min.*\n.env*\n!.env.example\n',
  'Ünïcödé/\nçalışma*.ts\n',
];
const paths: Array<[string, boolean]> = [];
for (const p of [
  'node_modules', 'a/node_modules/x.js', 'deep/err.log', 'logs/x.ts', 'logs', 'logs/important.log', 'logs/a.tmp',
  'build/a.ts', 'src/build/a.ts', 'out', 'out/x.ts', 'docs/a.md', 'docs/sub/a.md', 'a/gen/x.ts', 'gen/x.ts',
  'a.ts', 'keep.ts', 'src/keep.ts', 'vendor/keep.ts', 'vendor/x.ts', 'file3.ts', 'filea.ts', 'bx.js', 'ax.js',
  '#hash', '!bang', 'a/b', 'a/x/y/b', 'a/c', 'a/x/c', 'z/deep/q/x.ts', 'deep/x.ts', 'foo', 'x/foo', 'foo/y',
  'bar', 'bar/keep', 'bar/keep/z.ts', 'x/bar', 'src/a/tmp', 'src/a/b/tmp', 'types.d.ts', 'src/types.d.ts',
  'abcd', 'abc', 'b1.py', 'd1.py', 'dir', 'dir/x', 'app.min.js', '.env', '.env.local', '.env.example',
  'Ünïcödé', 'Ünïcödé/x.ts', 'çalışma1.ts', 'a/çalışma.ts', './a.ts', '/a.ts', 'a\\b\\c.ts',
]) {
  paths.push([p, false], [p, true]);
}
let matrixCases = 0;
let matrixDiff = 0;
for (const set of patternSets) {
  const ws = compileGitignore(set.split('\n'));
  const expected = paths.map(([p, d]) => ws(p, d) as boolean);
  const run = Bun.spawnSync([cli, 'debug', 'gitignore'], {
    stdin: new TextEncoder().encode(JSON.stringify({ gitignore: set, cases: paths.map(([path, isDir]) => ({ path, isDir })) })),
    stdout: 'pipe',
    stderr: 'pipe',
  });
  const actual = JSON.parse(run.stdout.toString()) as boolean[];
  paths.forEach(([p, d], i) => {
    matrixCases++;
    if (expected[i] !== actual[i]) {
      matrixDiff++;
      if (matrixDiff <= 15) console.log(`      gitignore ${JSON.stringify(set)} path=${p} dir=${d}: expected ${expected[i]} got ${actual[i]}`);
    }
  });
}
report(matrixDiff === 0, `gitignore matrix (${matrixCases} cases, ${patternSets.length} pattern sets)`, `${matrixDiff} differences`);

// ── 2. full discovery ───────────────────────────────────────────────────────
async function compareDiscovery(name: string, root: string) {
  const ws = await findSourceFiles(root, [], await loadGitignoreMatcher(root));
  const out = Bun.spawnSync([cli, 'debug', 'discover', root], { stdout: 'pipe', stderr: 'pipe' });
  if (out.exitCode !== 0) return report(false, name, out.stderr.toString());
  const ours = JSON.parse(out.stdout.toString());
  const sameOrder = JSON.stringify(ws.files) === JSON.stringify(ours.files);
  const a = new Set<string>(ws.files);
  const b = new Set<string>(ours.files);
  const missing = [...a].filter((f) => !b.has(f));
  const extra = [...b].filter((f) => !a.has(f));
  const sizeEq = (m1: Map<string, string> | undefined, o: Record<string, string>) =>
    (m1?.size ?? 0) === Object.keys(o).length && [...(m1 ?? new Map())].every(([k, v]) => o[k] === v);
  const checks = {
    files: missing.length === 0 && extra.length === 0,
    order: sameOrder,
    complete: ws.complete === ours.complete,
    snapshotKey: (ws.snapshotKey ?? null) === (ours.snapshotKey ?? null),
    cleanBlobs: sizeEq(ws.cleanBlobs, ours.cleanBlobs),
    dirtyHashes: sizeEq(ws.dirtyHashes, ours.dirtyHashes),
  };
  const ok = Object.values(checks).every(Boolean);
  report(
    ok,
    `${name} [${ours.source}, ${ours.files.length} files]`,
    `${JSON.stringify(checks)} missing=${missing.slice(0, 3)} extra=${extra.slice(0, 3)}`,
  );
}

const base = mkdtempSync(join(tmpdir(), 'wi-discovery-'));
const git = (cwd: string, ...args: string[]) => {
  const r = Bun.spawnSync(['git', '-c', 'user.name=t', '-c', 'user.email=t@t', ...args], { cwd, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0) throw new Error(r.stderr.toString());
};
for (const fixture of readdirSync(join(repoRoot, 'tests/fixtures/projects')).sort()) {
  const walkDir = join(base, 'walk', fixture);
  cpSync(join(repoRoot, 'tests/fixtures/projects', fixture), walkDir, { recursive: true });
  writeFileSync(join(walkDir, '.gitignore'), 'ignored-dir/\n*.tmp.ts\n');
  mkdirSync(join(walkDir, 'ignored-dir'), { recursive: true });
  writeFileSync(join(walkDir, 'ignored-dir', 'x.ts'), 'export const x = 1;\n');
  writeFileSync(join(walkDir, 'scratch.tmp.ts'), 'export const t = 1;\n');
  await compareDiscovery(`walk ${fixture}`, walkDir);

  const gitDir = join(base, 'git', fixture);
  cpSync(walkDir, gitDir, { recursive: true });
  git(gitDir, 'init', '-q', '-b', 'main');
  git(gitDir, 'add', '.');
  git(gitDir, 'commit', '-qm', 'init');
  // Dirty state: modify one tracked file, add an untracked one, delete one.
  const tracked = readdirSync(gitDir, { recursive: true, withFileTypes: true })
    .filter((e) => e.isFile() && !e.parentPath.includes('.git') && /\.(ts|py|go|rs|java|json|yaml)$/.test(e.name))
    .map((e) => join(e.parentPath, e.name))
    .sort();
  if (tracked[0]) writeFileSync(tracked[0], `${'// changed\n'}${Bun.file(tracked[0]).size}\n`);
  if (tracked[1]) unlinkSync(tracked[1]);
  writeFileSync(join(gitDir, 'untracked.ts'), 'export const u = 1;\n');
  await compareDiscovery(`git ${fixture}`, gitDir);
}

// Links: walk mode skips symlinks/junctions (Dirent semantics).
const linkRoot = join(base, 'links');
mkdirSync(join(linkRoot, 'real'), { recursive: true });
writeFileSync(join(linkRoot, 'real', 'a.ts'), 'export const a = 1;\n');
symlinkSync(join(linkRoot, 'real'), join(linkRoot, 'junction'), 'junction');
try {
  symlinkSync(join(linkRoot, 'real', 'a.ts'), join(linkRoot, 'file-link.ts'), 'file');
} catch {
  console.log('SKIP  file symlink (no privilege)');
}
await compareDiscovery('walk links/junctions', linkRoot);

for (const repo of extraRepos) await compareDiscovery(`repo ${repo}`, resolve(repo));

rmSync(base, { recursive: true, force: true });
console.log(failures === 0 ? 'all discovery checks match' : `${failures} check(s) differ`);
process.exit(failures === 0 ? 0 : 1);
