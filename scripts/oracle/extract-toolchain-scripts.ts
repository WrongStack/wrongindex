/**
 * Vendor WrongStack's out-of-process parser programs (`toolchain-scripts.ts`)
 * verbatim, so the native engine runs exactly the extraction WrongStack runs.
 *
 *   bun scripts/oracle/extract-toolchain-scripts.ts --wrongstack <clean oracle>
 *
 * Writes `crates/wrongindex-engine/src/parse/toolchain/py_batch.py` and
 * `provenance.json` (oracle commit, archive hash, source hash, script hash).
 * Do not edit the outputs by hand.
 */
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: --wrongstack <clean oracle>');
  process.exit(2);
}
const marker = join(resolve(wrongstack), '.wrongindex-oracle.json');
if (!existsSync(marker)) {
  console.error('--wrongstack must be a clean oracle (missing .wrongindex-oracle.json)');
  process.exit(2);
}
const oracle = JSON.parse(readFileSync(marker, 'utf8')) as { commit: string; archiveSha256: string };
const source = `${resolve(wrongstack)}/packages/tools/src/codebase-index/toolchain-scripts.ts`;
const mod = await import(source);
const repo = resolve(import.meta.dir, '..', '..');
const out = join(repo, 'crates/wrongindex-engine/src/parse/toolchain');
mkdirSync(out, { recursive: true });
const sha = (s: string | Buffer) => createHash('sha256').update(s).digest('hex');
const py: string = mod.PY_BATCH_SCRIPT;
writeFileSync(join(out, 'py_batch.py'), py);
writeFileSync(
  join(out, 'provenance.json'),
  `${JSON.stringify(
    {
      generator: 'scripts/oracle/extract-toolchain-scripts.ts',
      wrongstackCommit: oracle.commit,
      archiveSha256: oracle.archiveSha256,
      'toolchain-scripts.ts': sha(readFileSync(source)),
      'py_batch.py': sha(py),
    },
    null,
    2,
  )}\n`,
);
console.log(`py_batch.py: ${py.length} chars, sha256 ${sha(py)}`);
