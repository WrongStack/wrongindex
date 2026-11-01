/**
 * WI-006 (seed): build oracle fixture indexes with the real WrongStack indexer.
 *
 * For every project under tests/fixtures/projects/<name>, this copies the tree
 * to a private temp directory, runs WrongStack's own `IndexStore` +
 * `runIndexerWithStore`, and writes:
 *
 *   tests/fixtures/oracle/<name>/index.db       checkpointed single-file index
 *   tests/fixtures/oracle/<name>/snapshot.json  semantic projection, paths relative
 *
 * It also writes the storage profile manifest and the content-hash known-answer
 * fixtures that the Rust storage crate is tested against.
 *
 * `--wrongstack` should be a clean oracle prepared by
 * `prepare-clean-wrongstack.ts` (identified by `.wrongindex-oracle.json`):
 * archived from the baseline commit with workspace dists built from that
 * source. A plain working-tree checkout is still accepted for quick local
 * experiments, but its fixtures are labelled `provisional`.
 *
 * Usage: bun scripts/oracle/build-fixtures.ts --wrongstack <path> [--work-root <dir>]
 */
import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/oracle/build-fixtures.ts --wrongstack <path>');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const workRoot = resolve(arg('--work-root') ?? (process.platform === 'win32' ? 'C:/wrongindex-oracle' : '/tmp/wrongindex-oracle'));
const baselinePath = join(repo, 'contracts/baseline/wrongstack-baseline.json');
if (!existsSync(baselinePath)) {
  console.error('run scripts/baseline/export.ts first');
  process.exit(2);
}
const baseline = JSON.parse(readFileSync(baselinePath, 'utf8')) as {
  commit: string;
  contractFilesDirty: string[];
};
const cleanMarker = join(resolve(wrongstack), '.wrongindex-oracle.json');
const clean = existsSync(cleanMarker)
  ? (JSON.parse(readFileSync(cleanMarker, 'utf8')) as { commit: string; [k: string]: unknown })
  : null;
const head = clean
  ? clean.commit
  : Bun.spawnSync(['git', 'rev-parse', 'HEAD'], { cwd: wrongstack }).stdout.toString().trim();
if (head !== baseline.commit) {
  console.error(`WrongStack HEAD ${head} does not match baseline ${baseline.commit}; re-export the baseline`);
  process.exit(1);
}

const base = `${resolve(wrongstack)}/packages/tools/src/codebase-index`;
const { IndexStore } = await import(`${base}/writer.ts`);
const { runIndexerWithStore } = await import(`${base}/indexer.ts`);
const { callInline } = await import(`${base}/index-inline.ts`);
const { indexStorePool } = await import(`${base}/writer.ts`);
const { xxhash64Hex, xxhash64String } = await import(`${base}/content-hash.ts`);

/** Metadata values that legitimately differ per run. */
const VOLATILE_METADATA = new Set(['last_indexed', 'graph_stamp', 'relation_epoch']);

type Row = Record<string, unknown>;

function rel(root: string, p: unknown): unknown {
  if (typeof p !== 'string') return p;
  if (!p.startsWith(root)) return p;
  return `<root>/${relative(root, p).split(sep).join('/')}`;
}

