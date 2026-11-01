/**
 * WI-501/502: known answers for the native parser ports.
 *
 * Runs WrongStack's own `json-parser.ts` / `yaml-parser.ts` and, for Rust and
 * every language routed to the regex extractor (`generic-parser.ts` with
 * `infra-parser.ts`: scala, sql, md, toml, html, css, vue, svelte, dart, lua,
 * r, proto, graphql, zig, haskell, other), `parseFileContent(file, content,
 * lang)` (the parser plus the `withRelations` import fallback from
 * `import-extractor.ts`), and for TS/JS (`ts-parser.ts`, cases in
 * `parser-kat-ts.ts`), from a clean oracle prepared by
 * `prepare-clean-wrongstack.ts`, over a corpus of edge cases plus every file
 * of those languages under `tests/fixtures/projects`, and writes
 * `contracts/fixtures/parser-kat.json`. The Rust engine crate is tested
 * against that file; never hand-edit it.
 *
 * Usage: bun scripts/oracle/parser-kat.ts --wrongstack <clean oracle dir>
 *        [--corpus <dir> --out <file>]   (real-world corpus, not committed)
 *
 * A corpus written to a `.jsonl` file holds one case per line and no file
 * contents (the test re-reads each file by its absolute path), so corpora of
 * hundreds of megabytes stay streamable.
 *
 * JSON/YAML cases record `refs` as a count (YAML infra refs are not ported);
 * every other case records the refs themselves.
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
const dispatch = await import(`${base}/parser-dispatch.ts`);
const languages = await import(`${base}/languages.ts`);

/** Languages `parseFileContent` hands to `generic-parser.ts` (its `default` branch). */
const GENERIC = new Set([
  'scala', 'sql', 'md', 'toml', 'html', 'css', 'vue', 'svelte', 'dart', 'lua', 'r', 'proto',
  'graphql', 'zig', 'haskell', 'other',
]);
/** Languages `parseFileContent` hands to `ts-parser.ts`. */
const TS_LANGS = new Set(['ts', 'tsx', 'js', 'jsx']);
type Lang = string;
type Case = { lang: Lang; file: string; content: string; source: string };
const cases: Case[] = [];
const J = (name: string, content: string, file = `C:\\kat\\${name}.json`) =>
  cases.push({ lang: 'json', file, content, source: `synthetic:${name}` });
const Y = (name: string, content: string) =>
  cases.push({ lang: 'yaml', file: `C:\\kat\\${name}.yaml`, content, source: `synthetic:${name}` });
const R = (name: string, content: string) =>
  cases.push({ lang: 'rs', file: `C:\\kat\\${name}.rs`, content, source: `synthetic:rs-${name}` });

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

