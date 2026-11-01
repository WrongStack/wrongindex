/**
 * WI-501/502: known answers for the native JSON and YAML parser ports.
 *
 * Runs WrongStack's own `json-parser.ts` / `yaml-parser.ts` (from a clean
 * oracle prepared by `prepare-clean-wrongstack.ts`) over a corpus of edge
 * cases plus every JSON/YAML file under `tests/fixtures/projects`, and writes
 * `contracts/fixtures/parser-kat.json`. The Rust engine crate is tested
 * against that file; never hand-edit it.
 *
 * Usage: bun scripts/oracle/parser-kat.ts --wrongstack <clean oracle dir>
 */
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/oracle/parser-kat.ts --wrongstack <path>');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const marker = join(resolve(wrongstack), '.wrongindex-oracle.json');
if (!existsSync(marker)) {
  console.error('--wrongstack must be a clean oracle (missing .wrongindex-oracle.json)');
  process.exit(2);
}
const oracle = JSON.parse(readFileSync(marker, 'utf8')) as { commit: string; archiveSha256: string };
const base = `${resolve(wrongstack)}/packages/tools/src/codebase-index`;
const json = await import(`${base}/json-parser.ts`);
const yaml = await import(`${base}/yaml-parser.ts`);

type Case = { lang: 'json' | 'yaml'; file: string; content: string; source: string };
const cases: Case[] = [];
const J = (name: string, content: string, file = `C:\\kat\\${name}.json`) =>
  cases.push({ lang: 'json', file, content, source: `synthetic:${name}` });
const Y = (name: string, content: string) =>
  cases.push({ lang: 'yaml', file: `C:\\kat\\${name}.yaml`, content, source: `synthetic:${name}` });

// ── JSON ────────────────────────────────────────────────────────────────────
J('empty', '');
J('array-root', '[{"a": 1}, {"b": 2}]');
J('scalar-root', '"just a string"');
J('flat', '{\n  "name": "x",\n  "version": "1.0.0",\n  "nested": { "deep": { "k": 1 } }\n}\n');
J('crlf', '{\r\n  "a": 1,\r\n  "b": [1, 2],\r\n  "c": {"d": 2}\r\n}\r\n');
J('bom', '\uFEFF{\n  "bom": true\n}');
J('jsonc', '// header\n{\n  /* block } with brace */ "a": 1, // trailing\n  "b": "x//y", "c": "/*no*/",\n  "d": 3,\n}\n');
J('escapes', '{"q\\"uote": 1, "back\\\\slash": 2, "uni\\u00e9": 3, "colon:in:key": 4}');
J('unicode', '{\n  "émoji😀": 1,\n  "日本": {"x": 1},\n  "after": 2\n}');
J('surrogate-cols', '{"😀😀": 1, "z": 2}');
J('unterminated', '{"a": 1, "b');
J('package', '{\n  "name": "pkg",\n  "scripts": {\n    "build": "tsc",\n    "test:unit": "vitest ${X}",\n    "lint": "x"\n  },\n  "dependencies": {"a": "1"},\n  "devDependencies": {},\n  "peerDependencies": {"p": "*"},\n  "optionalDependencies": {"o": "1"},\n  "bin": {"pkg": "x"}\n}', 'C:\\kat\\nested\\package.json');
J('package-upper', '{"scripts": {"a": "b"}}', 'C:\\kat\\PACKAGE.JSON');
J('tsconfig', '{\n  // c\n  "compilerOptions": {\n    "strict": true,\n    "paths": {"@/*": ["src/*"]},\n    "target": "es2022",\n  },\n  "include": ["src"]\n}', 'C:\\kat\\tsconfig.json');
J('tsconfig-build', '{"compilerOptions": {"outDir": "dist"}}', 'C:\\kat\\tsconfig.build.json');
J('schema', '{\n  "$schema": "https://json-schema.org/draft/2020-12/schema",\n  "$id": "x",\n  "$defs": {\n    "A": {"type": "object"},\n    "B": {"$ref": "#/$defs/A"}\n  },\n  "definitions": {"Old": {}},\n  "properties": {"p": {"$ref": "#/$defs/A"}}\n}');
J('openapi', '{\n  "openapi": "3.1.0",\n  "components": {\n    "schemas": {\n      "User": {"type": "object"},\n      "Error": {}\n    }\n  },\n  "paths": {"/u": {"get": {}}}\n}');
J('deep-schema', '{"$schema": "x", "a": {"b": {"c": {"d": {"$defs": {"TooDeep": {}}}}}, "x": {"y": {"z": {"definitions": {"Deep3": {}}}}}}');
J('no-space-lines', '{"a":{"b":1},"c":[{"d":2}],"e":"}"}');
J('line-sep', '{\u2028"a": 1,\u2029"b": 2}');
J('many', `{\n${Array.from({ length: 1100 }, (_, i) => `  "k${i}": ${i}`).join(',\n')}\n}`);

