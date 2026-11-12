// Exercise the real certification command: an unexecuted required gate must
// not look successful to shell automation; a passing selected gate still can.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const work = resolve('target', 'certify-regression', `${process.pid}-${Date.now()}`);
mkdirSync(work, { recursive: true });
for (const [gate, pass] of [['G11', false], ['G01', true]]) {
  const env = { ...process.env };
  delete env.WRONGINDEX_SOAK_SECS;
  const out = join(work, gate);
  const result = spawnSync(process.execPath, ['scripts/release/certify.mjs', '--only', gate, '--out', out], { env, encoding: 'utf8', windowsHide: true });
  assert.equal(result.status === 0, pass, result.stdout + result.stderr);
  const summary = JSON.parse(readFileSync(join(out, 'certification.json'), 'utf8'));
  assert.equal(summary.gates.length, 1);
  assert.equal(summary.gates[0].status, pass ? 'PASS' : 'NOT RUN');
  assert.equal(summary.releaseReady, false, 'a focused check cannot qualify a whole release');
}
console.log('PASS: missing required gate fails; passing focused control succeeds; neither claims release readiness');