// ── Rust ────────────────────────────────────────────────────────────────────
R('empty', '');
R('items', 'pub struct Point { x: i32 }\nenum Color { Red }\ntrait Draw { fn draw(&self); }\ntype Alias = u32;\nconst MAX: usize = 3;\nstatic NAME: &str = "x";\nmod inner { }\nmod decl;\npub fn run() {}\n');
R('generics', 'fn parse<T: FromStr>(s: &str) -> Option<T> { None }\nfn  spaced  <T>() {}\nfn tuple ( ) {}\nfn no_paren\n() {}\ntype Map<K, V> = HashMap<K, V>;\ntype Bad<K> where K: X = u8;\ntype Out<A> ;\npub type R<T, E = Error> = Result<T, E>;\ntype G<T> /* c */ = T;\ntype Gt<T: A<B>> = T;\nstruct Wrap<T>(T);\nenum E<\'a> { A(&\'a str) }\n');
R('impl-blocks', 'impl Foo {}\nimpl<T> Foo<T> {}\nimpl<T: Into<String>> Display for Bar<T> {}\nimpl<T: A<B<C>>> deep::path::Thing for X {}\nimpl<T: A<B<C<D>>>> TooDeep for X {}\n  unsafe impl Send for Raw {}\nunsafe  impl<T> Sync for Raw<T> {}\nfn f() -> impl Iterator<Item = u8> { todo!() }\nimpl a::b::<X> {}\nimpl a:::b {}\nimpl ::std::fmt::Debug for Y {}\nimpl\n<T>\nMultiLine<T> {}\nimpl<T>  {}\nimpl dyn Trait {}\nimpl<\'a> Lt<\'a> {}\nimpl<T unclosed\nstruct AfterUnclosed;\nunsafe implx Bad {}\nimplement Nope {}\n');
R('traits', 'pub trait Shape: Debug + Clone {\n    const SIDES: u32;\n    type Output;\n    fn area(&self) -> f64;\n    fn default_name() -> String { String::new() }\n}\npub unsafe trait Danger {}\nauto trait Marker {}\n');
R('macros', 'macro_rules! square {\n    ($x:expr) => { $x * $x };\n}\nmacro_rules! make_fn {\n    ($n:ident) => { fn $n() {} };\n}\nmake_fn!(generated);\nlazy_static! {\n    static ref CACHE: Mutex<u8> = Mutex::new(0);\n}\nthread_local! { static LOCAL: Cell<u8> = Cell::new(0); }\n#[macro_export]\nmacro_rules! exported { () => {} }\n');
R('nested-mods', 'pub mod outer {\n    pub mod inner {\n        pub fn deep() {}\n        mod deeper;\n    }\n    pub(crate) mod sibling;\n}\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn it_works() {}\n}\n');
R('docs', '//! Crate docs\n//! fn not_a_symbol()\n\n/// First doc line\n/// second line\n#[derive(Debug)]\n#[serde(rename_all = "camelCase")]\npub struct Documented;\n\n///\n///   \n/// after blanks\nfn blank_first() {}\n\n/// Detached\n\nfn detached() {}\n//// four slashes\nfn four() {}\n/** block doc */\nfn block_doc() {}\n/// doc with trailing   \n   #[inline]   \n   fn indented() {}\n' + `/// ${'d'.repeat(250)}\nfn long_doc() {}\n`);
R('comments-strings', '// fn in_comment()\n/* struct InBlock /* nested */ still comment enum E2 */\nfn real() { let s = "fn in_string() { struct S }"; }\nlet esc = "quote \\" fn after_escape()";\nlet ch = \'"\'; fn after_char_quote() {}\nlet b = b"struct Bytes"; let c = \'\\n\'; let u = \'\\u{1F600}\';\nfn after_chars() {}\n/* unterminated block fn never()\n');
R('raw-strings', 'let a = r"fn raw_a()";\nlet b = r#"fn raw_b() "quoted" "#;\nlet c = r##"struct "# RawC "##;\nlet d = br"fn byte_raw()"; let e = cr"C:\\"; fn after_c_raw() {}\nlet f = r#ident; fn after_raw_ident() {}\nlet g = xr"not raw"; fn after_xr() {}\nlet h = abr"x"; fn after_abr() {}\nlet unterminated = r#"fn never_closed()\n');
R('lifetimes', "fn longest<'a>(x: &'a str, y: &'a str) -> &'a str { x }\nfn st(s: &'static str) {}\nstruct Holder<'a> { r: &'a u8 }\nlet c = 'x'; let e = '😀'; fn after_emoji_char() {}\nstatic GLOBAL: &'static str = \"g\";\nconst C: &'static [u8] = b\"\";\nfn label() { 'outer: loop { break 'outer; } }\nlet q = '\\''; fn after_escaped_quote() {}\n");
R('crlf', '/// Doc\r\npub fn crlf_fn() {}\r\nstruct CrlfStruct;\r\nuse std::io;\r\nmod m;\r\n#[cfg(x)]\r\n/// inner\r\nconst K: u8 = 1;\r\n');
R('unicode', '/// Ünïcödé doc 😀\nfn ünïcode() {}\nfn ascii_after_ünï() {}\nstruct Été;\nlet s = "😀😀"; fn after_emoji() {}\n// 日本語 fn hidden()\nfn 日本() {}\nstruct Tail😀Name;\n');
R('visibility', 'pub(crate) fn a() {}\npub(super) struct B;\npub(in crate::x) enum C {}\npub(crate) use crate::x::Y;\npub (crate) use spaced::Z;\npub(crate)use tight::W;\npub use plain::P;\npub(crate) mod vis_mod;\npub (in a::b) mod in_path;\npubuse nope::N;\npub(unclosed use x::Q;\n');
R('fn-qualifiers', 'async fn a() {}\nconst fn b() -> u8 { 0 }\nunsafe fn c() {}\npub async unsafe fn d() {}\nextern "C" fn e() {}\nconst unsafe fn f() {}\nconst async_x: u8 = 0;\nconst fnord: u8 = 1;\nconst extern_ok : u8 = 2;\nconst _: () = ();\nstatic mut COUNTER: u32 = 0;\nstatic mut : u8 = 0;\nstatic mutable: u8 = 0;\nstatic  mut\n  SPLIT: u8 = 0;\nx.const y: u8;\n');
R('use-trees', 'use a::{b, c::{d, e}};\nuse x as y;\nuse std::collections::HashMap;\nuse std::io::{self, Write};\nuse super::*;\nuse crate::parse::*;\nuse self::m::N;\nuse ::abs::Path;\nuse ::{a, b};\nuse {x, y};\nuse  spaced :: path;\nuse a::b\n    ::c;\nuse long::path::Thing as _;\nuse a::b::_;\n    use indented::Thing;\n\tuse tabbed::T;\nuse dup::A;\nuse dup::A;\nuse a::b::C as D;\nuse foo::bar::{\n    Baz,\n};\nuse ascii::Ünï;\nuse nosemicolon\nfn f() { use local::Inner; }\nlet x = 1; use not_line_start::X;\nuse a:::b;\nuse x asy;\nuse x as\ty;\nuse :::{q};\n');
R('extern-crate', 'extern crate serde;\nextern crate alloc as a;\n#[macro_use]\nextern crate log;\nuse serde::Deserialize;\n');
R('mod-decls', 'mod foo;\npub mod bar;\nmod baz { }\nmod  spaced ;\nmod r#type;\nmod a\n;\npub(crate) mod c;\npub(in crate::x) mod d;\nmod e {}\nmod nested { mod f; }\n');
R('char-edge', "let a = '\\x41'; let b = '\\u{10FFFF}'; let c = '\\\\'; fn after_backslash() {}\nlet long = '\\this_is_not_a_char_literal_because_too_long'; fn after_long() {}\nlet lt: &'a str; fn after_lt() {}\n");
R('dedup', 'fn same() {} fn same() {}\nstruct same;\nmod same;\nfn same() {}\n');
R('sig-truncate', `fn wide() { ${'x'.repeat(600)} }\n`);
R('trailing-backslash', 'let s = "abc\\');
R('cr-only', 'fn a() {}\rfn b() {}\ruse cr::Only;\rmod cr_mod;\r');
R('line-seps', 'fn a() {}\u2028use ls::One;\u2029mod ps_mod;\n');
R('many-imports', `${Array.from({ length: 450 }, (_, i) => `use m${i}::Item;`).join('\n')}\nmod last;\n`);

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
      else if (corpusLang(p) && st.size < 2_000_000) files.push(p);
    }
  };
  visit(resolve(corpus));
  cases.length = 0;
  for (const p of files.sort()) {
    cases.push({ lang: corpusLang(p)!, file: p, content: readFileSync(p, 'utf8'), source: `corpus:${p}` });
  }
}

