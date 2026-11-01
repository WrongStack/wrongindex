/**
 * WrongIndex compatibility worker (WI-300). Preview backend only.
 *
 * A long-lived Bun process owned and supervised by wrongindexd. It runs the
 * WrongStack indexer and query services IN-PROCESS from an explicitly
 * configured WrongStack engine directory. It never starts or elects a
 * WrongStack project server, never chooses what to index on its own, and
 * holds no ownership: the daemon owns the index and only sends write requests
 * while its ownership fence is valid.
 *
 * Protocol: one JSON object per line on stdin; one JSON object per line on
 * stdout. stderr is diagnostics only.
 *
 *   -> {"id": 1, "op": "hello"}
 *   <- {"id": 1, "ok": true, "result": {"engine": ..., "bun": ...}}
 *   -> {"id": 2, "op": "index", "args": {"projectRoot", "indexDir", "files"?, "force"?}}
 *   -> {"id": 3, "op": "query", "args": {"op": "search", "args": {...}}}
 *   -> {"id": 4, "op": "release", "args": {"projectRoot", "indexDir"}}   close pooled stores
 *
 * Usage: bun worker.ts --engine <wrongstack-dir>
 */
import { existsSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';

const i = process.argv.indexOf('--engine');
const engine = i >= 0 ? resolve(process.argv[i + 1]!) : '';
const base = join(engine, 'packages/tools/src/codebase-index');
if (!engine || !existsSync(join(base, 'indexer.ts'))) {
  console.error(`legacy-worker: no WrongStack engine at '${engine}'`);
  process.exit(2);
}

// Never let the engine spawn or elect its own project server or worker.
process.env['WRONGSTACK_INDEX_INLINE'] = '1';

const { IndexStore, indexStorePool } = await import(join(base, 'writer.ts'));
const { runIndexerWithStore } = await import(join(base, 'indexer.ts'));
const { callInline } = await import(join(base, 'index-inline.ts'));

const marker = join(engine, '.wrongindex-oracle.json');
const engineInfo = existsSync(marker)
  ? { kind: 'clean-archive', ...JSON.parse(readFileSync(marker, 'utf8')) }
  : { kind: 'checkout', path: engine };

const READ_OPS = new Set([
  'search',
  'stats',
  'packageGraph',
  'fileGraph',
  'symbolGraph',
  'incomingCalls',
  'outgoingCalls',
  'context',
  'vectorSearch',
]);

type Req = { id: number; op: string; args?: Record<string, unknown> };

function send(obj: unknown) {
  process.stdout.write(`${JSON.stringify(obj)}\n`);
}

async function handle(req: Req): Promise<unknown> {
  switch (req.op) {
    case 'hello':
      return { engine: engineInfo, bun: Bun.version, pid: process.pid };
    case 'index': {
      const a = req.args as { projectRoot: string; indexDir: string; files?: string[]; force?: boolean };
      // The pooled read store must not hold the database across a write run.
      indexStorePool.evict(a.projectRoot, a.indexDir);
      const store = new IndexStore(a.projectRoot, { indexDir: a.indexDir });
      try {
        const t0 = performance.now();
        const r = await runIndexerWithStore(store, {
          projectRoot: a.projectRoot,
          indexDir: a.indexDir,
          ...(a.files && a.files.length > 0 ? { files: a.files } : {}),
          ...(a.force ? { force: true } : {}),
        });
        const meta = (k: string) => store.getMetadata(k) ?? null;
        return {
          filesIndexed: r.filesIndexed,
          symbolsIndexed: r.symbolsIndexed,
          changedFiles: r.changedFiles ?? null,
          contentChanged: r.contentChanged ?? null,
          errors: r.errors,
          durationMs: Math.round(performance.now() - t0),
          graphStamp: meta('graph_stamp'),
          lastIndexed: meta('last_indexed'),
        };
      } finally {
        store.close();
      }
    }
    case 'query': {
      const q = req.args as { op: string; args: Record<string, unknown> };
      if (!READ_OPS.has(q.op)) throw new Error(`unsupported query op '${q.op}'`);
      return await callInline(q.op, q.args, { timeoutMs: 60_000 }, false);
    }
    case 'release': {
      const a = req.args as { projectRoot: string; indexDir: string };
      indexStorePool.evict(a.projectRoot, a.indexDir);
      return { released: true };
    }
    default:
      throw new Error(`unknown op '${req.op}'`);
  }
}

// Requests are processed strictly one at a time: SQLite access and the
// engine's module state are not shared across concurrent requests.
const rl = createInterface({ input: process.stdin });
let chain = Promise.resolve();
rl.on('line', (line) => {
  if (!line.trim()) return;
  let req: Req;
  try {
    req = JSON.parse(line);
  } catch {
    send({ id: null, ok: false, error: { name: 'ProtocolError', message: 'invalid JSON' } });
    return;
  }
  chain = chain.then(async () => {
    try {
      send({ id: req.id, ok: true, result: await handle(req) });
    } catch (err) {
      const e = err as Error;
      send({ id: req.id, ok: false, error: { name: e?.name ?? 'Error', message: String(e?.message ?? e) } });
    }
  });
});
rl.on('close', () => {
  chain.finally(() => {
    try {
      indexStorePool.closeAll();
    } catch {}
    process.exit(0);
  });
});
