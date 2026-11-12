/**
 * WI-006: materialize a clean, built WrongStack oracle from an exact commit.
 *
 * The working-tree oracle is provisional because (a) contract files may be
 * dirty and (b) `@wrongstack/*` packages resolve to whatever `dist/` was last
 * built. This script removes both sources of drift:
 *
 *   1. `git archive <commit>` of only the packages the index code loads.
 *   2. Third-party `node_modules` entries are junctioned to the checkout's
 *      installed tree (pinned by the archived `bun.lock`, whose hash is
 *      recorded); every `@wrongstack/*` entry is redirected into the archive.
 *   3. Each workspace package is built from the archived source with the
 *      archived build script, in dependency order.
 *
 * Output: <out>/ with `.wrongindex-oracle.json` provenance. Pass <out> as
 * `--wrongstack` to build-fixtures.ts.
 *
 * Usage: bun scripts/oracle/prepare-clean-wrongstack.ts --wrongstack <checkout>
 *          [--commit <sha>] [--out <dir>]
 */
import { createHash } from 'node:crypto';
import {
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function run(cmd: string[], cwd: string): string {
  const r = Bun.spawnSync(cmd, { cwd, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0) {
    throw new Error(`${cmd.join(' ')} (in ${cwd}) failed:\n${r.stdout.toString()}\n${r.stderr.toString()}`);
  }
  return r.stdout.toString();
}

const sha256 = (data: Uint8Array | string) => createHash('sha256').update(data).digest('hex');

const repo = resolve(import.meta.dir, '..', '..');
const checkout = arg('--wrongstack');
if (!checkout) {
  console.error('usage: bun scripts/oracle/prepare-clean-wrongstack.ts --wrongstack <checkout> [--commit <sha>] [--out <dir>]');
  process.exit(2);
}
const baseline = JSON.parse(readFileSync(join(repo, 'contracts/baseline/wrongstack-baseline.json'), 'utf8')) as {
  commit: string;
};
const commit = run(['git', 'rev-parse', `${arg('--commit') ?? baseline.commit}^{commit}`], checkout).trim();
const out = resolve(arg('--out') ?? join(process.platform === 'win32' ? 'C:/wrongindex-oracle' : '/tmp/wrongindex-oracle', `wrongstack-${commit.slice(0, 12)}`));

/** Workspace packages the index code loads at runtime, in build order. */
const PACKAGES = ['primitives', 'persistence', 'kanban', 'contracts', 'agent-catalog', 'core', 'coordination', 'tools'];
/** Packages whose dist must be built (tools is imported from source). */
const BUILD = ['primitives', 'persistence', 'kanban', 'contracts', 'agent-catalog', 'core'];

// 1. Clean archive.
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
const tarPath = join(out, '..', `wrongstack-${commit.slice(0, 12)}.tar`);
const rootFiles = run(['git', 'ls-tree', '--name-only', commit], checkout)
  .split('\n')
  .filter((f) => f === 'package.json' || f === 'bun.lock' || f === '.bun-version' || /^tsconfig.*\.json$/.test(f));
run(
  ['git', 'archive', '--format=tar', '-o', tarPath, commit, '--', ...rootFiles, 'scripts', ...PACKAGES.map((p) => `packages/${p}`)],
  checkout,
);
const archiveSha256 = sha256(readFileSync(tarPath));
// Relative paths: GNU tar treats a drive-letter prefix (`C:`) as a remote host.
run(['tar', '-xf', `../wrongstack-${commit.slice(0, 12)}.tar`], out);
rmSync(tarPath);

// An implementation under review can be tested without pretending a dirty
// checkout is an immutable oracle. Apply only recorded, checksummed files to
// a NEW archive; retain the original commit-only oracle beside it.
const overlayPath = arg('--overlay-manifest');
let overlay: { manifestSha256: string; files: { path: string; sha256: string }[] } | undefined;
if (overlayPath) {
  const bytes = readFileSync(overlayPath);
  const input = JSON.parse(bytes.toString()) as { baseCommit: string; files: { path: string; sha256: string; source: string }[] };
  if (input.baseCommit !== commit) throw new Error('overlay base commit does not match archive');
  for (const file of input.files) {
    if (isAbsolute(file.path) || file.path.includes('\\') || file.path.split('/').some((p) => p === '..' || p === '.') || !file.path.startsWith('packages/tools/src/codebase-index/')) throw new Error(`invalid overlay destination: ${file.path}`);
    const content = readFileSync(resolve(dirname(resolve(overlayPath)), file.source));
    if (sha256(content) !== file.sha256) throw new Error(`overlay hash mismatch: ${file.path}`);
    mkdirSync(dirname(join(out, file.path)), { recursive: true });
    writeFileSync(join(out, file.path), content);
  }
  overlay = { manifestSha256: sha256(bytes), files: input.files.map(({ path, sha256 }) => ({ path, sha256 })) };
}

// 2. node_modules: third-party entries junctioned, @wrongstack/* -> archive.
function junction(target: string, path: string) {
  symlinkSync(target, path, 'junction');
}

function mirrorNodeModules(srcDir: string, dstDir: string) {
  if (!existsSync(srcDir)) return;
  mkdirSync(dstDir, { recursive: true });
  for (const entry of readdirSync(srcDir)) {
    if (entry === '.bin' || entry === '.cache') continue;
    const src = join(srcDir, entry);
    const dst = join(dstDir, entry);
    if (entry === '@wrongstack') {
      mkdirSync(dst, { recursive: true });
      for (const name of readdirSync(src)) {
        const archived = join(out, 'packages', name);
        // Only archived packages are reachable; anything else stays absent so
        // an unexpected runtime dependency fails loudly instead of loading dirty code.
        if (existsSync(archived)) junction(archived, join(dst, name));
      }
      continue;
    }
    junction(lstatSync(src).isSymbolicLink() ? realpathSync(src) : src, dst);
  }
}

mirrorNodeModules(join(checkout, 'node_modules'), join(out, 'node_modules'));
for (const name of PACKAGES) {
  mirrorNodeModules(join(checkout, 'packages', name, 'node_modules'), join(out, 'packages', name, 'node_modules'));
}

// 3. Build workspace dists from archived sources.
const builds: Record<string, string> = {};
for (const name of BUILD) {
  const dir = join(out, 'packages', name);
  rmSync(join(dir, 'dist'), { recursive: true, force: true });
  const started = Date.now();
  run(['bun', '../../scripts/build-package.mjs'], dir);
  console.error(`built @wrongstack/${name} in ${((Date.now() - started) / 1000).toFixed(1)}s`);
  const files = readdirRecursive(join(dir, 'dist')).sort();
  const h = createHash('sha256');
  for (const f of files) h.update(f).update(readFileSync(join(dir, 'dist', f)));
  builds[name] = h.digest('hex');
}

function readdirRecursive(dir: string, prefix = ''): string[] {
  const outFiles: string[] = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix ? `${prefix}/${e.name}` : e.name;
    if (e.isDirectory()) outFiles.push(...readdirRecursive(join(dir, e.name), rel));
    else outFiles.push(rel);
  }
  return outFiles;
}

const provenance = {
  schema: 'wrongindex.clean-oracle/1',
  commit,
  archiveSha256,
  ...(overlay ? { sourceState: 'immutable-archive-with-recorded-overlay', overlay } : {}),
  archivedPaths: [...rootFiles, 'scripts', ...PACKAGES.map((p) => `packages/${p}`)],
  bunLockSha256: existsSync(join(out, 'bun.lock')) ? sha256(readFileSync(join(out, 'bun.lock'))) : null,
  thirdPartyNodeModules: 'junctioned from the source checkout; pinned by bun.lock',
  distSha256: builds,
  bun: Bun.version,
  preparedAt: new Date().toISOString(),
};
writeFileSync(join(out, '.wrongindex-oracle.json'), `${JSON.stringify(provenance, null, 2)}\n`);
console.log(out);
