/**
 * License texts of what `bundle-tree-sitter.ts` vendors into the native
 * engine (WI-009, WI-703): the bundled JavaScript (WrongStack's tree-sitter
 * visitor and queries, web-tree-sitter, TypeScript) and the ten grammar WASM
 * files (built by tree-sitter-wasm from upstream grammars, whose licenses
 * belong to the grammar authors).
 *
 *   bun scripts/oracle/bundle-licenses.ts --wrongstack <clean oracle> --wrongstack-git <WrongStack checkout>
 *
 * Every text comes from the exact version that was bundled: the oracle's
 * resolved packages, WrongStack's LICENSE at the oracle commit, and each
 * grammar's published npm tarball at the version tree-sitter-wasm's lockfile
 * pins (tree-sitter-php: the pinned git commit). Writes
 * `crates/wrongindex-engine/tree-sitter/LICENSES/` (`<component>.txt` and
 * `licenses.json` with versions, sources and sha256). Network: npm registry,
 * GitHub (codeload, the tree-sitter-wasm lockfile). Do not edit the outputs
 * by hand.
 */
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
const wrongstack = arg('--wrongstack');
const wrongstackGit = arg('--wrongstack-git');
if (!wrongstack || !wrongstackGit) {
  console.error('usage: --wrongstack <clean oracle> --wrongstack-git <WrongStack checkout>');
  process.exit(2);
}
const marker = join(resolve(wrongstack), '.wrongindex-oracle.json');
if (!existsSync(marker)) {
  console.error('--wrongstack must be a clean oracle (missing .wrongindex-oracle.json)');
  process.exit(2);
}
const oracle = JSON.parse(readFileSync(marker, 'utf8')) as { commit: string };
const repo = resolve(import.meta.dir, '..', '..');
const treeSitterDir = join(repo, 'crates/wrongindex-engine/tree-sitter');
const provenance = JSON.parse(readFileSync(join(treeSitterDir, 'provenance.json'), 'utf8')) as {
  wrongstackCommit: string;
  grammars: Record<string, string>;
};
if (provenance.wrongstackCommit !== oracle.commit) {
  throw new Error(`oracle ${oracle.commit} is not the bundled commit ${provenance.wrongstackCommit}`);
}
const out = join(treeSitterDir, 'LICENSES');
const sha = (b: string | Uint8Array) => createHash('sha256').update(b).digest('hex');
const base = `${resolve(wrongstack)}/packages/tools/src/codebase-index`;

type Entry = { component: string; version: string; license: string; source: string; file: string; sha256: string };
const entries: Entry[] = [];
const texts = new Map<string, string>();
function record(component: string, version: string, license: string, source: string, text: string) {
  const file = `${component}.txt`;
  texts.set(file, text.replace(/\r\n/g, '\n'));
  entries.push({ component, version, license, source, file, sha256: sha(texts.get(file)!) });
}

function pkgDir(name: string): string {
  return dirname(Bun.resolveSync(`${name}/package.json`, base));
}
/** The license text plus any NOTICE file (Apache-2.0 asks for both). */
function licenseText(dir: string): string {
  const names = readdirSync(dir);
  const f = names.find((n) => /^(licen[cs]e|copying)(\.(md|txt))?$/i.test(n));
  if (!f) throw new Error(`no license file in ${dir}`);
  const notices = names.filter((n) => /^notice(\.(md|txt))?$/i.test(n)).sort();
  return [readFileSync(join(dir, f), 'utf8'), ...notices.map((n) => `--- ${n} ---\n${readFileSync(join(dir, n), 'utf8')}`)].join('\n');
}
function fromPackage(component: string, name: string) {
  const dir = pkgDir(name);
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')) as { version: string; license: string };
  record(component, pkg.version, pkg.license, `npm ${name}@${pkg.version} (resolved by the oracle)`, licenseText(dir));
  return pkg.version;
}

// WrongStack's own code (visitor, queries, extractors, util).
const wsLicense = Bun.spawnSync(['git', '-C', wrongstackGit, 'show', `${oracle.commit}:LICENSE`]);
if (wsLicense.exitCode !== 0) throw new Error(`git show ${oracle.commit}:LICENSE failed: ${wsLicense.stderr}`);
record('wrongstack', oracle.commit, 'MIT', `WrongStack ${oracle.commit}:LICENSE`, wsLicense.stdout.toString());

