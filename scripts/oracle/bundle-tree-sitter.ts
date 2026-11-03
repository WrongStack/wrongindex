/**
 * Vendor WrongStack's tree-sitter route for the native engine: its visitor
 * and per-language query tables (`tree-sitter/visitor.ts`, `queries.ts`,
 * `query-extractors.ts`, `util.ts`) and the exact `web-tree-sitter` runtime it
 * uses, bundled into one program, plus the vendored grammar WASM files.
 *
 *   bun scripts/oracle/bundle-tree-sitter.ts --wrongstack <clean oracle>
 *
 * Writes `crates/wrongindex-engine/tree-sitter/`: `ts_batch.mjs`,
 * `tree-sitter-runtime.wasm`, `<grammar>/tree-sitter-<grammar>.wasm` for the
 * ten languages WrongStack routes through tree-sitter, and `provenance.json`
 * (oracle commit, source hashes, sha256 of every output). The program reads
 * `[{file, content, lang}]` on stdin and prints one envelope; per file it
 * does what `tree-sitter-parser.ts` `parseSymbols` does. Do not edit the
 * outputs by hand.
 */
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: --wrongstack <clean oracle>');
  process.exit(2);
}
const marker = join(resolve(wrongstack), '.wrongindex-oracle.json');
if (!existsSync(marker)) {
  console.error('--wrongstack must be a clean oracle (missing .wrongindex-oracle.json)');
  process.exit(2);
}
const oracle = JSON.parse(readFileSync(marker, 'utf8')) as { commit: string; archiveSha256: string };
const base = `${resolve(wrongstack)}/packages/tools/src/codebase-index`;
const repo = resolve(import.meta.dir, '..', '..');
const out = join(repo, 'crates/wrongindex-engine/tree-sitter');
const sha = (b: string | Buffer | Uint8Array) => createHash('sha256').update(b).digest('hex');

/** `LANG_TO_GRAMMAR` of `tree-sitter-parser.ts` (default, no opt-ins). */
const GRAMMARS: Record<string, string> = {
  c: 'c',
  cpp: 'cpp',
  java: 'java',
  csharp: 'c_sharp',
  php: 'php',
  ruby: 'ruby',
  swift: 'swift',
  kotlin: 'kotlin',
  shell: 'bash',
  elixir: 'elixir',
};

const webTreeSitter = Bun.resolveSync('web-tree-sitter', base);
const fwd = (p: string) => p.split('\\').join('/');
const entry = `
import { Parser, Language } from ${JSON.stringify(fwd(webTreeSitter))};
import { visitTree } from ${JSON.stringify(fwd(`${base}/tree-sitter/visitor.ts`))};
import { getQueries } from ${JSON.stringify(fwd(`${base}/tree-sitter/queries.ts`))};
import * as path from 'node:path';

const GRAMMARS = ${JSON.stringify(GRAMMARS)};
const dir = process.argv[2];
const wf = (v) => (typeof v === 'string' ? v.toWellFormed() : v);

async function readStdin() {
  const chunks = [];
  for await (const c of process.stdin) chunks.push(c);
  return Buffer.concat(chunks).toString('utf8');
}

async function main() {
  const input = JSON.parse(await readStdin());
  await Parser.init({ locateFile: () => path.join(dir, 'tree-sitter-runtime.wasm') });
  const languages = new Map();
  const results = [];
  for (const { file, content, lang } of input) {
    // parseSymbols: any failure -> no symbols (the dispatcher then falls back
    // to the regex extractor).
    let result = { file, symbols: [], refs: [] };
    try {
      const grammar = GRAMMARS[lang];
      if (!grammar) throw new Error('no grammar for ' + lang);
      let language = languages.get(grammar);
      if (!language) {
        language = await Language.load(path.join(dir, grammar, 'tree-sitter-' + grammar + '.wasm'));
        languages.set(grammar, language);
      }
      const parser = new Parser();
      let tree = null;
      try {
        parser.setLanguage(language);
        tree = parser.parse(content);
        if (tree) {
          const v = visitTree(tree, content, file, lang, getQueries(lang));
          result = {
            file,
            symbols: v.symbols.map((s) => ({
              kind: s.kind, name: wf(s.name), line: s.line, col: s.col, signature: wf(s.signature),
              docComment: wf(s.docComment), scope: wf(s.scope), text: wf(s.text),
              isExported: s.isExported, endLine: s.endLine,
            })),
            refs: (v.refs ?? []).map((r) => ({
              toName: wf(r.toName), callType: r.callType, line: r.line, module: wf(r.module),
            })),
          };
        }
      } finally {
        tree?.delete();
        parser.delete();
      }
    } catch (e) {
      result = { file, symbols: [], refs: [], error: String(e && e.message || e) };
    }
    results.push(result);
  }
  process.stdout.write(JSON.stringify({ version: 1, results }));
}

main().catch((e) => {
  process.stderr.write(String(e && e.stack || e));
  process.exit(1);
});
`;

/**
 * WrongStack's TypeScript-compiler parser (`ts-parser.ts` + the
 * `@typescript/typescript6` it loads), for the TS/JS files the native oxc
 * port refuses (syntax TypeScript recovers from, script-level \`await\`,
 * lone surrogates): same stdin/stdout contract as the tree-sitter program.
 */
