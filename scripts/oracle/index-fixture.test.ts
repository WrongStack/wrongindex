import { expect, test } from 'bun:test';
import { mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./index-fixture.ts', import.meta.url));

function fixture() {
  const root = realpathSync.native(mkdtempSync(join(tmpdir(), '.tmp-wi-oracle-')));
  const base = join(root, 'engine/packages/tools/src/codebase-index');
  const projectRoot = join(root, 'project');
  const indexDir = join(root, 'index');
  mkdirSync(join(base, 'wrongindex'), { recursive: true });
  mkdirSync(projectRoot); mkdirSync(indexDir);
  // Synthetic external-producer double; never used as an actual baseline.
  writeFileSync(join(root, 'engine/.wrongindex-oracle.json'), JSON.stringify({ schema: 'wrongindex.clean-oracle/1', commit: '0'.repeat(40) }));
  writeFileSync(join(base, 'writer.ts'), `
    import {appendFileSync} from 'node:fs'; import {join} from 'node:path';
    export class IndexStore { constructor(_root,opts) {appendFileSync(join(opts.indexDir,'writes'),'write');} getMetadata(){return null;} close(){} }
    export const indexStorePool={evict(){},closeAll(){}};
  `);
  writeFileSync(join(base, 'indexer.ts'), 'export async function runIndexerWithStore(){return {filesIndexed:1,symbolsIndexed:1,errors:[]};}');
  writeFileSync(join(base, 'wrongindex/owner-lock.ts'), 'export function isOwnedByWrongIndex(){return false;} export function withWrongIndexWriteDelegation(){throw Error("delegation is required");}');
  return {
    root, projectRoot, indexDir,
    run(overrides: Record<string, unknown> = {}) {
      const r = spawnSync(process.execPath, [script, '--engine', join(root, 'engine')], {
        input: JSON.stringify({ id: 1, op: 'index', args: { projectRoot, indexDir, ...overrides } }) + '\n',
        encoding: 'utf8', windowsHide: true, timeout: 15000,
      });
      expect(r.status).toBe(0);
      return JSON.parse(r.stdout.trim());
    },
    close() {
      const canonical = realpathSync.native(root);
      if (dirname(canonical).toLowerCase() !== realpathSync.native(tmpdir()).toLowerCase() || !basename(canonical).startsWith('.tmp-wi-oracle-')) throw Error('Unsafe fixture cleanup');
      rmSync(canonical, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
    },
  };
}

test('indexes an unowned fixture without using the service delegation API', () => {
  const f = fixture();
  try { expect(f.run()).toMatchObject({ ok: true, result: { filesIndexed: 1 } }); expect(readFileSync(join(f.indexDir, 'writes'), 'utf8')).toBe('write'); } finally { f.close(); }
});

test('refuses an ownership lock before opening a writer, even with delegation data', () => {
  const f = fixture();
  try {
    writeFileSync(join(f.indexDir, '.wrongindex-owner.lock'), '');
    expect(f.run({ writeDelegation: { pid: process.pid, epoch: 1, instanceId: 'fixture' } }).ok).toBe(false);
    expect(existsSync(join(f.indexDir, 'writes'))).toBe(false);
  } finally { f.close(); }
});

test('refuses a project outside the disposable fixture root', () => {
  const f = fixture();
  try { expect(f.run({ projectRoot: process.cwd() }).ok).toBe(false); expect(existsSync(join(f.indexDir, 'writes'))).toBe(false); } finally { f.close(); }
});

test('refuses an index outside the disposable fixture root', () => {
  const f = fixture();
  try { expect(f.run({ indexDir: join(process.cwd(), 'not-an-oracle-index') }).ok).toBe(false); expect(existsSync(join(f.indexDir, 'writes'))).toBe(false); } finally { f.close(); }
});

test('requires recorded oracle provenance before writing', () => {
  const f = fixture();
  try {
    writeFileSync(join(f.root, 'engine/.wrongindex-oracle.json'), '{}');
    expect(f.run().ok).toBe(false); expect(existsSync(join(f.indexDir, 'writes'))).toBe(false);
  } finally { f.close(); }
});
