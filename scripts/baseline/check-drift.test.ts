import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createHash } from 'node:crypto';

const root = mkdtempSync(join(tmpdir(), 'wi-drift-test-'));
function git(...args: string[]) {
  const r = Bun.spawnSync(['git', '-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', ...args], { cwd: root });
  assert.equal(r.exitCode, 0, r.stderr.toString());
  return r.stdout.toString().trim();
}
git('init', '-q');
mkdirSync(join(root, 'scope'));
writeFileSync(join(root, 'scope', 'keep.ts'), 'original');
writeFileSync(join(root, 'scope', 'remove.ts'), 'remove');
git('add', '.'); git('commit', '-qm', 'baseline');
const baseline = join(root, 'baseline.json');
writeFileSync(baseline, JSON.stringify({ commit: git('rev-parse', 'HEAD'), scope: ['scope/'],
  files: ['keep.ts', 'remove.ts'].map((f) => ({ path: `scope/${f}`, sha256: createHash('sha256').update(readFileSync(join(root, 'scope', f))).digest('hex') })) }));
const run = (...extra: string[]) => Bun.spawnSync(['bun', resolve(import.meta.dir, 'check-drift.ts'), '--wrongstack', root, '--baseline', baseline, '--check', ...extra]);
// Working-tree changes are deliberately ignored: certification compares immutable objects.
writeFileSync(join(root, 'scope', 'keep.ts'), 'edited');
assert.equal(run().exitCode, 0);

// A clean-commit snapshot must never silently certify a changed oracle.
const oracle = join(root, 'oracle');
mkdirSync(oracle);
const marker = join(oracle, '.wrongindex-oracle.json');
writeFileSync(marker, JSON.stringify({ commit: git('rev-parse', 'HEAD') }));
assert.equal(run('--snapshot-out', join(root, 'clean-snapshot.json'), '--oracle', oracle).exitCode, 0);
writeFileSync(marker, JSON.stringify({ commit: git('rev-parse', 'HEAD'), overlay: { files: ['changed-source.ts'] } }));
assert.notEqual(run('--snapshot-out', join(root, 'overlay-snapshot.json'), '--oracle', oracle).exitCode, 0, 'overlay must not certify a commit-only snapshot');
git('rm', 'scope/remove.ts');
writeFileSync(join(root, 'scope', 'added.ts'), 'new');
git('add', 'scope'); git('commit', '-qm', 'changed');
const r = run();
assert.equal(r.exitCode, 1);
const report = JSON.parse(r.stdout.toString());
assert.deepEqual(report.changed, ['scope/keep.ts']);
assert.deepEqual(report.removed, ['scope/remove.ts']);
assert.deepEqual(report.added, ['scope/added.ts']);
console.log('PASS: immutable control and clean snapshot; overlays cannot certify commit-only snapshots; modified, removed and added paths fail the drift gate');
