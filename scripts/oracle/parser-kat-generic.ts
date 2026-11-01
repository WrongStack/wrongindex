/**
 * Synthetic edge cases for the languages WrongStack routes to its regex
 * extractor (`generic-parser.ts` + `infra-parser.ts` + `import-extractor.ts`).
 * Imported by `parser-kat.ts`; the file names matter (infra artifacts are
 * detected by name), so every case states one.
 */
type Case = { lang: string; file: string; content: string; source: string };

export function addGenericCases(cases: Case[]): void {
  const G = (lang: string, name: string, file: string, content: string) =>
    cases.push({ lang, file: `C:\\kat\\${file}`, content, source: `synthetic:${lang}-${name}` });

  // ── Markdown ──────────────────────────────────────────────────────────────
  G('md', 'empty', 'empty.md', '');
  G('md', 'headings', 'README.md', '# Title\n\nText\n## Section Two\n### Third ###\n#### 4\n##### 5\n###### 6\n####### seven\n#nospace\n  # indented\n#\ttab\n');
  G('md', 'front-matter', 'docs\\guide.md', '---\ntitle: "Guide"\ntags: [a, b]\n---\n\n# Guide\n\n## Install\n');
  G('md', 'fences', 'fences.md', '# Top\n```bash\n# install\nnpm i\n```\n## After\n~~~\n# tilde\n~~~\n````md\n```\n# still inside\n```\n````\n# Out\n   ```\n# three-space fence\n   ```\n    ```\n# four-space not a fence\n```unclosed\n# hidden\n');
  G('md', 'fence-mismatch', 'mismatch.md', '```\n# a\n~~~\n# b\n```\n# c\n~~~\n[x](in-tilde.md)\n~~~\n[y](after.md)\n');
  G('md', 'cross-line', 'cross.md', '#  \n# real\n#\nnext line\n# \t \n\n## end\n');
  G('md', 'quotes', 'quotes.md', '# "Quoted" Title\n## `code` and \'single\'\n### ### nested hashes\n# """\n');
  G('md', 'links', 'docs\\links.md', 'See [guide](./guide.md) and [api](../api/README.md#usage).\n![img](./logo.png) [ext](https://example.com) [mail](mailto:a@b.c) [anchor](#top)\n[dup](./guide.md) [sp]( spaced.md ) [q](a.md "title")\n[[WikiPage]] [[Other Page#Heading|Label]] [[#local]] [[ Spaced ]]\nInline `[code](x.md)` link\n[empty]() [a](HTTP://UP.example) [b](Https:x)\n');
  G('md', 'links-in-fence', 'lf.md', '[before](b.md)\n```\n[inside](i.md)\n```\n  ```\n[indented-fence](x.md)\n  ```\n[after](a.md)\n');
  G('md', 'crlf', 'crlf.md', '# CRLF Title\r\n\r\n## Second\r\n[l](./x.md)\r\n```\r\n# in\r\n```\r\n');
  G('md', 'unicode', 'unicode.md', '# Ünïcödé 😀 heading\n## 日本語\n\u2028# after LS\n# nbsp\u00A0title\u00A0\n[ü](./ü.md)\n');
  G('md', 'long', 'long.md', `# ${'a'.repeat(199)}\n# ${'b'.repeat(200)}\n# ${'c'.repeat(201)}\n## ${'d'.repeat(600)} tail\n`);
  G('md', 'mdx', 'page.mdx', 'import X from "./x"\n\n# MDX Page\n<X />\n[doc](./doc.mdx)\n');
  G('md', 'dockerfile-named', 'dockerfile.md', '# Docker notes\nFROM node:20 AS base\nARG VERSION\n[l](x.md)\n');
  G('md', 'many', 'many.md', Array.from({ length: 620 }, (_, i) => `## H${i}\n[l${i}](./p${i}.md)`).join('\n'));
  G('md', 'binary', 'bin.md', '# a\0b\n');

  // ── HTML ──────────────────────────────────────────────────────────────────
  G('html', 'page', 'index.html', '<!doctype html>\n<html>\n<head>\n  <link rel="stylesheet" href="css/site.css">\n  <link href=\'favicon.ico\' rel="icon">\n  <script src="js/app.js" defer></script>\n  <script type="module" data-src="x" src=\'js/mod.js\'></script>\n  <SCRIPT SRC="upper.js"></SCRIPT>\n</head>\n<body>\n  <div id="main-nav" class="x"></div>\n  <section ID=\'Hero\'>\n  <p data-id="data1" id = "spaced" >\n  <a id="class">kw</a> <b id="has space"> <i id="9lives"> <u valid="no">\n  <template id="tpl"></template>\n  <style>#a{}</style>\n</body>\n</html>\n');
  G('html', 'multiline-tag', 'multi.htm', '<script\n  type="text/javascript"\n  src="multi.js">\n</script>\n<link\n rel="x"\n href="https://cdn.example/x.css">\n<div\nid="ml">\n');
  G('html', 'unicode', 'u.html', '<div id="ünï">😀</div>\n<div id="ok-1"></div>\n');
  G('html', 'window', 'big.html', `<div id="first"></div>\n${'<p>filler text line</p>\n'.repeat(5600)}<div id="beyond-window"></div>\n<script src="late.js"></script>\n`);

  // ── CSS / SCSS / LESS ─────────────────────────────────────────────────────
  G('css', 'basic', 'site.css', '@import "reset.css";\n@import url("fonts.css");\n@import url(\'theme.css\') screen;\n.btn {\n  color: red;\n}\n#header{ }\ndiv {\n}\na:hover {\n}\n  .nested-rule {\n}\n.multi\n{\n}\n@media screen and (max-width: 600px) {\n  .m { }\n}\n@keyframes spin {\n}\n@supports (display: grid) {\n}\nfor {\n}\n.-bad {\n}\n_under {\n}\n');
  G('css', 'scss', 'styles\\main.scss', '@use \'sass:math\';\n@use "src/corners" as c;\n@forward "src/list" hide list-reset;\n  @import \'partials/base\';\n$primary: #333;\n.card {\n  &:hover { }\n  .title {\n  }\n  @include mixin;\n}\n@mixin theme($t: dark) {\n}\n');
  G('css', 'less', 'theme.less', '@import (reference) "base.less";\n@color: red;\n.mixin() {\n}\n.box {\n  .mixin();\n}\n');
  G('css', 'many-imports', 'many.css', Array.from({ length: 450 }, (_, i) => `@import "p${i}.css";`).join('\n'));
  G('css', 'binary-imports', 'bin.css', `@import "still.css";\n${'\x01'.repeat(40)}\n.x {\n}\n`);
  G('css', 'crlf', 'crlf.css', '@import "a.css";\r\n.a {\r\n}\r\n.b\r\n{\r\n}\r\n');

  // ── SQL ───────────────────────────────────────────────────────────────────
  G('sql', 'schema', 'schema.sql', '-- users table\nCREATE TABLE users (\n  id INTEGER PRIMARY KEY,\n  team_id INTEGER REFERENCES teams(id),\n  owner INT references "public"."owners" (id), boss INT REFERENCES users(id)\n);\ncreate table if not exists "public"."teams" (id int);\nCREATE OR REPLACE VIEW active_users AS SELECT * FROM users;\nCREATE INDEX idx_users_team ON users(team_id);\nCREATE UNIQUE INDEX uniq ON users(id);\nCREATE FUNCTION fn_x() RETURNS int AS $$ SELECT 1 $$;\nCREATE PROCEDURE p_y()\nCREATE TRIGGER trg AFTER INSERT ON users;\nCREATE TABLE\n  split_name (x int);\nCREATE TABLE select (x int);\nCREATE TABLE "" (x int);\n/* CREATE TABLE in_block (x) */\n  -- CREATE TABLE commented\n');
  G('sql', 'crlf', 'm.sql', 'CREATE TABLE a (id int);\r\nCREATE TABLE b (a_id int REFERENCES a(id));\r\n');

  // ── Protobuf ──────────────────────────────────────────────────────────────
  G('proto', 'service', 'api.proto', 'syntax = "proto3";\npackage demo.v1;\nimport "google/protobuf/timestamp.proto";\nimport public "common/types.proto";\nimport weak "legacy.proto";\n\n// message Commented {}\nmessage User {\n  string name = 1;\n  message Nested { int32 x = 1; }\n}\nenum Role { ADMIN = 0; }\nservice UserService {\n  rpc GetUser (GetUserRequest) returns (User);\n  rpc Stream(stream demo.v1.Chunk) returns (stream google.protobuf.Empty) {}\n  rpc Scalar (string) returns (bool);\n  rpc Split (A)\n    returns (B);\n}\n');
  G('proto', 'imports-only', 'imp.proto', 'syntax = "proto3";\nimport "a.proto";\nimport "b/c.proto" ;\nimport  "a.proto";\nmessage M {}\n');

  // ── GraphQL ───────────────────────────────────────────────────────────────
  G('graphql', 'schema', 'schema.graphql', '# type Commented\nscalar DateTime\ntype Query {\n  user(id: ID!): User\n  users(first: Int = 10, after: String): [User!]!\n  count: Int!\n}\ntype User implements Node & Entity {\n  id: ID!\n  posts: [Post]\n  created: DateTime\n}\nextend type User {\n  team: Team\n}\ninterface Node { id: ID! }\ninput NewUser { name: String! }\nenum Role { ADMIN USER }\nunion Result = User | Team\nTYPE Upper {\n}\ntype Comma implements A, B {\n}\n');
  G('graphql', 'operations', 'ops.gql', 'query GetUser($id: ID!) {\n  user(id: $id) { id }\n}\nmutation CreateUser {\n  create { id }\n}\nsubscription OnEvent { e }\nfragment F on User { id }\n');

  // ── TOML ──────────────────────────────────────────────────────────────────
  G('toml', 'cargo', 'Cargo.toml', '[package]\nname = "x"\n\n[[bin]]\npath = \'a\'\n[ dependencies . serde ] # c\n[]\n[a\nb]\n["quoted.key"]\r\n[target.\'cfg(unix)\'.dependencies]\n');
  G('toml', 'dotenv-named', '.env.toml', 'API_KEY=secret\nexport URL = "http://${HOST}:${PORT}"\n[section]\n');
  G('toml', 'dockerfile-named', 'dockerfile.toml', 'FROM alpine\n[x]\n');

  // ── Other: Dockerfile / Makefile / CMake / Terraform / misc ───────────────
  G('other', 'dockerfile', 'Dockerfile', '# syntax=docker/dockerfile:1\nFROM --platform=$BUILDPLATFORM rust:1.80 AS build\nARG TARGET=x\nENV RUST_LOG=info\nCOPY . .\nRUN cargo build\n\nfrom gcr.io/distroless/cc as runtime\nCOPY --from=build /app/target/release/app /app\ncopy --chown=1:1 --from=docker.io/lib:1 /a /b\nEXPOSE 8080\nexpose 53/udp\nFROM scratch\nENTRYPOINT ["/app"]\n');
  G('other', 'makefile', 'Makefile', '.PHONY: all test\nall: build test\nbuild: src/main.c lib.o\n\tcc -o app $^\ntest:\n\t./run-tests\nCC := gcc\nVAR = x\nx.o: x.c $(DEPS) ../up\n# comment: no\n  indented: target\nfunction foo\n');
  G('other', 'makefile-crlf', 'GNUmakefile', 'all: one two\r\nclean:\r\n\trm -f x\r\n');
  G('other', 'justfile', 'justfile', 'default: build\nbuild:\n    cargo build\n');
  G('other', 'cmake', 'CMakeLists.txt', 'cmake_minimum_required(VERSION 3.20)\nproject(demo)\nadd_executable(demo main.c)\nADD_LIBRARY( core STATIC core.c )\n# add_library(commented)\n');
  G('other', 'terraform', 'main.tf', 'terraform {\n  required_version = ">= 1.0"\n}\nresource "aws_s3_bucket" "logs" {\n  bucket = var.bucket_name\n}\ndata "aws_iam_policy" "ro" {}\nmodule "vpc" {\n  source = "terraform-aws-modules/vpc/aws"\n  cidr = module.network.cidr\n}\nvariable "bucket_name" {}\noutput "arn" { value = aws_s3_bucket.logs.arn }\n// comment var.x\n# comment module.y\nlocals { a = var.one + var.two }\n');
  G('other', 'tfvars', 'prod.tfvars', 'region = "eu"\nname = var.unused\n');
  G('other', 'perl', 'tool.pl', 'use strict;\nsub helper { }\nfunction js_like() {}\nexport class Klass {}\nconst VALUE = 1\nasync def py_like():\nfunc (r *R) Method() {}\nfn rust_like() {}\ntarget: x\n');
  G('other', 'procfile', 'Procfile', 'web: bundle exec puma\nworker: sidekiq\n');

  // ── Remaining generic languages ───────────────────────────────────────────
  G('scala', 'basic', 'Main.scala', 'package demo\nimport scala.collection.mutable\nimport demo.util._\nimport demo.model.*\nimport a.{B, C}\nobject Main {\n  def main(args: Array[String]): Unit = {}\n  private def helper = 1\n}\ncase class Point(x: Int)\ntrait Shape\nenum Color { case Red }\nclass if\n');
  G('vue', 'sfc', 'App.vue', '<template>\n  <div id="app"></div>\n</template>\n<script>\nimport Foo from "./Foo.vue"\nimport { a,\n  b } from \'../lib/ab\'\nimport \'side-effect\'\nexport default {\n  name: "App"\n}\nfunction helper() {}\nconst x = 1\nexport const y = 2\nexport default class Z {}\n</script>\n');
  G('svelte', 'component', 'Button.svelte', '<script>\n  import Icon from "./Icon.svelte";\n  export let label = "x";\n  let count = 0;\n  function click() { count += 1 }\n</script>\n<button on:click={click}>{label}</button>\n');
  G('dart', 'basic', 'main.dart', 'import \'package:flutter/material.dart\';\nimport "src/util.dart" as util;\nexport \'src/api.dart\';\npart \'main.g.dart\';\nclass App extends StatelessWidget {\n  Widget build(BuildContext context) {\n    if (x) {\n      return y;\n    }\n  }\n  Future<void> load() async {\n  }\n}\nenum Mode { a, b }\nmixin Logger {}\nextension on String {}\nvoid main() => runApp(App());\n');
  G('lua', 'basic', 'init.lua', 'local json = require("dkjson")\nlocal util = require \'lib.util\'\nrequire"bare"\nfunction M.helper(x) end\nfunction global_fn() end\nlocal function private_fn() end\nfunction obj:method() end\n');
  G('r', 'basic', 'analysis.R', 'library(dplyr)\nrequire("ggplot2")\nsource("helpers/util.R")\nsource( \'x.R\' )\nclean_data <- function(df) {}\nsummarise.stats = function(x) {}\n9bad <- function() {}\nx == function(y) {}\n');
  G('zig', 'basic', 'main.zig', 'const std = @import("std");\nconst util = @import( "util.zig" );\npub fn main() void {}\nfn helper() void {}\nvar counter: u32 = 0;\nconst Point = struct { x: i32 };\n');
  G('haskell', 'basic', 'Main.hs', 'module Main.App where\n\nimport Data.List (sort)\nimport qualified Data.Map as M\nimport Control.Monad\n\nmain :: IO ()\nmain = pure ()\n\nhelper :: Int -> Int\ndata Shape = Circle\ntype family F a\ntype Name = String\nclass Monad m => MonadLogger m where\n');
}
