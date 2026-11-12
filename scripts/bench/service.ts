/** Reproducible disposable direct-adapter/MCP probe; never indexes the source checkout. */
import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const args = process.argv.slice(2);
const option = (key: string) => { const i = args.indexOf(key); return i < 0 ? undefined : args[i + 1]; };
const ws = resolve(option('--wrongstack') ?? '');
const daemonPath = resolve(option('--daemon') ?? 'target/release/wrongindexd.exe');
const files = Number(option('--files') ?? 500);
const repeats = Number(option('--repeats') ?? 5);
if (!option('--wrongstack') || !Number.isSafeInteger(files) || files < 1 || files > 100_000 || !Number.isSafeInteger(repeats) || repeats < 5 || repeats > 100) throw new Error('--wrongstack, --files 1..100000 and --repeats 5..100 required');
const work = mkdtempSync(join(os.tmpdir(), 'wi-service-bench-'));
const home = join(work, 'home');
const root = join(work, 'project');
const indexDir = join(work, 'index');
mkdirSync(root);
let sourceBytes = 0;
let largestFileBytes = 0;
for (let i = 0; i < files; i++) {
  const source = `export function entry${i}() { return ${i}; }\nexport function caller${i}() { return entry${i}(); }\n`;
  const size = Buffer.byteLength(source);
  sourceBytes += size;
  largestFileBytes = Math.max(largestFileBytes, size);
  writeFileSync(join(root, `unit${i}.ts`), source);
}
const service = await import(pathToFileURL(join(ws, 'packages/tools/src/codebase-index/wrongindex/service.ts')).href);
service.setIndexServiceMode('wrongindex', { dataRoot: home });
const daemon = spawn(daemonPath, ['--data-root', home, '--backend', 'native'], { windowsHide: true, stdio: 'ignore' });
const pause = (ms: number) => new Promise((r) => setTimeout(r, ms));
const opts = { timeoutMs: 240_000 };
let mcp: ReturnType<typeof spawn> | undefined;
const output: Record<string, unknown> = { schema: 'wrongindex.service-benchmark/1', work, files, repeats, corpus: 'generated TypeScript declarations and direct calls; no Git repository', machine: { platform: os.platform(), release: os.release(), cpus: os.cpus().length, memoryBytes: os.totalmem() }, daemonSha256: createHash('sha256').update(readFileSync(daemonPath)).digest('hex') };
output.workload = { sourceBytes, largestFileBytes, languages: { typescript: files }, expectedSymbols: files * 2, seed: 'sequential-unit-v1', ignores: [], dependencies: 'one same-file caller -> entry edge per file' };
output.environment = { cpu: os.cpus()[0]?.model, bun: Bun.version, storageAndAntivirus: 'not controlled; normal local Windows environment', measurement: 'synthetic functionality and latency probe; no baseline throughput comparison or total-RSS qualification' };
output.daemonBuildInfo = JSON.parse(execFileSync(daemonPath, ['--build-info'], { encoding: 'utf8' }));
const adapterFiles = ['client.ts', 'service.ts', 'publication.ts', 'owner-lock.ts'];
const sourceHashes = () => Object.fromEntries(adapterFiles.map((file) => [file, createHash('sha256').update(readFileSync(join(ws, 'packages/tools/src/codebase-index/wrongindex', file))).digest('hex')]));
output.adapterSourceSha256 = sourceHashes();
output.mcpArtifactSha256 = createHash('sha256').update(readFileSync(join(ws, 'packages/codebase-index-mcp/dist/cli.js'))).digest('hex');
output.cacheState = 'new corpus and empty index; source bytes may be cached by creation; two recorded setup calls reach both alternating read lanes before every warm query group';
output.editBoundary = 'write completed -> targeted adapter request -> search result verified; excludes watcher debounce';
const distribution = (samples: number[]) => {
  const ordered = [...samples].sort((a, b) => a - b);
  const at = (p: number) => ordered[Math.ceil(p * ordered.length) - 1];
  return { samples, p50: at(.5), p95: at(.95), p99: at(.99), max: ordered.at(-1) };
};
try {
  const deadline = Date.now() + 15_000;
  while (!existsSync(join(home, 'runtime/endpoint.json'))) { if (Date.now() >= deadline) throw new Error('daemon start deadline'); await pause(25); }
  const start = performance.now();
  await service.callWrongIndex('index', { projectRoot: root, indexDir }, opts);
  output.coldIndexMs = performance.now() - start;
  const operations = { search: { query: 'entry1', limit: 10 }, context: { query: 'entry1', limit: 5 }, incomingCalls: { symbol: 'entry1', limit: 10 }, outgoingCalls: { symbol: 'caller1', limit: 10 }, stats: {} };
  const direct: Record<string, unknown> = {};
  for (const [op, params] of Object.entries(operations)) {
    const warmupMs: number[] = [];
    let control: any;
    for (let lane = 0; lane < 2; lane++) {
      const t = performance.now();
      control = await service.callWrongIndex(op, { projectRoot: root, indexDir, ...params }, opts);
      warmupMs.push(performance.now() - t);
    }
    if (op === 'stats') {
      if (control.totalFiles !== files || control.totalSymbols !== files * 2) throw new Error('unexpected indexed corpus counts');
      output.indexStats = control;
    } else if (op === 'incomingCalls' || op === 'outgoingCalls') {
      if (!control.symbolFound || !control.calls?.length) throw new Error(`${op} did not resolve the fixture edge`);
    } else if (!JSON.stringify(control).includes('entry1')) throw new Error(`${op} did not find the fixture declaration`);
    const samples: number[] = [];
    for (let i = 0; i < repeats; i++) { const t = performance.now(); await service.callWrongIndex(op, { projectRoot: root, indexDir, ...params }, opts); samples.push(performance.now() - t); }
    direct[op] = { ...distribution(samples), warmupMs };
  }
  output.directMs = direct;
  const edits: number[] = [];
  for (let i = 0; i < repeats; i++) {
    const file = join(root, 'unit0.ts');
    writeFileSync(file, `export function changed${i}() { return ${i}; }\n`);
    const t = performance.now();
    await service.callWrongIndex('index', { projectRoot: root, indexDir, files: [file] }, opts);
    const hits = await service.callWrongIndex('search', { projectRoot: root, indexDir, query: `changed${i}`, limit: 5 }, opts);
    if (!JSON.stringify(hits).includes(`changed${i}`)) throw new Error('targeted edit was not searchable');
    edits.push(performance.now() - t);
  }
  output.targetedEditToSearchableMs = distribution(edits);
  mcp = spawn('bun', [join(ws, 'packages/codebase-index-mcp/dist/cli.js'), '--project-root', root, '--index-dir', indexDir], { windowsHide: true, env: { ...process.env, WRONGSTACK_INDEX_SERVICE: 'wrongindex', WRONGINDEX_HOME: home }, stdio: ['pipe', 'pipe', 'ignore'] });
  let buffer = ''; let id = 0;
  const waiters = new Map<number, (value: any) => void>();
  mcp.stdout!.on('data', (chunk) => {
    buffer += chunk.toString();
    if (buffer.length > 8 * 1024 * 1024) throw new Error('MCP response bound');
    let newline: number;
    while ((newline = buffer.indexOf('\n')) >= 0) { const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1); if (line.trim()) { const reply = JSON.parse(line); waiters.get(reply.id)?.(reply); } }
  });
  const rpc = (method: string, params: unknown) => new Promise<any>((resolveReply, reject) => {
    const requestId = ++id;
    const timer = setTimeout(() => { waiters.delete(requestId); reject(new Error(`MCP ${method} deadline`)); }, 30_000);
    waiters.set(requestId, (reply) => { clearTimeout(timer); waiters.delete(requestId); if (reply.error || reply.result?.isError) reject(new Error('MCP operation failed')); else resolveReply(reply); });
    mcp!.stdin!.write(`${JSON.stringify({ jsonrpc: '2.0', id: requestId, method, params })}\n`);
  });
  await rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'wrongindex-benchmark', version: '1' } });
  mcp.stdin!.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
  const samples: number[] = [];
  const mcpWarmupMs: number[] = [];
  for (let i = -2; i < repeats; i++) { const t = performance.now(); const reply = await rpc('tools/call', { name: 'codebase_search', arguments: { query: 'entry1', limit: 10 } }); if (!JSON.stringify(reply.result?.content).includes('entry1') || !reply.result?._meta?.['wrongindex/publications']?.length) throw new Error('MCP fixture result/publication metadata missing'); (i < 0 ? mcpWarmupMs : samples).push(performance.now() - t); }
  output.mcpSearchMs = distribution(samples);
  output.mcpWarmupMs = mcpWarmupMs;
  output.sourceStable = JSON.stringify(output.adapterSourceSha256) === JSON.stringify(sourceHashes());
  if (!output.sourceStable) throw new Error('adapter source changed during measurement');
  output.status = 'PASS';
} finally {
  mcp?.kill();
  try { service.resetWrongIndexConnection(); } finally { daemon.kill(); }
}
const out = resolve(option('--out') ?? 'target/service-benchmark.json');
mkdirSync(resolve(out, '..'), { recursive: true });
writeFileSync(out, `${JSON.stringify(output, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(output, null, 2)}\n`);
