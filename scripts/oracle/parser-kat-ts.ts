/**
 * Synthetic edge cases for the TypeScript/JavaScript parser (`ts-parser.ts`
 * through `parseFileContent(file, content, 'ts' | 'tsx' | 'js' | 'jsx')`).
 * Imported by `parser-kat.ts`. The first block reproduces the corpus of the
 * oxc feasibility spike (`mkedge.js`) verbatim; later blocks add cases the
 * native port treats as risky. Some cases are expected to be refused by the
 * native port (parse errors, script-file top-level `await`); the Rust KAT test
 * lists them explicitly.
 */
type Case = { lang: string; file: string; content: string; source: string };

export function addTsCases(cases: Case[]): void {
  const T = (name: string, file: string, content: string) => {
    const lower = file.toLowerCase();
    const lang = /\.tsx$/.test(lower) ? 'tsx' : /\.[mc]?ts$/.test(lower) ? 'ts' : /\.jsx$/.test(lower) ? 'jsx' : 'js';
    cases.push({ lang, file: `C:\\kat\\${file}`, content, source: `synthetic:ts-${name}` });
  };

  // ── Spike corpus (mkedge.js) ──────────────────────────────────────────────
  T('spike-a', 'a.ts', `
/** @license first */
/** real doc */
@Component({ selector: 'x' })
export class Widget<T extends Base> extends Mixin(Base) implements A.B, C<D>, E {
  @Input() name: string = '';
  @Output() static readonly change = new EventEmitter<string>();
  #secret = 1;
  'quoted'() {}
  0x10() {}
  1e3() {}
  .5() {}
  017() {}
  1_000() {}
  [Symbol.iterator]() { const inner = 1; return inner; }
  "constructor"() {}
  constructor(private readonly svc: Service, public x?: number) { super(); new.target; super.method(); }
  get value(): number { return this.#secret; }
  set value(v: number) { this.#secret = v; }
  accessor acc = 1;
  abstract?: void;
  m(): void;
  m(x: string): void;
  m(x?: string) { this?.x; obj?.y?.z; fn?.(); }
  static { const staticLocal = 1; var staticVar = 2; }
  arrow = async <U,>(u: U): Promise<U> => { return u; };
  fexp = function named() { return 1; };
  klass = class Inner { im() {} };
}
export default function () { anonCall(); }
export namespace Outer.Middle.Inner { export const deep = 1; }
declare global { interface Window { myGlobal: string } }
declare module 'virtual:mod' { export function vf(): void; const vc: number; export default vc; }
module LegacyNs { export var lv = 1; }
export declare namespace Decl { function df(): void; }
export as namespace UMDGlobal;
export enum Color { Red = 'r', Green = 'g' }
export const enum Flags { A = 1 << 0 }
declare enum Ambient { X }
export type Mapped<T> = { readonly [K in keyof T]?: T[K] extends infer U ? U : never };
type Tpl = \`prefix-\${string}\`;
type Q = typeof import('./mod').thing;
type QN = NS.Sub.Type<Inner.X>;
interface Acc { get a(): string; set a(v: string); method(): void; prop: number; (call: number): string; new (x: number): Acc; [k: string]: unknown; }
export function over(a: string): string;
export function over(a: number): number;
export function over(a: any) { return a; }
import fs = require('fs');
import type { T1, T2 as T3 } from './types';
import def, * as ns from './ns';
import { default as dflt, type T4, 'string name' as sn } from './x' with { type: 'json' };
import {} from './empty';
import './side';
export { a as b, c } from './re';
export * from './star';
export * as starNs from './starns';
export { default } from './dflt';
export {};
export = Widget;
for (const loopVar of list) { let blockLet = loopVar; }
for (let i = 0, j = 1; i < 1; i++) {}
for (var k in obj) {}
try { risky(); } catch (err) { const inCatch = 1; }
{ const inBlock = 1; }
if (x) { var hoisted = 2; }
const { d1, d2: renamed, d3 = 5, ...drest } = source(), [a1, , [a2], ...arest] = arr;
export let e1 = 1, e2 = () => {}, e3 = 'export';
using res = getRes();
label: for (;;) { break label; }
const o = { m() { const local = 1; }, get g() { return 1; }, set g(v) {}, async *gen() {}, ['comp']: 1, prop: () => {}, 'str'() {}, 42() {} };
const tagged = tag\`x\${y}\`;
const asserted = <Foo>bar;
const sat = cfg satisfies Config as unknown as Other;
const nn = maybe!.value!;
const call = (paren)(1);
const el = arr[0].x;
const meta = import.meta.url;
const dyn = import('./dyn');
const req = require('./req');
function outer() { function innerFn() {} class InnerCls { im() {} } const notIndexed = 1; return { om() {} }; }
const \u0061bc = 1;
const ünïcødé = '😀';
/* block */ /** after block */ export const afterBlock = 1;
export const sameLine = 1; /** trailing doc */
export const nextDecl = 2;
/** multi
 * line
 */
export type Multi = string;
/***/ const stars = 1;
/**/ const emptyDoc = 1;
`);
  T('spike-b', 'b.tsx', `
import React from 'react';
export const App = <T,>(p: Props<T>) => (
  <Layout.Main title={t('x')} {...rest}>
    <this.Sub />
    <ns:tag />
    <Foo.Bar.Baz>{items.map((i) => <Item key={i.id} />)}</Foo.Bar.Baz>
    &nbsp;text
  </Layout.Main>
);
export default class extends React.Component {}
`);
  T('spike-c', 'c.js', `#!/usr/bin/env node
'use strict';
const path = require('path');
module.exports = { a: 1 };
exports.b = function b() {};
function Legacy() {}
Legacy.prototype.m = function () { return <div className="x">{this.y}</div>; };
class K { static s = 1; #p() {} }
`);
  T('spike-d-cr', 'd-cr.ts', '/** cr doc */\rexport function crOnly() {\r  call1();\r}\rconst x = 1;\r\n/** mixed */\r\nexport const y = foo.bar;\n');
  T('spike-e-lsps', 'e-lsps.ts', 'const a = " "; export function afterLS() { call2(); } /** ps doc */ export const z = b.c;\n/*   in block */ export const w = d.e;\nconst \u{1D49C}x = 1;\n');
  T('spike-f-bom', 'f-bom.ts', '\uFEFF/** bom doc */\nexport const bom = 1;\n');
  T('spike-g-long', 'g-long.ts', 'export const big: Record<string, "' + '😀'.repeat(300) + '"> = {};\n' + 'export type T' + ' = A' + ' | A'.repeat(10) + ';\n' + 'export function longSig(' + Array.from({ length: 80 }, (_, i) => `p${i}: string`).join(',\n    ') + ') {}\n');
  T('spike-h-script', 'h-script.ts', 'await (async () => {})();\nawait\n(x);\nconst v = 1;\n');
  T('spike-i-nul', 'i-nul.js', 'const a = 1;\0\nfunction f() {}\n');
  T('spike-j-conflict', 'j-conflict.ts', 'export const a = 1;\n<<<<<<< HEAD\nconst b = 1;\n=======\nconst b = 2;\n>>>>>>> branch\n');
  T('spike-k-broken', 'k-broken.ts', 'export function ok() {}\nexport class Broken {\n  m( {\n}\nexport const after = foo();\n');
  T('spike-l-jsx-in-ts', 'l-jsx-in-ts.ts', 'const x = <T>(y: T) => y;\nconst z = <div/>;\n');
  T('spike-m-decl', 'm-decl.d.ts', 'declare function g(): void;\nexport declare class DC { private constructor(); m(): void; }\ndeclare const dc: number;\nexport {};\n');
  T('spike-n', 'n.cjs', 'if (!x) return;\nconst y = require("y");\n');

  // ── Additional risky shapes ───────────────────────────────────────────────
  T('empty', 'empty.ts', '');
  T('numeric-names', 'nums.ts', 'export class N {\n  123456789012345678901234567890() {}\n  1.0() {}\n  1e21() {}\n  1e-7() {}\n  0.000001() {}\n  0b101() {}\n  0o17() {}\n  0XFF() {}\n  .5e1() {}\n  1n() {}\n  9007199254740993() {}\n  0() {}\n  1_0.5_0() {}\n  get 7() { return 7; }\n  static 8 = 1;\n}\nconst lit = { 1.50() {}, 0x1F() {}, 2e0() {} };\n');
  T('escaped-names', 'esc.ts', 'const \\u0061bc2 = 1;\nclass \\u{62}cls { \\u0063m() {} "\\u0064s"() {} \'it\\\'s\'() {} }\nexport function f\\u0031() {}\n');
  T('decorators', 'deco.ts', '@a\n@b.c()\nexport class D1 {}\nexport @e class D2 {}\n@f export default class D3 {\n  @g() @h.i m() {}\n  @j static p = 1;\n  constructor(@Inject(TOKEN) private t: T) {}\n  @k accessor q = 2;\n  @l get g2() { return 1; }\n}\nabstract class Abs { abstract m(): void; abstract p: number; protected abstract get g(): string; }\n');
  T('modifiers', 'mods.ts', 'export default async function namedDefault() { await x; }\nexport async function* gen() {}\ndeclare class DeclCls { m(): void; }\ndeclare function declFn(a: number): void;\ndeclare let declLet: string;\ndeclare var declVar: string, other: number;\nexport declare const ec: number;\nexport abstract class EA {}\nexport default interface DI { x: number }\n');
  T('heritage-lines', 'heritage.ts', 'class X\n  extends\n  Base\n  implements\n    I1,\n    ns.I2,\n    I3<T> {}\ninterface Y extends\n  A, B.C<D>,\n  E {}\nconst Z = class extends Q { };\nclass W extends (Base as any) {}\nclass V extends ns.Base<T> {}\nclass U extends mix(A, B) implements I {}\n');
  T('docs', 'docs.ts', '/** one */ /** two */\nexport const first = 1;\n// line\n/** after line */\nexport const afterLine = 2;\n/** doc */ // trailing line comment\nexport const mixed = 3;\nconst prev = 1; /** same line doc */\nexport const notCollected = 4;\nconst prev2 = 1; /** same line doc */ /** next */\n\n/** collected */\nexport const collected = 5;\n/**\n *\n * second line is first non-empty\n */\nexport const blankFirst = 6;\n/**    padded   */\nexport const padded = 7;\n/** ' + 'x'.repeat(250) + ' */\nexport const longDoc = 8;\n/** a\r\n * crlf b\r\n */\r\nexport const crlfDoc = 9;\n/*\n * not jsdoc\n */\nexport const notJsDoc = 10;\n/** doc */\n\n\n\nexport function farAway() {}\nexport class C {\n  /** member doc */\n  m() {}\n  /** prop doc */ p = 1;\n}\n');
  T('vars', 'vars.ts', 'export const a = 1, b = function () {}, c = class { m() {} }, d = () => 1;\nlet { x: { y: [z = 1] } } = obj;\nvar [, , third] = arr;\nexport let multi\n  = 1,\n  multi2 = 2;\nconst fnInit = function named(a) {\n  return a;\n};\nconst arrowBlock = async (a: number): Promise<void> => {\n  body();\n};\nconst clsInit = class Named extends Base {\n  im() {}\n};\nfor (const [k, v] of Object.entries(o)) {}\nfor await (const chunk of stream) {}\nswitch (x) { case 1: { const inCase = 1; } }\n');
  T('using', 'using.ts', 'export {};\nusing u1 = res();\nawait using u2 = res();\nconst top = await load();\nfor (using u3 of list) {}\n');
  T('refs', 'refs.ts', 'import type * as TNS from "./tns";\nimport D, { type X as Y, Z } from "./m";\nimport * as NS from "@scope/pkg/sub";\nexport type { T1 } from "./t";\nexport { default as Def, "str" as S } from "./d";\nexport * as "quoted ns" from "./q";\nfoo(); foo(); bar.baz(); a.b.c.d(); new Klass(); new ns.Klass(); new (getK())();\n(paren)(); (obj).prop; obj!.p; obj?.p; obj?.[k]; obj[k]; this.p; super.x;\nconst t: Map<string, Array<Foo.Bar>> = new Map();\nfunction g<T extends Base>(a: T): asserts a is Narrow {}\nlet q: typeof val.prop; let r: keyof typeof val;\nlet i: import("./mod").Type<Arg>;\nlet tl: `a${Lit}`;\nlet tup: [a: A, b?: B, ...rest: C[]];\nlet fnT: (a: P) => R; let ctorT: new (a: P) => R;\nlet cond: A extends B ? C : D;\nlet idx: O["k"];\nobj.a.b; obj.a; x?.y.z;\ntag`t${expr.p}`;\n');
  T('jsx', 'comp.tsx', 'import * as React from "react";\nexport function Comp({ a }: Props) {\n  return (\n    <Ctx.Provider value={a}>\n      <Ctx.Consumer>{(v) => <span>{v}</span>}</Ctx.Consumer>\n      <a.b.c />\n      <svg:rect />\n      <>{list.map(render)}</>\n      <Foo\n        x={1}\n      />\n    </Ctx.Provider>\n  );\n}\nexport default () => <div />;\n');
  T('jsx-js', 'view.jsx', 'import React from "react";\nexport default function View(props) {\n  return <Layout.Page {...props}><props.Child /></Layout.Page>;\n}\nconst Arrow = () => <></>;\n');
  T('mjs', 'mod.mjs', 'import { a } from "./a.mjs";\nexport const b = a + 1;\nexport default { b };\n');
  T('cjs-script', 'script.cjs', '"use strict";\nconst { join } = require("path");\nfunction main() { return join("a", "b"); }\nmodule.exports = { main };\nvar hoisted = 1;\nlet octalLike = 0;\n');
  T('mts-cts', 'x.mts', 'export const m: number = 1;\nexport function f(): void {}\n');
  T('cts', 'y.cts', 'import fs = require("fs");\nexport = { fs };\n');
  T('namespaces', 'ns.ts', 'namespace A.B { export class C {} }\nexport module M { export namespace N { export const v = 1; } }\ndeclare module "ambient";\ndeclare module "body" { export const k: number; }\nnamespace Empty {}\nexport declare global { var g: number; }\n');
  T('enum-type', 'types.ts', 'export enum E { A, B = A + 1, C = "c".length }\nexport type Fn = (a: number) => void;\nexport type Obj = {\n  get x(): number;\n  set x(v: number);\n  m(): void;\n  p: string;\n};\ninterface I { get y(): string; m?(): void; readonly [k: string]: unknown }\nexport type Long = ' + Array.from({ length: 200 }, (_, i) => `'v${i}'`).join(' | ') + ';\n');
  T('surrogate-cut', 'cut.ts', 'export const s = "' + 'a'.repeat(484) + '😀😀😀";\n/** ' + 'b'.repeat(198) + '😀 tail */\nexport const d = 1;\n');
  T('line-breaks', 'lb.ts', 'export function a() {\u2028  callA();\u2029}\rexport function b() {\r\n  callB();\n}\u2028/** ls doc */\u2028export const c = 1;\n');
  T('object-members', 'obj.js', 'export const api = {\n  get: function () {},\n  async post() {},\n  get value() { return 1; },\n  set value(v) {},\n  [computed]() {},\n  "quoted-key"() {},\n  #notValid: 1,\n};\n');
  T('object-members-ok', 'obj2.js', 'export const api = {\n  get: function () {},\n  async post() { inner(); },\n  get value() { return 1; },\n  set value(v) {},\n  [computed]() {},\n  "quoted-key"() {},\n  nested: { deep() {} },\n};\n');
  T('class-fields', 'fields.ts', 'export class F {\n  declare d: string;\n  readonly r = 1;\n  static s?: number;\n  private p!: string;\n  override o = 2;\n  #h = () => { body(); };\n  [k] = 1;\n  "str" = 2;\n  42 = 3;\n  f = function () {};\n  c = class {};\n  static { init(); }\n  constructor() { super(); }\n}\n');
  T('script-await-id', 'awaitid.js', 'var await = 1;\nawait(2);\n');
  T('script-await-nested', 'awaitfn.js', 'async function f() { await g(); }\nconst h = async () => { await (1); };\n');
  T('regex-and-html-comment', 'misc.js', 'const re = /ab+c/gi;\nconst re2 = /[/]/;\nconst div = a / b / c;\n// end\n');
  T('generics-arrow-ts', 'gen.ts', 'const id = <T,>(x: T) => x;\nconst id2 = <T extends unknown>(x: T): T => x;\nfunction f<const T>(x: T) {}\nclass G<in out T> {}\n');
  T('crlf-file', 'crlf.ts', 'import { a } from "./a";\r\n\r\n/** Doc */\r\nexport class K extends a.Base {\r\n  method(): void {\r\n    a.call();\r\n  }\r\n}\r\n');
  T('nested-functions', 'nested.ts', 'export function outer() {\n  function inner() {\n    class Local { m() {} }\n  }\n  return inner;\n}\nexport const arrow = () => {\n  function insideArrow() {}\n  const notTop = 1;\n};\n');
  // Script files (no import/export) with top-level await: TypeScript parses
  // `await <identifier|keyword|literal on the same line>` as an await
  // expression even there (same tree as oxc); anything else is refused.
  T('script-await-alike', 'tla.mjs', "await main();\nawait 'x';\nawait new Foo();\nawait import('./x');\nawait 5;\nawait this.y;\nconst r = await fetch(u);\nawait /* c */ go();\nawait\tasync function () {};\nfunction later() {}\n");
  T('script-await-template', 'tla2.mjs', 'const v = 1;\nawait `t${v}`;\n');
  T('script-await-unary', 'tla3.js', 'await -x;\nawait [a];\n');
}
