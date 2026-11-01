/**
 * WI-000: capture an immutable WrongStack source baseline.
 *
 * Records the WrongStack HEAD commit, the Git blob SHA-256 of every source file
 * that defines the index contract, and whether the working tree diverges from
 * HEAD for that file. Contract fixtures must only be regenerated from a
 * baseline whose `contractFilesDirty` list is empty.
 *
 * Usage: bun scripts/baseline/export.ts --wrongstack <path> [--out <file>]
 */
import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';

const CONTRACT_PATH_PREFIXES = [
  'packages/tools/src/codebase-index/',
  'packages/persistence/src/wstack-paths.ts',
  'packages/cli/src/wiring/codebase-index.ts',
  'packages/codebase-index-mcp/package.json',
  'packages/core/src/utils/',
  '.bun-version',
];

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function git(cwd: string, args: string[]): Buffer {
  const r = Bun.spawnSync(['git', ...args], { cwd, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0) {
    throw new Error(`git ${args.join(' ')} failed: ${r.stderr.toString()}`);
  }
  return Buffer.from(r.stdout);
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/baseline/export.ts --wrongstack <path> [--out <file>]');
  process.exit(2);
}
const out = resolve(arg('--out') ?? 'contracts/baseline/wrongstack-baseline.json');

const head = git(wrongstack, ['rev-parse', 'HEAD']).toString().trim();
const tracked = git(wrongstack, ['ls-tree', '-r', '--name-only', 'HEAD'])
  .toString()
  .split('\n')
  .filter((p) => CONTRACT_PATH_PREFIXES.some((prefix) => p === prefix || p.startsWith(prefix)))
  .sort();

const dirtyAll = git(wrongstack, ['status', '--porcelain', '--untracked-files=no'])
  .toString()
  .split('\n')
  .filter(Boolean)
  .map((line) => line.slice(3).replace(/^"|"$/g, ''));
const dirtySet = new Set(dirtyAll);

const files = tracked.map((path) => {
  const blob = git(wrongstack, ['show', `HEAD:${path}`]);
  return {
    path,
    sha256: createHash('sha256').update(blob).digest('hex'),
    bytes: blob.length,
    workingTreeDirty: dirtySet.has(path),
  };
});

const manifest = {
  schema: 'wrongindex.baseline/1',
  source: 'https://github.com/WrongStack/WrongStack',
  commit: head,
  capturedAt: new Date().toISOString(),
  capturedWith: { bun: Bun.version },
  scope: CONTRACT_PATH_PREFIXES,
  repositoryDirtyFileCount: dirtyAll.length,
  contractFilesDirty: files.filter((f) => f.workingTreeDirty).map((f) => f.path),
  files,
};

mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify(manifest, null, 2)}\n`);
console.error(
  `baseline ${head}: ${files.length} contract files, ${manifest.contractFilesDirty.length} dirty in working tree -> ${out}`,
);