/** JSON/YAML/Rust as before, plus every generic-routed language and TS/JS (WrongStack's `detectLang`). */
function corpusLang(p: string): Lang | null {
  if (/\.json$/i.test(p)) return 'json';
  if (/\.ya?ml$/i.test(p)) return 'yaml';
  if (/\.rs$/i.test(p)) return 'rs';
  const lang = languages.detectLang(p);
  return lang && (GENERIC.has(lang) || TS_LANGS.has(lang)) ? lang : null;
}

// ── Real fixture files ──────────────────────────────────────────────────────
function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}
const fixtureFiles = corpus ? [] : walk(join(repo, 'tests/fixtures/projects')).sort();
for (const p of fixtureFiles) {
  const lower = p.toLowerCase();
  const lang: Lang | null = lower.endsWith('.json')
    ? 'json'
    : lower.endsWith('.yaml') || lower.endsWith('.yml')
      ? 'yaml'
      : lower.endsWith('.rs')
        ? 'rs'
        : null;
  if (!lang) continue;
  // Exactly what the indexer hands the parser: utf8-decoded file text.
  cases.push({ lang, file: p, content: readFileSync(p, 'utf8'), source: `fixture:${relative(repo, p).split('\\').join('/')}` });
}

// ── Generic-routed languages (appended so earlier cases stay byte-identical) ──
if (!corpus) {
  const { addGenericCases } = await import('./parser-kat-generic.ts');
  addGenericCases(cases);
  for (const p of fixtureFiles) {
    const lang = languages.detectLang(p);
    if (!lang || !GENERIC.has(lang)) continue;
    cases.push({ lang, file: p, content: readFileSync(p, 'utf8'), source: `fixture:${relative(repo, p).split('\\').join('/')}` });
  }
}

// ── TypeScript/JavaScript (appended so earlier cases stay byte-identical) ──
if (!corpus) {
  const { addTsCases } = await import('./parser-kat-ts.ts');
  addTsCases(cases);
  for (const p of fixtureFiles) {
    const lang = languages.detectLang(p);
    if (!lang || !TS_LANGS.has(lang)) continue;
    cases.push({ lang, file: p, content: readFileSync(p, 'utf8'), source: `fixture:${relative(repo, p).split('\\').join('/')}` });
  }
}