fromPackage('web-tree-sitter', 'web-tree-sitter');
fromPackage('typescript', '@typescript/typescript6');
const tswVersion = fromPackage('tree-sitter-wasm', 'tree-sitter-wasm');

// Upstream grammars at the versions tree-sitter-wasm built.
async function get(url: string): Promise<Response> {
  const r = await fetch(url, { headers: { 'User-Agent': 'wrongindex-bundle-licenses' } });
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  return r;
}
const lock = await (await get(`https://raw.githubusercontent.com/Crysthamus/tree-sitter-wasm/v${tswVersion}/pnpm-lock.yaml`)).text();
function pinned(pkg: string): string {
  const m = lock.match(new RegExp(`\\n {6}${pkg}:\\n {8}specifier: [^\\n]+\\n {8}version: ([^\\n]+)`));
  if (!m) throw new Error(`${pkg} not in the tree-sitter-wasm lockfile`);
  return m[1].trim();
}
const NPM: Record<string, string> = {
  c: 'tree-sitter-c',
  cpp: 'tree-sitter-cpp',
  java: 'tree-sitter-java',
  c_sharp: 'tree-sitter-c-sharp',
  php: 'tree-sitter-php',
  ruby: 'tree-sitter-ruby',
  swift: 'tree-sitter-swift',
  kotlin: 'tree-sitter-kotlin',
  bash: 'tree-sitter-bash',
  elixir: 'tree-sitter-elixir',
};
const tmp = mkdtempSync(join(tmpdir(), 'wi-licenses-'));
try {
  for (const grammar of Object.values(provenance.grammars)) {
    const npm = NPM[grammar];
    if (!npm) throw new Error(`no upstream mapping for grammar ${grammar}`);
    const version = pinned(npm);
    let tarball: string;
    let license: string;
    let source: string;
    if (version.startsWith('https://codeload.github.com/')) {
      tarball = version;
      const meta = (await (await get(`https://registry.npmjs.org/${npm}/latest`)).json()) as { license?: string };
      license = meta.license ?? 'see text';
      source = `${version} (pinned by tree-sitter-wasm ${tswVersion})`;
    } else {
      const meta = (await (await get(`https://registry.npmjs.org/${npm}/${version}`)).json()) as {
        license?: string;
        dist: { tarball: string };
      };
      tarball = meta.dist.tarball;
      license = meta.license ?? 'see text';
      source = `npm ${npm}@${version} (pinned by tree-sitter-wasm ${tswVersion})`;
    }
    const bytes = new Uint8Array(await (await get(tarball)).arrayBuffer());
    const dir = join(tmp, grammar);
    mkdirSync(dir);
    writeFileSync(join(dir, 'pkg.tgz'), bytes);
    const x = Bun.spawnSync(['tar', '-xzf', 'pkg.tgz'], { cwd: dir });
    if (x.exitCode !== 0) throw new Error(`tar ${tarball}: ${x.stderr}`);
    const top = readdirSync(dir).find((n) => n !== 'pkg.tgz');
    if (!top) throw new Error(`empty tarball ${tarball}`);
    record(
      `tree-sitter-${grammar}`,
      version.startsWith('https://') ? version.split('/').pop()! : version,
      license,
      source,
      licenseText(join(dir, top)),
    );
  }
} finally {
  rmSync(tmp, { recursive: true, force: true });
}

for (const f of existsSync(out) ? readdirSync(out) : []) rmSync(join(out, f), { recursive: true, force: true });
mkdirSync(out, { recursive: true });
for (const [file, text] of texts) writeFileSync(join(out, file), text);
writeFileSync(
  join(out, 'licenses.json'),
  `${JSON.stringify({ generator: 'scripts/oracle/bundle-licenses.ts', wrongstackCommit: oracle.commit, treeSitterWasm: tswVersion, components: entries }, null, 2)}\n`,
);
for (const e of entries) console.log(`${e.component} ${e.version} ${e.license}`);
