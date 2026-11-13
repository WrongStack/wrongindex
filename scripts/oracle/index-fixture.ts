/** Reference indexing of disposable Rust-test fixtures, never a service worker. */
import { existsSync, readFileSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { createInterface } from 'node:readline';

const i = process.argv.indexOf('--engine');
const engine = i >= 0 ? resolve(process.argv[i + 1]!) : '';
const normal = (p: string) => p.replace(/^\\\\\?\\/, '');
const temporary = normal(realpathSync.native(tmpdir()));

function fixture(p: unknown): string {
  if (typeof p !== 'string' || !isAbsolute(p)) throw new Error('Absolute fixture path required');
  let ancestor = resolve(p);
  const missing: string[] = [];
  while (!existsSync(ancestor)) {
    const parent = dirname(ancestor);
    if (parent === ancestor) throw new Error('Cannot resolve fixture ancestor');
    missing.unshift(basename(ancestor));
    ancestor = parent;
  }
  const canonical = resolve(normal(realpathSync.native(ancestor)), ...missing);
  const rel = relative(temporary, canonical);
  if (isAbsolute(rel) || rel === '..' || rel.startsWith(`..${sep}`) || !rel.split(sep)[0]?.startsWith('.tmp')) {
    throw new Error('Oracle indexing is restricted to disposable .tmp fixtures');
  }
  return join(temporary, rel.split(sep)[0]!);
}

async function index(args: Record<string, unknown>) {
  const provenance = JSON.parse(readFileSync(join(engine, '.wrongindex-oracle.json'), 'utf8'));
  if (provenance.schema !== 'wrongindex.clean-oracle/1' || !/^[a-f0-9]{40}$/.test(provenance.commit)) {
    throw new Error('A recorded WrongStack oracle is required');
  }
  fixture(args.projectRoot);
  fixture(args.indexDir);
  const projectRoot = args.projectRoot as string;
  const indexDir = args.indexDir as string;
  // Reference jobs never use delegation to enter an index held by a service.
  if (existsSync(join(indexDir, '.wrongindex-owner.lock'))) throw new Error('Oracle fixture has an ownership lock');
  process.env['WRONGSTACK_INDEX_INLINE'] = '1';
  const base = join(engine, 'packages/tools/src/codebase-index');
  const ownerPath = join(base, 'wrongindex/owner-lock.ts');
  const owner = existsSync(ownerPath) ? await import(ownerPath) : null;
  if (owner?.isOwnedByWrongIndex?.(indexDir)) throw new Error('Oracle fixture is owned by a service');
  const { IndexStore, indexStorePool } = await import(join(base, 'writer.ts'));
  const { runIndexerWithStore } = await import(join(base, 'indexer.ts'));
  indexStorePool.evict(projectRoot, indexDir);
  const store = new IndexStore(projectRoot, { indexDir });
  try {
    const started = performance.now();
    const result = await runIndexerWithStore(store, {
      projectRoot, indexDir,
      ...(Array.isArray(args.files) && args.files.length ? { files: args.files } : {}),
      ...(args.force ? { force: true } : {}),
    });
    return {
      filesIndexed: result.filesIndexed, symbolsIndexed: result.symbolsIndexed,
      changedFiles: result.changedFiles ?? null, contentChanged: result.contentChanged ?? null,
      errors: result.errors, durationMs: Math.round(performance.now() - started),
      graphStamp: store.getMetadata('graph_stamp') ?? null, lastIndexed: store.getMetadata('last_indexed') ?? null,
    };
  } finally {
    store.close();
    indexStorePool.closeAll();
  }
}

const rl = createInterface({ input: process.stdin });
let chain = Promise.resolve();
rl.on('line', (line) => {
  if (!line.trim()) return;
  chain = chain.then(async () => {
    let id: unknown = null;
    try {
      const request = JSON.parse(line);
      id = request.id;
      if (request.op !== 'index') throw new Error('Only fixture index requests are supported');
      process.stdout.write(`${JSON.stringify({ id, ok: true, result: await index(request.args) })}\n`);
    } catch (error) {
      const e = error as Error;
      process.stdout.write(`${JSON.stringify({ id, ok: false, error: { name: e.name, message: e.message } })}\n`);
    }
  });
});
rl.on('close', () => { void chain.finally(() => process.exit(0)); });
