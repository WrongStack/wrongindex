/**
 * WI-002: language / extension / backend / helper-process inventory.
 *
 * Generated from the clean oracle's archived sources so it cannot drift from
 * the baseline silently:
 *
 *   - extension -> language map and special filenames (languages.ts)
 *   - language families (LANG_FAMILY_ENTRIES)
 *   - dispatch routes: `case` groups in parser-dispatch.ts `dispatch()` and the
 *     parser modules each group loads, in fallback order
 *   - every `process.env[...]` switch read by index code
 *   - every child-process spawn site in index code
 *   - helper toolchains present on this machine (they change baseline output:
 *     without `go` / Python, .go / .py fall back to the regex extractor)
 *
 * Usage: bun scripts/inventory/languages.ts --wrongstack <clean-oracle-dir>
 */
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/inventory/languages.ts --wrongstack <clean-oracle-dir>');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const marker = join(resolve(wrongstack), '.wrongindex-oracle.json');
if (!existsSync(marker)) {
  console.error('expected a clean oracle prepared by scripts/oracle/prepare-clean-wrongstack.ts');
  process.exit(2);
}
const oracle = JSON.parse(readFileSync(marker, 'utf8')) as { commit: string };
const dir = join(resolve(wrongstack), 'packages/tools/src/codebase-index');
const sha256 = (s: string | Uint8Array) => createHash('sha256').update(s).digest('hex');
const source = (f: string) => readFileSync(join(dir, f), 'utf8');

const languages = await import(join(dir, 'languages.ts'));
const extToLang = languages.EXT_TO_LANG as Record<string, string>;
const families = Object.fromEntries(languages.LANG_FAMILY_ENTRIES as Array<[string, string]>);

