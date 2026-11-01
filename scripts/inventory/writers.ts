/**
 * WI-003: inventory of every index writer and mutable constructor.
 *
 * Production write takeover requires that EVERY WrongStack path able to
 * mutate `index.db` honors external ownership (compatibility §6). This script
 * enumerates them at the immutable baseline commit with `git grep <commit>`
 * across the whole monorepo (not only the packages the oracle archives):
 *
 *   1. Mutating IndexStore methods, derived from the class hierarchy's method
 *      names (constructor included: opening a store runs the schema
 *      initializer, which can drop/repair/rebuild).
 *   2. Entry points that reach those methods (store construction/pool,
 *      indexer runs, index services, background/startup/server launchers,
 *      enrichment and embedding passes).
 *   3. Every call site of 1 and 2 outside test files, grouped by package.
 *
 * Name-based classification is a heuristic; the output lists the evidence so
 * a reviewer can confirm each site. Unclassified methods are listed too.
 *
 * Usage: bun scripts/inventory/writers.ts --wrongstack <checkout> --oracle <clean-oracle-dir>
 */
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const checkout = arg('--wrongstack');
const oracleDir = arg('--oracle');
if (!checkout || !oracleDir) {
  console.error('usage: bun scripts/inventory/writers.ts --wrongstack <checkout> --oracle <clean-oracle-dir>');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const oracle = JSON.parse(readFileSync(join(oracleDir, '.wrongindex-oracle.json'), 'utf8')) as { commit: string };
const commit = oracle.commit;
const ci = join(oracleDir, 'packages/tools/src/codebase-index');
const src = (f: string) => readFileSync(join(ci, f), 'utf8');

// 1. Methods of IndexStore and its base layers.
const classFiles = ['writer.ts', 'writer-store-derived.ts', 'writer-store-base.ts'];
const methods = new Set<string>();
for (const f of classFiles) {
  for (const m of src(f).matchAll(/^ {2}(?:async |override |protected |public |static )*([a-z][A-Za-z0-9]+)\(/gm)) {
    methods.add(m[1]!);
  }
}
const MUTATING = /^(insert|delete|clear|upsert|replace|set|commit|begin|rollback|resolve|bind|apply|allocate|mark|prune|reconcile|record|invalidateIncoming|compact|optimize|checkpoint|defer|restore|initSchema|runAtomic)/;
const READ_ONLY = /^(get|find|search|count|has|data|stmt|close|invalidateBm25$|.*Host$)/;
const mutatingMethods = [...methods].filter((m) => MUTATING.test(m)).sort();
const readMethods = [...methods].filter((m) => !MUTATING.test(m) && READ_ONLY.test(m)).sort();
const unclassified = [...methods].filter((m) => !MUTATING.test(m) && !READ_ONLY.test(m) && m !== 'constructor').sort();

// 2. Entry points: exported functions of modules that run or schedule writes.
function exportedFunctions(file: string): string[] {
  return [...src(file).matchAll(/^export (?:async )?function ([A-Za-z0-9_]+)/gm)].map((m) => m[1]!);
}
const entryModules = [
  'indexer.ts',
  'index-service.ts',
  'background-indexer.ts',
  'index-inline.ts',
  'concept-enrichment.ts',
  'embedding-pass.ts',
  'graph-rank-pass.ts',
  'project-server-launch.ts',
];
const entryPoints: Record<string, string[]> = {};
for (const f of entryModules) {
  try {
    entryPoints[f] = exportedFunctions(f);
  } catch {
    entryPoints[f] = [];
  }
}

// 3. Call sites across the monorepo at the baseline commit.
function gitGrep(pattern: string): Array<{ path: string; line: number; text: string }> {
  const r = Bun.spawnSync(
    ['git', 'grep', '-n', '-I', '-E', pattern, commit, '--', '*.ts', '*.tsx', '*.mts', '*.cts', ':!**/tests/**', ':!**/*.test.ts', ':!**/*.spec.ts', ':!**/__tests__/**'],
    { cwd: checkout, stdout: 'pipe', stderr: 'pipe' },
  );
  if (r.exitCode !== 0 && r.exitCode !== 1) throw new Error(r.stderr.toString());
  return r.stdout
    .toString()
    .split('\n')
    .filter(Boolean)
    .map((l) => {
      const rest = l.slice(commit.length + 1);
      const m = /^(.*?):(\d+):(.*)$/.exec(rest)!;
      return { path: m[1]!, line: Number(m[2]), text: m[3]!.trim().slice(0, 160) };
    });
}

const packageOf = (p: string) => /^(packages|apps)\/([^/]+)/.exec(p)?.slice(1, 3).join('/') ?? p.split('/')[0]!;
const inIndexModule = (p: string) => p.startsWith('packages/tools/src/codebase-index/');

type Site = { path: string; line: number; text: string; via: string };
const sites: Site[] = [];
const constructionPatterns: Record<string, string> = {
  'new IndexStore': String.raw`new IndexStore\(`,
  'indexStorePool.acquire': String.raw`indexStorePool\.acquire\(`,
};
for (const [via, pattern] of Object.entries(constructionPatterns)) {
  for (const s of gitGrep(pattern)) sites.push({ ...s, via });
}
const entryNames = [...new Set(Object.values(entryPoints).flat())].sort();
for (const name of entryNames) {
  for (const s of gitGrep(String.raw`(^|[^A-Za-z0-9_.])${name}\(`)) {
    if (/^\s*(export )?(async )?function /.test(s.text)) continue; // the definition itself
    sites.push({ ...s, via: name });
  }
}
const mutatingCallPattern = String.raw`\.(${mutatingMethods.join('|')})\(`;
for (const s of gitGrep(mutatingCallPattern)) {
  const method = new RegExp(mutatingCallPattern).exec(s.text)?.[1] ?? '?';
  // Generic names (setMetadata on unrelated objects, resolve on promises)
  // outside index code are kept but flagged for review.
  sites.push({ ...s, via: `.${method}()` });
}

/** Entry points that only read (queries, state observers). */
const READ_ENTRY = /^(get|search|check|is|has|on[A-Z])|(Graph|Calls|Stats)Service(Inline)?$|^(searchService|contextService|vectorSearchService|statsService)/;
const entryKind = (name: string) => (READ_ENTRY.test(name) ? 'read' : 'write');

// A site outside the index module counts as index-related only if its file
// imports the index code; otherwise a same-named method/function on an
// unrelated object is the likelier explanation.
const fileCache = new Map<string, string>();
function fileText(path: string): string {
  if (!fileCache.has(path)) {
    const r = Bun.spawnSync(['git', 'show', `${commit}:${path}`], { cwd: checkout, stdout: 'pipe' });
    fileCache.set(path, r.stdout.toString());
  }
  return fileCache.get(path)!;
}
const importsIndex = (path: string) => /codebase-index|@wrongstack\/tools(['"/])/.test(fileText(path));

const external = sites
  .filter((s) => !inIndexModule(s.path))
  .map((s) => {
    const name = s.via.replace(/^\.|\(\)$/g, '');
    const kind = s.via.startsWith('.') || s.via.startsWith('new ') || s.via.includes('acquire') ? 'write' : entryKind(name);
    return { ...s, kind, evidence: importsIndex(s.path) ? 'imports-index' : 'name-collision-suspected' };
  });
const byPackage: Record<string, Record<string, number>> = {};
for (const s of external) {
  if (s.kind !== 'write' || s.evidence !== 'imports-index') continue;
  const pkg = packageOf(s.path);
  byPackage[pkg] ??= {};
  byPackage[pkg][s.via] = (byPackage[pkg][s.via] ?? 0) + 1;
}

const out = {
  schema: 'wrongindex.writer-inventory/1',
  wrongstackCommit: commit,
  method: 'git grep at the baseline commit, test files excluded; name-based classification for review',
  storeClassFiles: classFiles,
  mutatingMethods,
  readMethods,
  unclassifiedMethods: unclassified,
  constructorIsMutation:
    'IndexStore construction runs initIndexSchema: version-mismatch DROP, column repair, FTS rebuild, symbol_vectors drop when the vector flag is off.',
  entryPoints,
  entryPointKinds: Object.fromEntries(entryNames.map((n) => [n, entryKind(n)])),
  // Write-capable sites in files that import index code: the adoption surface.
  summaryByPackage: byPackage,
  suspectedNameCollisions: external.filter((s) => s.evidence !== 'imports-index').length,
  externalSites: external.sort((a, b) => a.path.localeCompare(b.path) || a.line - b.line),
  indexModuleSiteCount: sites.length - external.length,
};
mkdirSync(join(repo, 'contracts/inventory'), { recursive: true });
writeFileSync(join(repo, 'contracts/inventory/writers.json'), `${JSON.stringify(out, null, 2)}\n`);
console.error(
  `writers: ${mutatingMethods.length} mutating methods, ${entryNames.length} entry points, ` +
    `${external.length} external sites (${out.suspectedNameCollisions} suspected name collisions), ` +
    `write-capable index consumers in ${Object.keys(byPackage).length} packages, ${out.indexModuleSiteCount} inside codebase-index`,
);