function snapshot(db: DatabaseSync, root: string) {
  const all = (sql: string) => db.prepare(sql).all() as Row[];
  const symbols = all('SELECT * FROM symbols ORDER BY id');
  const byId = new Map(symbols.map((s) => [s.id, s]));
  const symKey = (id: unknown) => {
    const s = byId.get(id);
    return s ? `${rel(root, s.file)}#${s.kind}:${s.scope ? `${s.scope}.` : ''}${s.name}@${s.line}:${s.col}` : null;
  };
  return {
    metadata: Object.fromEntries(
      all('SELECT key, value FROM metadata ORDER BY key').map((r) => [
        r.key,
        VOLATILE_METADATA.has(String(r.key)) ? '<volatile>' : r.value,
      ]),
    ),
    files: all('SELECT * FROM files ORDER BY file').map((f) => ({
      ...f,
      file: rel(root, f.file),
      mtime_ms: '<volatile>',
      last_indexed: '<volatile>',
    })),
    symbols: symbols.map((s) => ({ ...s, id: undefined, key: symKey(s.id), file: rel(root, s.file) })),
    refs: all('SELECT * FROM refs ORDER BY id').map((r) => ({
      from: symKey(r.from_id),
      to_name: r.to_name,
      to: r.to_id === null ? null : symKey(r.to_id),
      call_type: r.call_type,
      line: r.line,
      lang: r.lang,
      module: r.module,
      to_file: rel(root, r.to_file),
    })),
    file_rank: all('SELECT * FROM file_rank ORDER BY file').map((r) => ({ ...r, file: rel(root, r.file) })),
    symbol_rank: all('SELECT * FROM symbol_rank ORDER BY symbol_id').map((r) => ({
      symbol: symKey(r.symbol_id),
      rank: r.rank,
      in_deg: r.in_deg,
      out_deg: r.out_deg,
    })),
  };
}

function schemaObjects(db: DatabaseSync) {
  return (
    db
      .prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
      )
      .all() as Array<{ type: string; name: string; tbl_name: string; sql: string | null }>
  ).filter((o) => !/^symbols_fts_(config|content|data|docsize|idx)$/.test(o.name));
}

function tableColumns(db: DatabaseSync, table: string) {
  return (db.prepare(`PRAGMA table_xinfo(${table})`).all() as Row[]).map((c) => ({
    name: c.name,
    type: c.type,
    notnull: c.notnull === 1,
    default: c.dflt_value ?? null,
    pk: c.pk,
  }));
}

async function buildFixture(name: string, env: Record<string, string>) {
  const src = join(repo, 'tests/fixtures/projects', name);
  // Fixed, user-neutral location: indexed paths are absolute and end up in the
  // committed fixture, so they must not embed a home directory or random suffix.
  const work = join(workRoot, name);
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const root = join(work, 'project');
  const indexDir = join(work, 'index');
  cpSync(src, root, { recursive: true });
  const saved: Record<string, string | undefined> = {};
  for (const [k, v] of Object.entries(env)) {
    saved[k] = process.env[k];
    process.env[k] = v;
  }
  try {
    const store = new IndexStore(root, { indexDir });
    const result = await runIndexerWithStore(store, { projectRoot: root, indexDir });
    store.close();
    const dbPath = join(indexDir, 'index.db');
    const db = new DatabaseSync(dbPath);
    db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
    const snap = snapshot(db, root);
    const objects = schemaObjects(db);
    const columns = Object.fromEntries(
      objects.filter((o) => o.type === 'table' && o.name !== 'symbols_fts').map((o) => [o.name, tableColumns(db, o.name)]),
    );
    db.close();
    // Capture the published bytes before any query: opening an IndexStore runs
    // WrongStack's schema initializer, which is itself a potential mutation.
    const dbBytes = readFileSync(dbPath);
    // WI-004: public query operations, run twice to prove they are
    // deterministic over one database before recording them.
    const queries = await captureQueries(root, indexDir, snap);
    const again = await captureQueries(root, indexDir, snap);
    indexStorePool.closeAll();
    if (JSON.stringify(queries) !== JSON.stringify(again)) {
      throw new Error(`${name}: query results are not deterministic over the same database`);
    }
    return { root, work, dbPath, dbBytes, result, snap, objects, columns, queries };
  } finally {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  }
}

const sha256 = (data: string | Uint8Array) => createHash('sha256').update(data).digest('hex');

/** Values that legitimately differ per run (times), never semantic output. */
const VOLATILE_KEYS = /^(lastIndexed|last_indexed|durationMs|elapsedMs|indexedAt|generatedAt|mtimeMs|mtime_ms|timestamp)$/;