const tsEntry = `
import { parseSymbols } from ${JSON.stringify(fwd(`${base}/ts-parser.ts`))};
const wf = (v) => (typeof v === 'string' ? v.toWellFormed() : v);
async function readStdin() {
  const chunks = [];
  for await (const c of process.stdin) chunks.push(c);
  return Buffer.concat(chunks).toString('utf8');
}
async function main() {
  const input = JSON.parse(await readStdin());
  const results = [];
  for (const { file, content, lang } of input) {
    const v = await parseSymbols({ file, content, lang });
    results.push({
      file,
      symbols: v.symbols.map((s) => ({
        kind: s.kind, name: wf(s.name), line: s.line, col: s.col, signature: wf(s.signature),
        docComment: wf(s.docComment), scope: wf(s.scope), text: wf(s.text),
        isExported: s.isExported, endLine: s.endLine,
      })),
      refs: (v.refs ?? []).map((r) => ({
        toName: wf(r.toName), callType: r.callType, line: r.line, module: wf(r.module),
      })),
    });
  }
  process.stdout.write(JSON.stringify({ version: 1, results }));
}
main().catch((e) => {
  process.stderr.write(String(e && e.stack || e));
  process.exit(1);
});
`;

// A fixed staging directory inside the repository (gitignored `target/`):
// the bundler records entry paths in comments, so a random temp directory
// would make the output differ per run and embed the user's temp path.
const work = join(repo, 'target', 'ts-bundle-work');
rmSync(work, { recursive: true, force: true });
mkdirSync(work, { recursive: true });
try {
  const entryPath = join(work, 'entry.mjs');
  writeFileSync(entryPath, entry);
  const built = await Bun.build({ entrypoints: [entryPath], target: 'node', format: 'esm', minify: false });
  if (!built.success) {
    for (const l of built.logs) console.error(l);
    process.exit(1);
  }
  const js = await built.outputs[0]!.text();
  const tsEntryPath = join(work, 'ts-entry.mjs');
  writeFileSync(tsEntryPath, tsEntry);
  const builtTs = await Bun.build({ entrypoints: [tsEntryPath], target: 'node', format: 'esm', minify: false, splitting: false });
  if (!builtTs.success || builtTs.outputs.length !== 1) {
    for (const l of builtTs.logs) console.error(l);
    console.error(`ts-parser bundle: ${builtTs.outputs.length} outputs`);
    process.exit(1);
  }
  const tsJs = await builtTs.outputs[0]!.text();
  // Replace this script's outputs; LICENSES/ belongs to bundle-licenses.ts.
  for (const f of existsSync(out) ? readdirSync(out) : []) {
    if (f !== 'LICENSES') rmSync(join(out, f), { recursive: true, force: true });
  }
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, 'ts_batch.mjs'), js);
  writeFileSync(join(out, 'ts_parse.mjs'), tsJs);
  const files: Record<string, string> = { 'ts_batch.mjs': sha(js), 'ts_parse.mjs': sha(tsJs) };
  const wasmDir = `${base}/wasm`;
  const pinned = JSON.parse(readFileSync(`${wasmDir}/checksums.json`, 'utf8')) as Record<string, string>;
  const copy = (rel: string) => {
    const bytes = readFileSync(`${wasmDir}/${rel}`);
    if (pinned[rel] !== sha(bytes)) throw new Error(`${rel} does not match WrongStack's checksums.json`);
    mkdirSync(join(out, rel, '..'), { recursive: true });
    copyFileSync(`${wasmDir}/${rel}`, join(out, rel));
    files[rel] = sha(bytes);
  };
  copy('tree-sitter-runtime.wasm');
  for (const g of Object.values(GRAMMARS)) copy(`${g}/tree-sitter-${g}.wasm`);
  const webPkg = { version: packageVersion(webTreeSitter) };
  writeFileSync(
    join(out, 'provenance.json'),
    `${JSON.stringify(
      {
        generator: 'scripts/oracle/bundle-tree-sitter.ts',
        wrongstackCommit: oracle.commit,
        archiveSha256: oracle.archiveSha256,
        bun: Bun.version,
        webTreeSitter: webPkg.version,
        typescript: packageVersion(Bun.resolveSync('@typescript/typescript6', base)),
        grammars: GRAMMARS,
        sources: Object.fromEntries(
          ['ts-parser.ts', 'tree-sitter-parser.ts', 'tree-sitter/visitor.ts', 'tree-sitter/queries.ts', 'tree-sitter/query-extractors.ts', 'tree-sitter/util.ts'].map(
            (f) => [f, sha(readFileSync(`${base}/${f}`))],
          ),
        ),
        licences: 'TypeScript (@typescript/typescript6) Apache-2.0; web-tree-sitter MIT; tree-sitter-wasm MIT; grammars per upstream (MIT, tree-sitter-elixir Apache-2.0); WrongStack MIT. Texts: LICENSES/ (scripts/oracle/bundle-licenses.ts)',
        files,
      },
      null,
      2,
    )}\n`,
  );
  console.log(`tree-sitter bundle: ${js.length} chars, ${Object.keys(files).length} files`);
} finally {
  rmSync(work, { recursive: true, force: true });
}

/** Version from the nearest `package.json` above a resolved module file. */
function packageVersion(file: string): string {
  let dir = join(file, '..');
  for (;;) {
    const p = join(dir, 'package.json');
    if (existsSync(p)) return (JSON.parse(readFileSync(p, 'utf8')) as { version: string }).version;
    const up = join(dir, '..');
    if (up === dir) return 'unknown';
    dir = up;
  }
}