// ── YAML ────────────────────────────────────────────────────────────────────
Y('empty', '');
Y('basic', 'server:\n  port: 8080\n  hosts:\n    - a.example\n    - b.example\nfeatures:\n  search: true\n');
Y('crlf', 'a: 1\r\nb:\r\n  c: "x"\r\n  - d: 2\r\n');
Y('scalars', 'i: 42\nn: -3.5e+2\nb: TRUE\nnul: Null\nu: undefined\nsq: \'single\'\ndq: "double"\nmixed: \'a\'b\'\ns: plain text\nempty:\nnum2: 1.\nnum3: 1e5\n');
Y('anchors', 'base: &base\n  x: 1\nderived:\n  <<: *base\n  y: [*base, &inline v]\nurl: http://h/?a=1&b=2\nmd: a*b **bold**\nlist:\n  - *base\n  -&nospace x\n');
Y('block-scalars', 'run: |\n  echo key: value\n  &notanchor x\n  - item: no\nscript: >-\n  folded: text\nkeep: |+\n\n  after blank\nindent: |2\n    two\nafter: 1\n- step: |\n    body: x\n  name: y\n');
Y('list-items', 'items:\n  - num: 42\n  -   spaced: "v"\n  - nested:\n      inner: 1\n- top: x\n-\tTab: 1\n');
Y('quoted-keys', '"quoted": 1\n\'single\': 2\n"un\'matched": 3\n"": 4\n');
Y('comments', '# comment: not a key\nkey: value # trailing\n#nokey: 1\nurl: "http://x:8080"\n');
Y('doc-markers', '---\na: 1\n...\n---\nb: 2\n');
Y('deep-indent', 'a:\n             too_deep: 1\n            ok_deep: 2\n');
Y('truncate', `long: ${'x'.repeat(80)}\nexact: ${'y'.repeat(60)}\nover: ${'z'.repeat(61)}\nemoji: ${'e'.repeat(58)}😀tail\n`);
Y('unicode', 'ключ: значение\n日本: 語\n😀: smile\nnbsp:\u00A0value\u00A0\n');
Y('line-seps', 'a: 1\u2028b: 2\u2029c: 3\rd: 4\n');
Y('multiline-key', 'key\n: value\n- item\n  : v\n');
Y('flow', '{a: 1, b: 2}\n[x, y]\nk: {c: 3}\n');
Y('weird', ':\n::\n  : x\n- :\n-\n- - nested: 1\nk:v\n');
Y('gha', 'name: CI\non: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - name: Test\n        run: |\n          cargo test\n          key: inside\n');

// ── Optional real-world corpus (not committed): --corpus <dir> --out <file> ──
const corpus = arg('--corpus');
if (corpus) {
  const skip = new Set(['node_modules', '.git', 'dist', 'target', '.wrongstack']);
  const files: string[] = [];
  const visit = (dir: string) => {
    for (const n of readdirSync(dir)) {
      if (skip.has(n)) continue;
      const p = join(dir, n);
      const st = statSync(p);
      if (st.isDirectory()) visit(p);
      else if (/\.(json|ya?ml)$/i.test(n) && st.size < 2_000_000) files.push(p);
    }
  };
  visit(resolve(corpus));
  cases.length = 0;
  for (const p of files.sort()) {
    const lang = /\.json$/i.test(p) ? 'json' : 'yaml';
    cases.push({ lang, file: p, content: readFileSync(p, 'utf8'), source: `corpus:${p}` });
  }
}

// ── Real fixture files ──────────────────────────────────────────────────────
function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}
for (const p of corpus ? [] : walk(join(repo, 'tests/fixtures/projects')).sort()) {
  const lower = p.toLowerCase();
  const lang = lower.endsWith('.json') ? 'json' : lower.endsWith('.yaml') || lower.endsWith('.yml') ? 'yaml' : null;
  if (!lang) continue;
  // Exactly what the indexer hands the parser: utf8-decoded file text.
  cases.push({ lang, file: p, content: readFileSync(p, 'utf8'), source: `fixture:${relative(repo, p).split('\\').join('/')}` });
}

const out = cases.map((c) => {
  const r = c.lang === 'json'
    ? json.parseSymbols({ file: c.file, content: c.content, lang: 'json' })
    : yaml.parseSymbols({ file: c.file, content: c.content, lang: 'yaml' });
  return {
    source: c.source,
    lang: c.lang,
    // Fixture files are referenced by path relative to the repo, so the KAT
    // does not embed a machine-specific absolute path.
    file: c.source.startsWith('fixture:') ? `<repo>/${c.source.slice(8)}` : c.file,
    content: c.content,
    symbols: r.symbols.map((s: Record<string, unknown>) => ({
      kind: s.kind, name: s.name, line: s.line, col: s.col, signature: s.signature,
      docComment: s.docComment, scope: s.scope, text: s.text,
    })),
    refs: (r.refs ?? []).length,
  };
});

const doc = {
  generator: 'scripts/oracle/parser-kat.ts',
  provenance: { wrongstackCommit: oracle.commit, archiveSha256: oracle.archiveSha256, bun: Bun.version },
  parsers: {
    'json-parser.ts': createHash('sha256').update(readFileSync(`${base}/json-parser.ts`)).digest('hex'),
    'yaml-parser.ts': createHash('sha256').update(readFileSync(`${base}/yaml-parser.ts`)).digest('hex'),
  },
  cases: out,
};
writeFileSync(arg('--out') ?? join(repo, 'contracts/fixtures/parser-kat.json'), `${JSON.stringify(doc, null, 2)}\n`);
console.log(`parser-kat: ${out.length} cases, ${out.reduce((n, c) => n + c.symbols.length, 0)} symbols`);