/** Make results machine-independent: project paths become `<root>/rel/path`. */
function normalize(value: unknown, root: string, key = ''): unknown {
  if (VOLATILE_KEYS.test(key) && (typeof value === 'number' || typeof value === 'string')) return '<volatile>';
  if (typeof value === 'string') {
    // Index dir is a sibling of the project root under the fixture work dir.
    const indexDir = join(root, '..', 'index');
    let out = value;
    for (const [needle, label] of [
      [root, '<root>'],
      [indexDir, '<indexDir>'],
    ] as const) {
      for (const v of [needle, needle.split(sep).join('/')]) {
        if (out.includes(v)) {
          out = out
            .split(v)
            .join(label)
            .replace(new RegExp(`${label}[^\\s"'),]*`, 'g'), (m) => m.split('\\').join('/'));
        }
      }
    }
    return out;
  }
  if (Array.isArray(value)) return value.map((v) => normalize(v, root));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, normalize(v, root, k)]));
  }
  return value;
}

type Snap = ReturnType<typeof snapshot>;

/** Query vectors derived from the fixture's own symbols (compatibility §8). */
function queryVectors(root: string, indexDir: string, snap: Snap): Array<{ op: string; args: Record<string, unknown> }> {
  const abs = (rel: string) => join(root, rel.replace(/^<root>\//, ''));
  const base = { projectRoot: root, indexDir };
  const vectors: Array<{ op: string; args: Record<string, unknown> }> = [];
  const names = [...new Set(snap.symbols.map((s) => String(s.name)))].sort();
  const files = [...new Set(snap.files.map((f) => String(f.file)))].sort();
  const packages = [...new Set(snap.files.map((f) => String(f.package ?? '')))].sort();
  const kinds = [...new Set(snap.symbols.map((s) => String(s.kind)))].sort();
  const langs = [...new Set(snap.symbols.map((s) => String(s.lang)))].sort();

  vectors.push({ op: 'stats', args: { ...base } });
  vectors.push({ op: 'packageGraph', args: { ...base } });
  for (const pkg of ['', ...packages.filter(Boolean)]) vectors.push({ op: 'fileGraph', args: { ...base, packageFilter: pkg } });
  for (const f of files) vectors.push({ op: 'symbolGraph', args: { ...base, fileFilter: abs(f) } });
  for (const q of [...names, '', 'zzzz_no_match', 'ru', names[0]?.slice(0, 3) ?? 'abc']) {
    vectors.push({ op: 'search', args: { ...base, query: q, limit: 10 } });
  }
  for (const kind of kinds) vectors.push({ op: 'search', args: { ...base, query: names[0] ?? 'a', kind, limit: 5 } });
  for (const lang of langs) vectors.push({ op: 'search', args: { ...base, query: names[0] ?? 'a', lang, limit: 5 } });
  if (files[0]) vectors.push({ op: 'search', args: { ...base, query: names[0] ?? 'a', file: abs(files[0]), limit: 5 } });
  vectors.push({ op: 'search', args: { ...base, query: names[0] ?? 'a', limit: 1 } });
  for (const sym of snap.symbols) {
    const name = String(sym.name);
    for (const transitive of [false, true]) {
      vectors.push({ op: 'incomingCalls', args: { ...base, symbol: name, transitive, limit: 50 } });
      vectors.push({ op: 'outgoingCalls', args: { ...base, symbol: name, transitive, limit: 50 } });
    }
    vectors.push({ op: 'incomingCalls', args: { ...base, symbol: name, file: abs(String(sym.file)), limit: 50 } });
  }
  vectors.push({ op: 'context', args: { ...base, query: names.slice(0, 3).join(' ') || 'x', limit: 5 } });
  vectors.push({ op: 'context', args: { ...base, query: 'zzzz_no_match', limit: 5 } });
  vectors.push({ op: 'vectorSearch', args: { ...base, vector: new Array(384).fill(0.01), limit: 5 } });
  return vectors;
}

async function captureQueries(root: string, indexDir: string, snap: Snap) {
  const out: Array<{ op: string; args: unknown; result?: unknown; error?: { name: string; message: string } }> = [];
  for (const { op, args } of queryVectors(root, indexDir, snap)) {
    const recordedArgs = normalize(Object.fromEntries(Object.entries(args).filter(([k]) => k !== 'projectRoot' && k !== 'indexDir')), root);
    try {
      const result = await callInline(op, args, { timeoutMs: 60_000 }, false);
      out.push({ op, args: recordedArgs, result: normalize(result, root) });
    } catch (err) {
      const e = err as Error;
      out.push({ op, args: recordedArgs, error: { name: e.name, message: String(normalize(e.message, root)) } });
    }
  }
  return out;
}

function probe(cmd: string[]): string | null {
  try {
    const r = Bun.spawnSync(cmd, { stdout: 'pipe', stderr: 'pipe' });
    if (r.exitCode !== 0) return null;
    return (r.stdout.toString() || r.stderr.toString()).trim().split('\n')[0] ?? null;
  } catch {
    return null;
  }
}

// Contract-file evidence: the dist actually imported by the oracle.
const coreDist = join(resolve(wrongstack), 'packages/core/dist/utils/index.js');
const provenance = {
  wrongstackCommit: baseline.commit,
  oracle: clean
    ? { kind: 'clean-archive', archiveSha256: clean.archiveSha256, bunLockSha256: clean.bunLockSha256, distSha256: clean.distSha256 }
    : { kind: 'working-tree', contractFilesDirty: baseline.contractFilesDirty },
  coreUtilsDistSha256: existsSync(coreDist) ? sha256(readFileSync(coreDist)) : null,
  bun: Bun.version,
  sqlite: (new DatabaseSync(':memory:').prepare('select sqlite_version() v').get() as { v: string }).v,
  // Go/Python parse output depends on these helper toolchains (WI-002).
  toolchains: {
    go: probe(['go', 'version']),
    python3: probe(['python3', '--version']),
    python: probe(['python', '--version']),
    py: probe(['py', '--version']),
  },
  platform: process.platform,
  provisional: !clean,
  ...(clean
    ? {}
    : {
        provisionalReason:
          'Built from a working tree whose @wrongstack/* dists are not proven to match the baseline commit; use prepare-clean-wrongstack.ts.',
      }),
};

const fixtures = readdirSync(join(repo, 'tests/fixtures/projects'), { withFileTypes: true })
  .filter((d) => d.isDirectory())
  .map((d) => d.name)
  .sort();

let profileSource: Awaited<ReturnType<typeof buildFixture>> | undefined;
let vectorColumns: unknown;
for (const name of fixtures) {
  const built = await buildFixture(name, {});
  const outDir = join(repo, 'tests/fixtures/oracle', name);
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  writeFileSync(join(outDir, 'index.db'), built.dbBytes);
  writeFileSync(
    join(outDir, 'snapshot.json'),
    `${JSON.stringify({ fixture: name, provenance, result: { filesIndexed: built.result.filesIndexed, symbolsIndexed: built.result.symbolsIndexed, errors: built.result.errors }, ...built.snap }, null, 2)}\n`,
  );
  writeFileSync(
    join(outDir, 'queries.json'),
    `${JSON.stringify({ fixture: name, provenance, operations: built.queries }, null, 2)}
`,
  );
  rmSync(built.work, { recursive: true, force: true });
  const failed = built.queries.filter((q) => q.error).length;
  console.error(`fixture ${name}: ${built.result.filesIndexed} files, ${built.result.symbolsIndexed} symbols, ${built.queries.length} queries (${failed} errored)`);
  profileSource ??= built;
}

// Optional symbol_vectors table only exists when WRONGSTACK_INDEX_VECTORS=1.
if (fixtures.length > 0) {
  const withVectors = await buildFixture(fixtures[0]!, { WRONGSTACK_INDEX_VECTORS: '1' });
  vectorColumns = withVectors.columns['symbol_vectors'];
  rmSync(withVectors.work, { recursive: true, force: true });
}

if (profileSource) {
  const ddl = profileSource.objects.map((o) => `${o.sql};`).join('\n');
  const profileDir = join(repo, 'contracts/profiles/wrongstack-index-v5-baseline');
  mkdirSync(profileDir, { recursive: true });
  writeFileSync(join(profileDir, 'schema.sql'), `-- Generated by scripts/oracle/build-fixtures.ts from WrongStack ${baseline.commit}\n${ddl}\n`);
  const manifest = {
    schema: 'wrongindex.profile/1',
    id: 'wrongstack-index-v5-baseline',
    provenance,
    schemaVersion: 5,
    ddlSha256: sha256(ddl),
    sqliteRequirements: { fts5: true, ftsTokenizer: 'trigram' },
    tables: profileSource.columns,
    optionalTables: { symbol_vectors: vectorColumns ?? null },
    virtualTables: profileSource.objects.filter((o) => o.type === 'table' && o.name === 'symbols_fts').map((o) => ({ name: o.name, sql: o.sql })),
    indexes: profileSource.objects.filter((o) => o.type === 'index').map((o) => ({ name: o.name, table: o.tbl_name, sql: o.sql })),
    // Keys observed in the oracle run plus keys found in writer/finalizer source.
    metadataKeys: [
      ...new Set([
        ...Object.keys(profileSource.snap.metadata),
        'file_vector_provider',
      ]),
    ].sort(),
    pragmas: {
      journal_mode: 'wal',
      synchronous: 'NORMAL',
      busy_timeout_ms: 15000,
      temp_store: 'MEMORY',
      foreign_keys: true,
      journal_size_limit: 67108864,
      wal_autocheckpoint: 1000,
    },
    contentHash: {
      algorithm: 'xxhash64',
      seed: 0,
      format: 'lowercase-hex-16',
      input: 'utf8-encode(utf8-decode-lossy(file bytes)); BOM retained',
    },
  };
  writeFileSync(join(profileDir, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
}

// Content-hash known answers, computed by WrongStack's own implementation the
// same way the indexer does: readFile(file, 'utf8') -> xxhash64String.
const enc = new TextEncoder();
const cases: Array<[string, Uint8Array]> = [
  ['empty', new Uint8Array()],
  ['ascii-1', enc.encode('a')],
  ['ascii-31', enc.encode('abcdefghijklmnopqrstuvwxyz01234')],
  ['ascii-32', enc.encode('abcdefghijklmnopqrstuvwxyz012345')],
  ['ascii-33', enc.encode('abcdefghijklmnopqrstuvwxyz0123456')],
  ['ascii-100', enc.encode('x'.repeat(100))],
  ['lf', enc.encode('line1\nline2\n')],
  ['crlf', enc.encode('line1\r\nline2\r\n')],
  ['utf8-bom', Uint8Array.from([0xef, 0xbb, 0xbf, ...enc.encode('const a = 1;\n')])],
  ['non-ascii', enc.encode('const çağrı = "İstanbul";\n')],
  ['astral', enc.encode('// 😀 𝔘𝔫𝔦𝔠𝔬𝔡𝔢\nexport const e = "🚀";\n')],
  ['invalid-lone-continuation', Uint8Array.from([0x61, 0x80, 0x62])],
  ['invalid-truncated-3byte', Uint8Array.from([0x61, 0xe2, 0x82, 0x62])],
  ['invalid-overlong', Uint8Array.from([0xc0, 0xaf, 0x41])],
  ['invalid-surrogate', Uint8Array.from([0xed, 0xa0, 0x80, 0x41])],
  ['invalid-trailing', Uint8Array.from([0x41, 0xf0, 0x9f, 0x98])],
  ['nul-bytes', Uint8Array.from([0x00, 0x01, 0x00, 0x41])],
];
const kat = {
  schema: 'wrongindex.content-hash-kat/1',
  provenance,
  note: 'fileHash = xxhash64String(Buffer.from(bytes).toString("utf8")) as used by indexer-batch-read; rawHash = xxhash64Hex(bytes).',
  cases: cases.map(([name, bytes]) => ({
    name,
    bytesHex: Buffer.from(bytes).toString('hex'),
    rawHash: xxhash64Hex(bytes),
    fileHash: xxhash64String(Buffer.from(bytes).toString('utf8')),
  })),
};
mkdirSync(join(repo, 'contracts/fixtures'), { recursive: true });
writeFileSync(join(repo, 'contracts/fixtures/content-hash-kat.json'), `${JSON.stringify(kat, null, 2)}\n`);
console.error(`wrote profile manifest, ${fixtures.length} oracle fixtures, ${cases.length} hash KATs (${clean ? 'clean oracle' : 'provisional'})`);
