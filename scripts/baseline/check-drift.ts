/** Read immutable Git objects only; never run the inspected project's scripts. */
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';

const args = process.argv.slice(2);
const option = (name: string) => {
  const i = args.indexOf(name);
  return i < 0 ? undefined : args[i + 1];
};
const checkout = option('--wrongstack');
if (!checkout) throw new Error('--wrongstack <checkout> is required');
const baseline = JSON.parse(readFileSync(resolve(option('--baseline') ?? 'contracts/baseline/wrongstack-baseline.json'), 'utf8')) as {
  commit: string;
  scope: string[];
  files: { path: string; sha256: string }[];
};
function git(...values: string[]): Buffer {
  const r = Bun.spawnSync(['git', ...values], { cwd: checkout, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0) throw new Error(`git object read failed: ${r.stderr.toString()}`);
  return Buffer.from(r.stdout);
}
const revision = git('rev-parse', '--verify', `${option('--revision') ?? 'HEAD'}^{commit}`).toString().trim();
const paths = git('ls-tree', '-rz', '--name-only', revision).toString().split('\0')
  .filter((p) => p && baseline.scope.some((prefix) => p === prefix || p.startsWith(prefix)));
// One process for all blobs: spawning Git for every file makes a drift gate
// take minutes on Windows. NUL framing also preserves spaces/newlines in names.
const batch = Bun.spawnSync(['git', 'cat-file', '--batch', '-Z'], {
  cwd: checkout, stdin: Buffer.from(paths.map((p) => `${revision}:${p}\0`).join('')),
  stdout: 'pipe', stderr: 'pipe',
});
if (batch.exitCode !== 0) throw new Error(`git batch read failed: ${batch.stderr.toString()}`);
const blobs = Buffer.from(batch.stdout);
let offset = 0;
const current = new Map<string, string>();
for (const p of paths) {
  const end = blobs.indexOf(0, offset);
  const header = blobs.subarray(offset, end).toString();
  const size = Number(header.split(' ')[2]);
  if (end < 0 || !Number.isSafeInteger(size) || size < 0 || !header.includes(' blob ')) throw new Error('invalid Git blob response');
  const start = end + 1;
  if (start + size >= blobs.length || blobs[start + size] !== 0) throw new Error('truncated Git blob response');
  current.set(p, createHash('sha256').update(blobs.subarray(start, start + size)).digest('hex'));
  offset = start + size + 1;
}
const previous = new Map(baseline.files.map((f) => [f.path, f.sha256]));
const changed = [...previous].filter(([p, h]) => current.has(p) && current.get(p) !== h).map(([p]) => p).sort();
const removed = [...previous.keys()].filter((p) => !current.has(p)).sort();
const added = [...current.keys()].filter((p) => !previous.has(p)).sort();
const report = {
  schema: 'wrongindex.source-drift/1', baselineCommit: baseline.commit, candidateCommit: revision,
  inspectedFiles: current.size, changed, removed, added,
  status: changed.length + removed.length + added.length ? 'DRIFT' : 'PASS',
};
const json = `${JSON.stringify(report, null, 2)}\n`;
const snapshot = option('--snapshot-out');
if (snapshot) {
  const oraclePath = option('--oracle');
  if (!oraclePath) throw new Error('--snapshot-out requires --oracle <prepared clean oracle> provenance');
  const oracle = JSON.parse(readFileSync(resolve(oraclePath, '.wrongindex-oracle.json'), 'utf8'));
  if (oracle.commit !== revision) throw new Error('oracle and Git revision do not match');
  if (oracle.overlay || oracle.sourceState === 'immutable-archive-with-recorded-overlay') throw new Error('commit-only snapshots require a commit-only oracle; retain overlays as separate manifests');
  const captured = {
    schema: 'wrongindex.baseline/1', source: 'https://github.com/WrongStack/WrongStack', commit: revision,
    capturedAt: new Date().toISOString(), capturedWith: { bun: Bun.version }, scope: baseline.scope,
    sourceState: 'immutable-git-objects', oracleProvenance: oracle, contractFilesDirty: [],
    files: [...current].map(([path, sha256]) => ({ path, sha256 })),
  };
  mkdirSync(dirname(resolve(snapshot)), { recursive: true });
  writeFileSync(resolve(snapshot), `${JSON.stringify(captured, null, 2)}\n`);
}
const out = option('--out');
if (out) { mkdirSync(dirname(resolve(out)), { recursive: true }); writeFileSync(resolve(out), json); }
process.stdout.write(json);
if (args.includes('--check') && report.status === 'DRIFT') process.exitCode = 1;