/**
 * TS/JS signatures and doc comments are cut with `slice`, which can split a
 * surrogate pair; SQLite stores such a string with U+FFFD, and JSON parsers
 * reject lone surrogates, so the known answers record the well-formed form.
 */
const wf = (v: unknown) => (typeof v === 'string' ? v.toWellFormed() : v);

const outFile = arg('--out') ?? join(repo, 'contracts/fixtures/parser-kat.json');
const streaming = corpus !== undefined && outFile.endsWith('.jsonl');
const lines: string[] = [];
const out = [];
for (const c of cases) {
  const r = c.lang === 'json'
    ? json.parseSymbols({ file: c.file, content: c.content, lang: 'json' })
    : c.lang === 'yaml'
      ? yaml.parseSymbols({ file: c.file, content: c.content, lang: 'yaml' })
      : await dispatch.parseFileContent(c.file, c.content, c.lang);
  const entry = {
    source: c.source,
    lang: c.lang,
    // Fixture files are referenced by path relative to the repo, so the KAT
    // does not embed a machine-specific absolute path.
    file: c.source.startsWith('fixture:') ? `<repo>/${c.source.slice(8)}` : c.file,
    ...(streaming ? {} : { content: c.content }),
    symbols: r.symbols.map((s: Record<string, unknown>) =>
      TS_LANGS.has(c.lang)
        ? {
            kind: s.kind, name: wf(s.name), line: s.line, col: s.col, signature: wf(s.signature),
            docComment: wf(s.docComment), scope: wf(s.scope), text: wf(s.text),
            isExported: s.isExported, endLine: s.endLine,
          }
        : {
            kind: s.kind, name: s.name, line: s.line, col: s.col, signature: s.signature,
            docComment: s.docComment, scope: s.scope, text: s.text,
          }),
    refs: c.lang !== 'json' && c.lang !== 'yaml'
      ? (r.refs ?? []).map((x: Record<string, unknown>) => ({
          toName: wf(x.toName), callType: x.callType, line: x.line, lang: x.lang, module: wf(x.module),
        }))
      : (r.refs ?? []).length,
  };
  if (streaming) lines.push(JSON.stringify(entry));
  else out.push(entry);
}
if (streaming) {
  writeFileSync(outFile, `${lines.join('\n')}\n`);
  console.log(`parser-kat corpus: ${lines.length} cases -> ${outFile}`);
  process.exit(0);
}

const doc = {
  generator: 'scripts/oracle/parser-kat.ts',
  provenance: { wrongstackCommit: oracle.commit, archiveSha256: oracle.archiveSha256, bun: Bun.version },
  parsers: {
    'json-parser.ts': createHash('sha256').update(readFileSync(`${base}/json-parser.ts`)).digest('hex'),
    'yaml-parser.ts': createHash('sha256').update(readFileSync(`${base}/yaml-parser.ts`)).digest('hex'),
    'rs-parser.ts': createHash('sha256').update(readFileSync(`${base}/rs-parser.ts`)).digest('hex'),
    'import-extractor.ts': createHash('sha256').update(readFileSync(`${base}/import-extractor.ts`)).digest('hex'),
    'parser-dispatch.ts': createHash('sha256').update(readFileSync(`${base}/parser-dispatch.ts`)).digest('hex'),
    'generic-parser.ts': createHash('sha256').update(readFileSync(`${base}/generic-parser.ts`)).digest('hex'),
    'infra-parser.ts': createHash('sha256').update(readFileSync(`${base}/infra-parser.ts`)).digest('hex'),
    'ts-parser.ts': createHash('sha256').update(readFileSync(`${base}/ts-parser.ts`)).digest('hex'),
  },
  cases: out,
};
writeFileSync(outFile, `${JSON.stringify(doc, null, 2)}\n`);
const refCount = out.reduce((n, c) => n + (Array.isArray(c.refs) ? c.refs.length : 0), 0);
const byLang = new Map<string, number>();
for (const c of out) byLang.set(c.lang, (byLang.get(c.lang) ?? 0) + 1);
console.log(
  `parser-kat: ${out.length} cases (${[...byLang].map(([l, n]) => `${l} ${n}`).join(', ')}), ${out.reduce((n, c) => n + c.symbols.length, 0)} symbols, ${refCount} refs`,
);
