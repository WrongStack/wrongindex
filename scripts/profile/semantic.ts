/**
 * WI-007: semantic profile manifest and the full profile fingerprint.
 *
 * The storage manifest says what the tables look like; it cannot say how the
 * engine fills them. This combines, from already generated contracts:
 *
 *   - storage manifest (DDL fingerprint, tables, metadata keys)
 *   - parser routes and helper backends (contracts/inventory/languages.json)
 *   - semantic version markers the baseline writes (ref/module/relation/rank
 *     versions observed in every oracle fixture; they must agree)
 *   - fingerprint algorithms (content hash, structural fingerprint)
 *   - the public query surface exercised by the oracle (op names + arg keys)
 *
 * into contracts/profiles/<id>/semantic.json with `profileFingerprint`, a
 * SHA-256 over the canonical JSON of the storage manifest's structural fields
 * and the semantic section. Clients negotiate the profile id; the fingerprint
 * proves both sides mean the same thing.
 *
 * Usage: bun scripts/profile/semantic.ts
 */
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const repo = resolve(import.meta.dir, '..', '..');
const profileDir = join(repo, 'contracts/profiles/wrongstack-index-v5-baseline');
const read = (p: string) => JSON.parse(readFileSync(p, 'utf8'));
const storage = read(join(profileDir, 'manifest.json'));
const languages = read(join(repo, 'contracts/inventory/languages.json'));

/** Canonical JSON: sorted keys, no whitespace. */
function canonical(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map(canonical).join(',')}]`;
  if (v && typeof v === 'object') {
    return `{${Object.keys(v as object)
      .sort()
      .map((k) => `${JSON.stringify(k)}:${canonical((v as Record<string, unknown>)[k])}`)
      .join(',')}}`;
  }
  return JSON.stringify(v);
}

const VERSION_KEYS = [
  'version',
  'ref_resolution_version',
  'ref_binding_version',
  'relation_graph_version',
  'module_resolution_version',
  'module_owner_version',
  'rank_version',
];
const oracleDir = join(repo, 'tests/fixtures/oracle');
const markers: Record<string, string> = {};
const opShapes: Record<string, Set<string>> = {};
for (const name of readdirSync(oracleDir).sort()) {
  const snap = read(join(oracleDir, name, 'snapshot.json'));
  for (const k of VERSION_KEYS) {
    const v = snap.metadata[k];
    if (v === undefined) throw new Error(`${name}: missing semantic marker ${k}`);
    if (markers[k] !== undefined && markers[k] !== v) {
      throw new Error(`semantic marker ${k} disagrees across fixtures: ${markers[k]} vs ${v}`);
    }
    markers[k] = v;
  }
  for (const op of read(join(oracleDir, name, 'queries.json')).operations) {
    opShapes[op.op] ??= new Set();
    for (const k of Object.keys(op.args)) opShapes[op.op]!.add(k);
  }
}

const semantic = {
  wrongstackCommit: languages.wrongstackCommit,
  versionMarkers: markers,
  parserRoutes: languages.routes,
  helperBackends: languages.helperBackends,
  routeSourceSha256: languages.sources,
  extensions: languages.extensions,
  specialFilenames: languages.specialFilenames,
  families: languages.families,
  fingerprints: {
    contentHash: storage.contentHash,
    structural: 'SHA-256 canonicalization over declarations and outgoing refs (fingerprint.ts); parity fixtures pending',
    signatureHash: 'files/symbols signature_hash per baseline writer; parity fixtures pending',
  },
  positions: {
    line: '1-based',
    column: '0-based UTF-16 code units of the decoded text; a UTF-8 BOM occupies column 0',
  },
  querySurface: Object.fromEntries(
    Object.entries(opShapes)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([op, keys]) => [op, [...keys].sort()]),
  ),
};

const storageStructural = {
  id: storage.id,
  schemaVersion: storage.schemaVersion,
  ddlSha256: storage.ddlSha256,
  tables: storage.tables,
  optionalTables: storage.optionalTables,
  virtualTables: storage.virtualTables,
  indexes: storage.indexes,
  metadataKeys: storage.metadataKeys,
  sqliteRequirements: storage.sqliteRequirements,
};
const storageFingerprint = createHash('sha256').update(canonical(storageStructural)).digest('hex');
const semanticFingerprint = createHash('sha256').update(canonical(semantic)).digest('hex');
const profileFingerprint = createHash('sha256')
  .update(canonical({ storage: storageFingerprint, semantic: semanticFingerprint }))
  .digest('hex');

writeFileSync(
  join(profileDir, 'semantic.json'),
  `${JSON.stringify(
    {
      schema: 'wrongindex.semantic-profile/1',
      id: storage.id,
      storageFingerprint,
      semanticFingerprint,
      profileFingerprint,
      semantic,
    },
    null,
    2,
  )}\n`,
);
console.error(`profile ${storage.id}: ${profileFingerprint.slice(0, 16)}… (storage ${storageFingerprint.slice(0, 12)}, semantic ${semanticFingerprint.slice(0, 12)})`);
