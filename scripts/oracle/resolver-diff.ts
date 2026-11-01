/**
 * Differential check: WrongStack's index-location resolver vs the Rust port
 * (`wrongindex resolve-index-dir`) on real filesystem scenarios.
 *
 * Scenarios are created in a private temp directory: plain checkout, letter
 * case variants, trailing/forward slashes, dot segments, non-ASCII and long
 * folder names, a real `git worktree`, a submodule-style `.git` file, a
 * malformed `.git` file, and a directory junction.
 *
 * Usage: bun scripts/oracle/resolver-diff.ts --wrongstack <clean-oracle-dir> [--cli <wrongindex.exe>]
 */
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
const oracle = arg('--wrongstack');
if (!oracle) {
  console.error('usage: bun scripts/oracle/resolver-diff.ts --wrongstack <clean-oracle-dir> [--cli <path>]');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const cli = resolve(arg('--cli') ?? join(repo, 'target/debug/wrongindex.exe'));
const { resolveWstackPaths } = await import(join(oracle, 'packages/persistence/src/wstack-paths.ts'));

const base = mkdtempSync(join(tmpdir(), 'wi-resolver-'));
const globalRoot = join(base, 'ws-home');
const run = (cmd: string[], cwd: string) => {
  const r = Bun.spawnSync(cmd, { cwd, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0) throw new Error(`${cmd.join(' ')}: ${r.stderr.toString()}`);
  return r.stdout.toString();
};

const scenarios: Array<[string, string]> = [];
const plain = join(base, 'Plain Repo');
mkdirSync(plain);
scenarios.push(['plain', plain]);
scenarios.push(['upper-case spelling', plain.toUpperCase()]);
scenarios.push(['lower-case spelling', plain.toLowerCase()]);
scenarios.push(['trailing separator', `${plain}\\`]);
scenarios.push(['forward slashes', plain.split('\\').join('/')]);
scenarios.push(['dot segments', join(base, 'x', '..', 'Plain Repo', '.')]);
const turkish = join(base, 'Çalışma Projesi');
mkdirSync(turkish);
scenarios.push(['non-ASCII name', turkish]);
const long = join(base, `${'VeryLongProjectName'.repeat(4)}_end`);
mkdirSync(long);
scenarios.push(['long name (slug cap)', long]);
const symbolsOnly = join(base, '___');
mkdirSync(symbolsOnly);
scenarios.push(['name without slug characters', symbolsOnly]);

// 8.3 short-name spelling (differs from the real path by more than case, so
// WrongStack keeps it as given: a different identity than the long name).
if (process.platform === 'win32') {
  // Path passed through the environment: no shell quoting involved.
  const r = Bun.spawnSync(
    ['powershell', '-NoProfile', '-NonInteractive', '-Command', '(New-Object -ComObject Scripting.FileSystemObject).GetFolder($env:WI_TARGET).ShortPath'],
    { stdout: 'pipe', env: { ...process.env, WI_TARGET: plain } },
  );
  const short = r.stdout.toString().trim();
  if (short && short.toLowerCase() !== plain.toLowerCase()) scenarios.push(['8.3 short-name spelling', short]);
  else console.log('SKIP  8.3 short-name spelling (short names disabled on this volume)');
}

// Real git worktree.
const main = join(base, 'MainRepo');
mkdirSync(main);
run(['git', 'init', '-q', '-b', 'main'], main);
writeFileSync(join(main, 'a.txt'), 'a');
run(['git', '-c', 'user.name=t', '-c', 'user.email=t@t', 'add', '.'], main);
run(['git', '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-qm', 'init'], main);
const wt = join(base, 'Feature WT');
run(['git', 'worktree', 'add', '-q', wt, '-b', 'feature'], main);
scenarios.push(['git main checkout', main]);
scenarios.push(['git linked worktree', wt]);

// Submodule-style .git file (modules/, not worktrees/) keeps its own identity.
const sub = join(base, 'SubLike');
mkdirSync(join(main, '.git', 'modules', 'sub'), { recursive: true });
mkdirSync(sub);
writeFileSync(join(sub, '.git'), `gitdir: ${join(main, '.git', 'modules', 'sub')}\n`);
writeFileSync(join(main, '.git', 'modules', 'sub', 'commondir'), '../..\n');
scenarios.push(['submodule-style gitdir', sub]);

// Malformed .git file never fails resolution.
const malformed = join(base, 'Malformed');
mkdirSync(malformed);
writeFileSync(join(malformed, '.git'), 'not a gitdir line\n');
scenarios.push(['malformed .git file', malformed]);

// Junction: a different path, not a case variant, keeps its own spelling.
const junction = join(base, 'Junction To Plain');
symlinkSync(plain, junction, 'junction');
scenarios.push(['junction', junction]);

let failures = 0;
const results = [];
for (const [name, root] of scenarios) {
  const expected = resolveWstackPaths({ projectRoot: root, globalRoot }).projectCodebaseIndex as string;
  const out = Bun.spawnSync([cli, 'resolve-index-dir', root, '--wrongstack-home', globalRoot, '--json'], {
    stdout: 'pipe',
    stderr: 'pipe',
  });
  let actual = `<exit ${out.exitCode}: ${out.stderr.toString().trim()}>`;
  if (out.exitCode === 0) actual = (JSON.parse(out.stdout.toString()) as { indexDir: string }).indexDir;
  const ok = actual === expected;
  if (!ok) failures++;
  results.push({ name, ok, expected: expected.replace(base, '<base>'), actual: actual.replace(base, '<base>') });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : `\n      expected ${expected}\n      actual   ${actual}`}`);
}

run(['git', 'worktree', 'remove', '--force', wt], main);
rmSync(base, { recursive: true, force: true });
console.log(`${scenarios.length - failures}/${scenarios.length} scenarios match`);
process.exit(failures === 0 ? 0 : 1);