// SPECIAL_FILENAMES is module-private: recover it from source, then verify
// each entry through the exported detectLang so a parse error cannot invent one.
const langSrc = source('languages.ts');
const specialBlock = /const SPECIAL_FILENAMES[^{]*\{([\s\S]*?)\};/.exec(langSrc)?.[1] ?? '';
const specialFilenames: Record<string, string> = {};
for (const m of specialBlock.matchAll(/'?([\w.-]+)'?\s*:\s*'(\w+)'/g)) {
  const [, name, lang] = m as unknown as [string, string, string];
  if (languages.detectLang(`/x/${name}`) !== lang) {
    throw new Error(`special filename ${name} did not verify via detectLang`);
  }
  specialFilenames[name] = lang;
}

// Dispatch routes: `case` groups of the dispatch() switch and the modules they load.
const dispatchSrc = source('parser-dispatch.ts');
const fnStart = dispatchSrc.indexOf('async function dispatch(');
const fnBody = dispatchSrc.slice(fnStart, dispatchSrc.indexOf('\n}\n', fnStart));
const routes: Array<{ langs: string[]; modules: string[]; default?: true }> = [];
let pending: string[] = [];
for (const segment of fnBody.split(/\n\s*(?=case '|default:)/)) {
  const caseLabels = [...segment.matchAll(/^\s*case '([\w]+)':/gm)].map((m) => m[1]!);
  const isDefault = /^\s*default:/.test(segment);
  const modules = [...segment.matchAll(/import\('\.\/([\w-]+)\.js'\)/g)].map((m) => m[1]!);
  pending.push(...caseLabels);
  if (modules.length > 0) {
    routes.push(isDefault ? { langs: [], modules, default: true } : { langs: pending, modules });
    pending = [];
  }
}
const routed = new Set(routes.flatMap((r) => r.langs));
const mappedLangs = [...new Set([...Object.values(extToLang), ...Object.values(specialFilenames)])].sort();

// Environment switches and child-process spawn sites across index code.
const files = readdirSync(dir).filter((f) => f.endsWith('.ts')).sort();
const envSwitches: Record<string, string[]> = {};
const spawnSites: Array<{ file: string; line: number; text: string }> = [];
for (const f of files) {
  const lines = source(f).split('\n');
  lines.forEach((line, i) => {
    if (/^\s*(\/\/|\*)/.test(line)) return;
    for (const m of line.matchAll(/process\.env\[\s*'([A-Z0-9_]+)'\s*\]|process\.env\.([A-Z0-9_]+)/g)) {
      const key = (m[1] ?? m[2])!;
      (envSwitches[key] ??= []).includes(f) || envSwitches[key]!.push(f);
    }
    for (const m of line.matchAll(/\b(const\s+([A-Z][A-Z0-9_]+)\s*=\s*'(WRONGSTACK_[A-Z0-9_]+)')/g)) {
      const key = m[3]!;
      (envSwitches[key] ??= []).includes(f) || envSwitches[key]!.push(f);
    }
    // Real child-process launches only: not RegExp#exec or db.exec.
    if (/(?<![.\w])(spawn|spawnSync|execFile|execFileSync)\s*\(|Bun\.spawn(Sync)?\s*\(/.test(line) && !/import\s/.test(line)) {
      spawnSites.push({ file: f, line: i + 1, text: line.trim().slice(0, 160) });
    }
  });
}

/** Helper toolchains that change baseline parse output when present/absent. */
function probe(cmd: string[]): string | null {
  try {
    const r = Bun.spawnSync(cmd, { stdout: 'pipe', stderr: 'pipe' });
    if (r.exitCode !== 0) return null;
    return (r.stdout.toString() || r.stderr.toString()).trim().split('\n')[0] ?? null;
  } catch {
    return null;
  }
}
const toolchains = {
  go: probe(['go', 'version']),
  python3: probe(['python3', '--version']),
  python: probe(['python', '--version']),
  py: probe(['py', '--version']),
  git: probe(['git', '--version']),
};

const inventory = {
  schema: 'wrongindex.language-inventory/1',
  wrongstackCommit: oracle.commit,
  sources: Object.fromEntries(
    ['languages.ts', 'parser-dispatch.ts', 'parser-batch.ts', 'go-parser.ts', 'py-parser.ts', 'rs-parser.ts', 'ts-parser.ts', 'tree-sitter-parser.ts', 'generic-parser.ts', 'json-parser.ts', 'yaml-parser.ts', 'import-extractor.ts']
      .filter((f) => existsSync(join(dir, f)))
      .map((f) => [f, sha256(readFileSync(join(dir, f)))]),
  ),
  extensions: Object.fromEntries(Object.entries(extToLang).sort(([a], [b]) => a.localeCompare(b))),
  declarationExtensions: { '.d.ts': 'ts', '.d.mts': 'ts', '.d.cts': 'ts' },
  specialFilenames,
  families,
  routes,
  languagesWithoutDedicatedRoute: mappedLangs.filter((l) => !routed.has(l)),
  helperBackends: {
    'go-parser': 'child process: `go run` of WrongStack-owned helper source (batched via parser-batch); regex fallback when unavailable',
    'py-parser': 'child process: python3 -> python -> py (Windows) running WrongStack-owned helper; regex fallback when unavailable',
    'ts-parser': 'in-process TypeScript compiler API',
    'rs-parser': 'in-process (no cargo subprocess at this commit)',
    'tree-sitter-parser': 'in-process WASM grammars; regex fallback on load failure or zero symbols',
    'generic-parser': 'in-process regex extractor',
    'json-parser': 'in-process',
    'yaml-parser': 'in-process',
  },
  envSwitches: Object.fromEntries(Object.entries(envSwitches).sort(([a], [b]) => a.localeCompare(b))),
  spawnSites,
  environment: { platform: process.platform, toolchains },
  note: 'Baseline output for go/py depends on the helper toolchains; fixtures record which were present.',
};

mkdirSync(join(repo, 'contracts/inventory'), { recursive: true });
writeFileSync(join(repo, 'contracts/inventory/languages.json'), `${JSON.stringify(inventory, null, 2)}\n`);
console.error(
  `inventory: ${Object.keys(extToLang).length} extensions, ${mappedLangs.length} languages, ${routes.length} routes, ` +
    `${Object.keys(envSwitches).length} env switches, ${spawnSites.length} spawn sites`,
);
