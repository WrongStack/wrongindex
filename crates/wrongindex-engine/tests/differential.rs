//! Native writer vs WrongStack's own indexer on generated projects.
//!
//! Each scenario builds a project, indexes it twice into fresh directories -
//! once with WrongStack (`runIndexerWithStore` through the compatibility
//! worker) and once natively - and requires the two databases to be
//! logically identical (every table, FTS content by rowid, `sqlite_stat1`).
//!
//! Needs a prepared WrongStack engine and bun, so it is ignored by default:
//!
//!   WRONGINDEX_TEST_ENGINE=C:/wrongindex-oracle/wrongstack-<sha> \
//!     cargo test -p wrongindex-engine --test differential -- --ignored

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use wrongindex_engine::dbdump::{Volatile, diff, dump};
use wrongindex_engine::native::index_fresh;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

/// Index `root` into `index` with WrongStack itself.
fn wrongstack_index(root: &Path, index: &Path) -> Value {
    let engine = std::env::var("WRONGINDEX_TEST_ENGINE").expect("WRONGINDEX_TEST_ENGINE");
    let mut child = Command::new("bun")
        .arg(repo().join("packages/legacy-worker/worker.ts"))
        .arg("--engine")
        .arg(engine)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("bun");
    let mut stdin = child.stdin.take().unwrap();
    let req = json!({"id": 1, "op": "index", "args": {"projectRoot": root, "indexDir": index}});
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(stdin);
    let _ = child.wait();
    let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("worker said: {line}"));
    assert_eq!(v["ok"], true, "WrongStack index failed: {v}");
    v["result"].clone()
}

fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn compare(name: &str, root: &Path, base: &Path) -> Vec<String> {
    let a = base.join("idx-wrongstack");
    let b = base.join("idx-native");
    let ws = wrongstack_index(root, &a);
    let native = index_fresh(root, &b).unwrap_or_else(|e| panic!("{name}: native: {e}"));
    let v = Volatile::logical();
    let want = dump(&a.join("index.db"), &v).unwrap();
    let got = dump(&b.join("index.db"), &v).unwrap();
    let mut out: Vec<String> = diff(&want, &got, 8)
        .into_iter()
        .map(|l| format!("{name}: {l}"))
        .collect();
    if ws["symbolsIndexed"] != json!(native.symbols_indexed) {
        out.push(format!(
            "{name}: symbols {} (WrongStack) vs {} (native)",
            ws["symbolsIndexed"], native.symbols_indexed
        ));
    }
    eprintln!(
        "{name}: {} files, {} symbols, {} differences",
        want["files"].len(),
        native.symbols_indexed,
        out.len()
    );
    out
}

fn scalar(idx: &Path, sql: &str) -> String {
    let c = rusqlite::Connection::open_with_flags(
        idx.join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    c.query_row(sql, [], |r| {
        Ok(match r.get_ref(0)? {
            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
            rusqlite::types::ValueRef::Integer(i) => i.to_string(),
            other => format!("{other:?}"),
        })
    })
    .unwrap()
}

fn scenario_files(root: &Path) {
    write(
        root,
        "package.json",
        "{\n  \"name\": \"demo\",\n  \"scripts\": {\"build\": \"x\", \"test:unit\": \"y\"}\n}\n",
    );
    write(
        root,
        "config/app.yaml",
        "server:\n  port: 8080\n  hosts:\n    - a\n    - b\nbase: &b\n  x: 1\nother:\n  <<: *b\nrun: |\n  key: inside\n",
    );
    write(root, "config/empty.json", "");
    write(root, "config/array.json", "[1, 2, 3]");
    write(root, "config/crlf.yaml", "a: 1\r\nb:\r\n  - c: \"x\"\r\n");
    write(
        root,
        "config/bom.json",
        "\u{FEFF}{\"bom\": true, \"public\": 1}",
    );
    write(
        root,
        "data/ünïcödé.json",
        "{\"ключ\": 1, \"😀\": {\"x\": 1}}",
    );
    write(
        root,
        "schemas/user.schema.json",
        "{\"$schema\": \"x\", \"$defs\": {\"User\": {}, \"Team\": {}}}",
    );
    write(
        root,
        "tsconfig.json",
        "{\n  // c\n  \"compilerOptions\": {\"strict\": true, \"outDir\": \"dist\"},\n}\n",
    );
    write(root, "ignored/skip.json", "{\"never\": 1}");
    write(root, ".gitignore", "ignored/\n");
    for i in 0..57 {
        let body = if i % 9 == 0 {
            String::new()
        } else {
            format!("{{\"k{i}\": {i}, \"nested{i}\": {{\"a\": 1}}}}")
        };
        write(root, &format!("many/f{i:03}.json"), &body);
    }
    write(
        root,
        "many/deep/a/b/c.yaml",
        "deep: true\nlist:\n  - name: x\n",
    );
}

/// A Cargo workspace (virtual root manifest) with two crates, cross-crate
/// and `crate::`/`super::`/`self::` paths, `mod` declarations, a file outside
/// every crate, an integration test, import-only files (whose module owners
/// put edges into the rank graph) and JSON/YAML/TOML data files.
fn scenario_cargo_workspace(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/omega\"]\nresolver = \"2\"\n\n[workspace.package]\nname = \"not-a-crate\"\n",
    );
    write(
        root,
        "crates/alpha/Cargo.toml",
        "[package]\nname = \"alpha-core\" # dashes become underscores\nversion = \"0.1.0\"\n\n[[bench]]\nname = \"speed\"\n\n[dependencies]\nserde = \"1\"\n",
    );
    write(
        root,
        "crates/alpha/src/lib.rs",
        "//! Alpha core.\npub mod model;\npub mod util;\nmod hidden;\n\npub use model::User;\npub use self::util::helper as h;\n\n/// Library version.\npub const VERSION: &str = \"1\";\n\npub fn version() -> &'static str {\n    VERSION\n}\n",
    );
    write(
        root,
        "crates/alpha/src/model/mod.rs",
        "pub mod user;\npub mod team;\npub use self::user::User;\npub use self::team::*;\n\npub trait Named {\n    fn name(&self) -> String;\n}\n",
    );
    write(
        root,
        "crates/alpha/src/model/user.rs",
        "use super::super::util::helper;\nuse super::Named;\nuse crate::VERSION;\n\n#[derive(Debug, Clone)]\npub struct User {\n    pub id: u64,\n}\n\nimpl User {\n    pub fn new(id: u64) -> Self {\n        helper();\n        Self { id }\n    }\n}\n\nimpl Named for User {\n    fn name(&self) -> String {\n        format!(\"{}{}\", self.id, VERSION)\n    }\n}\n",
    );
    write(
        root,
        "crates/alpha/src/model/team.rs",
        "use super::user::User;\nuse std::collections::HashMap;\n\npub struct Team {\n    pub members: HashMap<u64, User>,\n}\n\npub enum Role { Lead, Member }\n",
    );
    write(
        root,
        "crates/alpha/src/util.rs",
        "use crate::model::User;\nuse crate::model::{Named, Team};\n\npub fn helper() {}\n\npub(crate) fn describe(u: &User) -> String {\n    u.name()\n}\n\nstatic COUNTER: u32 = 0;\ntype Alias = Team;\n",
    );
    write(
        root,
        "crates/alpha/src/hidden.rs",
        "use crate::util::describe;\nuse crate::missing::Nothing;\n",
    );
    write(
        root,
        "crates/alpha/tests/it.rs",
        "use alpha_core::User;\nuse alpha_core::model::Named;\n\n#[test]\nfn works() {\n    let _ = User::new(1);\n}\n",
    );
    write(
        root,
        "crates/omega/Cargo.toml",
        "[package]\nname = 'omega'\nedition = \"2021\"\n\n[dependencies]\nalpha-core = { path = \"../alpha\" }\n",
    );
    write(
        root,
        "crates/omega/src/main.rs",
        "mod cli;\nmod prelude;\n\nuse alpha_core::model::user::User;\nuse alpha_core::{model, util};\nuse crate::cli::run;\nuse std::io::Write as _;\n\nfn main() {\n    run();\n    let _u = User::new(2);\n}\n",
    );
    write(
        root,
        "crates/omega/src/cli.rs",
        "pub mod args;\nuse super::*;\nuse self::args::Args;\n\npub fn run() {\n    let _ = Args::default();\n}\n",
    );
    write(
        root,
        "crates/omega/src/cli/args.rs",
        "use super::super::prelude::*;\nuse crate::cli::run;\n\n#[derive(Default)]\npub struct Args {\n    pub verbose: bool,\n}\n",
    );
    write(
        root,
        "crates/omega/src/prelude.rs",
        "pub use alpha_core::User;\npub use alpha_core::util::helper;\npub use crate::cli::args::Args;\npub(crate) use alpha_core::version;\n",
    );
    write(
        root,
        "crates/omega/config/settings.yaml",
        "log:\n  level: info\nfeatures:\n  - a\n  - b\n",
    );
    write(
        root,
        "crates/omega/config/defaults.json",
        "{\"name\": \"omega\", \"retries\": 3}",
    );
    write(
        root,
        "rustfmt.toml",
        "edition = \"2021\"\n[format]\nmax_width = 100\n",
    );
    write(
        root,
        "scripts/gen.rs",
        "use crate::util::helper;\nuse alpha_core::model;\nuse std::fs;\n\nfn main() {}\n",
    );
    write(root, ".gitignore", "target/\n");
    write(root, "target/debug/junk.rs", "fn junk() {}\n");
}

/// One crate with enough files for several write batches.
fn scenario_many_modules(root: &Path, n: usize) {
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"many\"\nversion = \"0.1.0\"\n",
    );
    let mut lib = String::from("//! Many modules.\n");
    for i in 0..n {
        lib.push_str(&format!("pub mod m{i:03};\n"));
    }
    lib.push_str("pub mod nested;\n\npub fn root() {}\n");
    write(root, "src/lib.rs", &lib);
    for i in 0..n {
        let prev = (i + n - 1) % n;
        let far = (i * 7 + 3) % n;
        let body = if i % 10 == 0 {
            // Import-only module: its owner keeps its edges in the rank graph.
            format!("pub use crate::m{prev:03}::f{prev};\npub use super::m{far:03};\n")
        } else if i % 10 == 5 {
            String::new()
        } else {
            format!(
                "use crate::m{prev:03}::f{prev};\nuse super::m{far:03}::{{S{far}, f{far}}};\nuse crate::nested::deep::Deep;\n\n/// Function {i}.\npub fn f{i}() {{\n    f{prev}();\n}}\n\npub struct S{i} {{\n    pub x: i32,\n}}\n\nimpl S{i} {{\n    pub fn new() -> Self {{ Self {{ x: {i} }} }}\n}}\n"
            )
        };
        write(root, &format!("src/m{i:03}.rs"), &body);
    }
    write(
        root,
        "src/nested/mod.rs",
        "pub mod deep;\nuse self::deep::Deep;\n",
    );
    write(
        root,
        "src/nested/deep.rs",
        "use super::super::m001::f1;\npub struct Deep;\npub fn f1() {}\n",
    );
    for i in 0..12 {
        write(
            root,
            &format!("data/d{i:02}.json"),
            &format!("{{\"k{i}\": {{\"v\": {i}}}}}"),
        );
    }
    write(root, "data/pipeline.yaml", "steps:\n  - build\n  - test\n");
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_matches_wrongstack_on_rust_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();

    let s = base.join("rs-walk");
    let root = s.join("project");
    scenario_cargo_workspace(&root);
    failures.extend(compare("rs-workspace-walk", &root, &s));
    // The scenario exercises what it is meant to (checked on the native
    // index; the WrongStack one is identical).
    let idx = s.join("idx-native");
    let packages = scalar(
        &idx,
        "SELECT group_concat(DISTINCT package) FROM (SELECT package FROM files ORDER BY package)",
    );
    assert_eq!(packages, "(root),crate:alpha-core,crate:omega");
    let resolved = scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''");
    let unresolved = scalar(&idx, "SELECT count(*) FROM refs WHERE to_file IS NULL");
    let ranked = scalar(&idx, "SELECT count(*) FROM symbol_rank");
    eprintln!(
        "rs-workspace-walk: {resolved} resolved / {unresolved} unresolved imports, {ranked} ranked symbols"
    );
    assert!(resolved.parse::<usize>().unwrap() >= 15);
    assert!(unresolved.parse::<usize>().unwrap() >= 3);
    assert!(ranked.parse::<usize>().unwrap() > 0);

    let s = base.join("rs-git");
    let root = s.join("project");
    scenario_cargo_workspace(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    write(
        &root,
        "crates/omega/src/cli.rs",
        "pub mod args;\nuse self::args::Args;\npub fn run() {}\n",
    );
    write(&root, "crates/omega/src/extra.rs", "use crate::cli::run;\n");
    failures.extend(compare("rs-workspace-git", &root, &s));

    let s = base.join("rs-many");
    let root = s.join("project");
    scenario_many_modules(&root, 130);
    failures.extend(compare("rs-many-modules", &root, &s));
    let idx = s.join("idx-native");
    let ranked = scalar(&idx, "SELECT count(*) FROM file_rank");
    let resolved = scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''");
    eprintln!("rs-many-modules: {resolved} resolved imports, {ranked} ranked files");
    assert!(ranked.parse::<usize>().unwrap() >= 10);

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_matches_wrongstack_on_generated_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();

    // 1. Plain directory (filesystem walk).
    let s = base.join("walk");
    let root = s.join("project");
    scenario_files(&root);
    failures.extend(compare("walk", &root, &s));

    // 2. Clean Git checkout: git_blob trust stamps.
    let s = base.join("git-clean");
    let root = s.join("project");
    scenario_files(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("git-clean", &root, &s));
    let stamped = |idx: &Path| -> usize {
        let c = rusqlite::Connection::open_with_flags(
            idx.join("index.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        c.query_row("SELECT count(*) FROM files WHERE git_blob <> ''", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap() as usize
    };
    assert_eq!(
        stamped(&s.join("idx-native")),
        67,
        "every clean file is stamped"
    );

    // 3. Git checkout with modified, staged-new and untracked files.
    let s = base.join("git-dirty");
    let root = s.join("project");
    scenario_files(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    write(
        &root,
        "config/app.yaml",
        "server:\n  port: 9090\nchanged: yes\n",
    );
    write(&root, "staged.json", "{\"staged\": 1}");
    git(&root, &["add", "staged.json"]);
    write(&root, "untracked.yaml", "fresh: 1\n");
    std::fs::remove_file(root.join("many/f001.json")).unwrap();
    failures.extend(compare("git-dirty", &root, &s));
    // The modified and the untracked file carry no stamp; a staged-new file
    // whose working copy equals the staged blob is clean and is stamped.
    assert_eq!(stamped(&s.join("idx-native")), 66);

    // 4. Monorepo-style layout: the package label comes from the path.
    let s = base.join("layout");
    let root = s.join("packages").join("widgets");
    write(&root, "package.json", "{\"name\": \"widgets\"}");
    write(&root, "conf/x.yaml", "a: 1\n");
    failures.extend(compare("layout", &root, &s));

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A Rust crate with its documentation and schema files: README/CHANGELOG/
/// docs with relative, anchored, wiki and external links, SQL with foreign
/// keys, Protobuf with imports and RPC types, GraphQL with interfaces and
/// field types, and the build/infra files the regex extractor reads
/// (Dockerfile, Makefile, CMakeLists.txt, Terraform).
fn scenario_documented_crate(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"documented\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
    );
    write(
        root,
        "src/lib.rs",
        "//! Documented crate.\npub mod db;\n\npub use db::Store;\n\npub fn open() -> Store {\n    Store::default()\n}\n",
    );
    write(
        root,
        "src/db.rs",
        "use crate::open;\n\n#[derive(Default)]\npub struct Store;\n\nimpl Store {\n    pub fn users(&self) {}\n}\n",
    );
    write(
        root,
        "README.md",
        "# Documented\n\nSee the [guide](docs/guide.md), the [API](./docs/api.md#store) and the\n[changelog](CHANGELOG.md). ![logo](assets/logo.png)\n[Home](https://example.com) [[Glossary]] [[docs/api|API docs]]\n\n## Install\n\n```bash\n# not a heading\ncargo add documented\n```\n\n## Usage\n[missing](./nope.md) [src](src/lib.rs)\n",
    );
    write(
        root,
        "CHANGELOG.md",
        "# Changelog\n\n## [0.1.0] - 2026-01-01\n### Added\n- Initial release ([#1](https://github.com/x/y/pull/1))\n- See [README](README.md)\n",
    );
    write(
        root,
        "docs/guide.md",
        "---\ntitle: Guide\n---\n# Guide\n\nBack to [README](../README.md). Next: [API](api.md).\n\n## Store\n\n~~~sql\n# inside tilde fence\n~~~\n",
    );
    write(
        root,
        "docs/api.md",
        "# API\r\n\r\n## Store\r\n\r\n[Guide](guide.md#store) [schema](../db/schema.sql)\r\n",
    );
    write(
        root,
        "docs/Glossary.md",
        "# Glossary\n\n## Store\nA [[Guide]] term.\n",
    );
    write(
        root,
        "db/schema.sql",
        "-- schema\nCREATE TABLE teams (id INTEGER PRIMARY KEY);\nCREATE TABLE users (\n  id INTEGER PRIMARY KEY,\n  team_id INTEGER REFERENCES teams(id)\n);\nCREATE VIEW active AS SELECT * FROM users;\nCREATE INDEX idx_team ON users(team_id);\n",
    );
    write(
        root,
        "db/migrations/002_audit.sql",
        "create table if not exists audit (user_id int references users(id), team int REFERENCES \"teams\"(id));\n",
    );
    write(
        root,
        "proto/store.proto",
        "syntax = \"proto3\";\npackage store.v1;\nimport \"proto/common.proto\";\n\nmessage User {\n  string name = 1;\n}\nmessage GetUserRequest { string id = 1; }\nservice StoreService {\n  rpc GetUser (GetUserRequest) returns (User);\n  rpc Ping (string) returns (store.v1.Pong);\n}\n",
    );
    write(
        root,
        "proto/common.proto",
        "syntax = \"proto3\";\nimport \"proto/store.proto\";\nmessage Pong {}\nenum Status { OK = 0; }\n",
    );
    write(
        root,
        "graphql/schema.graphql",
        "scalar DateTime\ninterface Node {\n  id: ID!\n}\ntype User implements Node {\n  id: ID!\n  team: Team\n  created: DateTime\n}\ntype Team implements Node & Entity {\n  id: ID!\n  members(first: Int): [User!]!\n}\ntype Query {\n  user(id: ID!): User\n}\n",
    );
    write(
        root,
        "graphql/ops.gql",
        "query GetUser($id: ID!) {\n  user(id: $id) { id }\n}\n",
    );
    write(
        root,
        "Dockerfile",
        "FROM rust:1.80 AS build\nARG PROFILE=release\nCOPY . .\nRUN cargo build\nFROM debian:stable-slim\nCOPY --from=build /app /app\nEXPOSE 8080\n",
    );
    write(
        root,
        "Makefile",
        ".PHONY: all\nall: build test\nbuild:\n\tcargo build\ntest: build\n\tcargo test\n",
    );
    write(
        root,
        "native/CMakeLists.txt",
        "project(native)\nadd_library(shim STATIC shim.c)\n",
    );
    write(
        root,
        "infra/main.tf",
        "module \"net\" {\n  source = \"./modules/net\"\n}\nresource \"aws_s3_bucket\" \"logs\" {\n  bucket = var.bucket\n}\nvariable \"bucket\" {}\noutput \"arn\" { value = module.net.arn }\n",
    );
    write(root, "infra/prod.tfvars", "bucket = \"logs\"\n");
}

/// A static site: HTML pages linking scripts/styles, CSS/SCSS/LESS with
/// imports between them, and Markdown content.
fn scenario_static_site(root: &Path) {
    write(
        root,
        "index.html",
        "<!doctype html>\n<html>\n<head>\n  <link rel=\"stylesheet\" href=\"css/site.css\">\n  <link rel=\"icon\" href=\"favicon.ico\">\n  <script src=\"js/app.js\" defer></script>\n</head>\n<body>\n  <nav id=\"main-nav\"></nav>\n  <section id='hero'></section>\n  <a href=\"about.html\" id=\"about-link\">About</a>\n</body>\n</html>\n",
    );
    write(
        root,
        "about.html",
        "<html><head><link href=\"css/site.css\" rel=\"stylesheet\"></head>\n<body><div id=\"about\"></div><div id=\"main-nav\"></div></body></html>\n",
    );
    write(
        root,
        "blog/post.htm",
        "<link rel=\"stylesheet\" href=\"../css/site.css\">\n<article id=\"post\"></article>\n",
    );
    write(
        root,
        "css/site.css",
        "@import \"reset.css\";\n@import url(\"theme.css\");\n.btn {\n  color: red;\n}\n#main-nav {\n}\n@media screen and (max-width: 600px) {\n  .btn { }\n}\n@keyframes spin {\n}\n",
    );
    write(root, "css/reset.css", "html {\n}\nbody {\n}\n");
    write(
        root,
        "css/theme.css",
        "@import \"site.css\";\n:root {\n}\n.theme-dark {\n}\n",
    );
    write(
        root,
        "scss/main.scss",
        "@use 'sass:math';\n@use \"partials/base\";\n@forward \"partials/mixins\";\n.card {\n  .title {\n  }\n}\n",
    );
    write(root, "scss/partials/base.scss", ".base {\n}\n");
    write(
        root,
        "less/theme.less",
        "@import \"vars.less\";\n.box {\n}\n",
    );
    write(
        root,
        "content/index.md",
        "# Welcome\n\nRead [about](../about.html) and the [post](posts/first.md).\n",
    );
    write(
        root,
        "content/posts/first.md",
        "# First post\n\n## Details\n[home](../index.md) [[index]]\n",
    );
}

/// The remaining regex-extracted languages, each with symbols and imports.
fn scenario_misc_languages(root: &Path) {
    write(
        root,
        "scala/Main.scala",
        "package demo\nimport demo.util.Helper\nimport scala.collection.mutable\nobject Main {\n  def main(args: Array[String]): Unit = Helper.run()\n}\n",
    );
    write(
        root,
        "scala/util/Helper.scala",
        "package demo.util\nobject Helper {\n  def run(): Unit = {}\n}\ntrait Runner\n",
    );
    write(
        root,
        "dart/main.dart",
        "import 'src/util.dart';\nimport 'package:flutter/material.dart';\nclass App {\n  void build() {\n    helper();\n  }\n}\n",
    );
    write(
        root,
        "dart/src/util.dart",
        "void helper() {\n}\nclass Util {}\n",
    );
    write(
        root,
        "lua/init.lua",
        "local util = require(\"lua.util\")\nfunction start() end\nlocal function private() end\n",
    );
    write(root, "lua/util.lua", "function M.helper() end\n");
    write(
        root,
        "r/analysis.R",
        "library(dplyr)\nsource(\"r/helpers.R\")\nclean <- function(df) {}\n",
    );
    write(root, "r/helpers.R", "helper = function(x) {}\n");
    write(
        root,
        "zig/main.zig",
        "const std = @import(\"std\");\nconst util = @import(\"util.zig\");\npub fn main() void {}\n",
    );
    write(
        root,
        "zig/util.zig",
        "pub fn helper() void {}\nconst X = 1;\n",
    );
    write(
        root,
        "hs/Main.hs",
        "module Main where\n\nimport Data.Shapes\nimport qualified Data.Map as M\n\nmain :: IO ()\nmain = pure ()\n",
    );
    write(
        root,
        "hs/Data/Shapes.hs",
        "module Data.Shapes (Shape(..), area) where\n\ndata Shape = Circle\narea :: Shape -> Double\nclass HasArea a where\n",
    );
    write(
        root,
        "perl/tool.pl",
        "sub helper { }\nfunction js_like() {}\n",
    );
    write(root, "Procfile", "web: ./run\n");
    write(
        root,
        "justfile",
        "default: build\nbuild:\n    cargo build\n",
    );
}

/// Vue/Svelte single-file components (JS family: their imports go through
/// the JavaScript module resolver).
fn scenario_components(root: &Path) {
    write(root, "package.json", "{\"name\": \"widgets\"}");
    write(
        root,
        "src/App.vue",
        "<template><div id=\"app\"></div></template>\n<script>\nimport Button from './components/Button.svelte'\nimport Card from \"./components/Card.vue\"\nimport { x } from 'external-lib'\nexport default {}\nfunction helper() {}\nconst state = 1\n</script>\n",
    );
    write(
        root,
        "src/components/Card.vue",
        "<script>\nimport Button from './Button.svelte'\nexport const card = 1\n</script>\n",
    );
    write(
        root,
        "src/components/Button.svelte",
        "<script>\n  import Card from './Card.vue';\n  export let label = 'x';\n  function click() {}\n</script>\n",
    );
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_matches_wrongstack_on_generic_language_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    type Scenario = fn(&Path);
    let scenarios: [(&str, Scenario); 4] = [
        ("documented-crate", scenario_documented_crate),
        ("static-site", scenario_static_site),
        ("misc-languages", scenario_misc_languages),
        ("components", scenario_components),
    ];
    for (name, build) in scenarios {
        let s = base.join(name);
        let root = s.join("project");
        build(&root);
        failures.extend(compare(name, &root, &s));
        let idx = s.join("idx-native");
        let langs = scalar(
            &idx,
            "SELECT group_concat(lang) FROM (SELECT DISTINCT lang FROM files ORDER BY lang)",
        );
        let refs = scalar(&idx, "SELECT count(*) FROM refs");
        let resolved = scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''");
        let bound = scalar(&idx, "SELECT count(*) FROM refs WHERE to_id IS NOT NULL");
        eprintln!(
            "{name}: langs {langs}; {refs} refs, {resolved} resolved to a file, {bound} bound to a symbol"
        );
    }
    // The documented crate in a clean Git checkout (blob trust stamps).
    let s = base.join("documented-git");
    let root = s.join("project");
    scenario_documented_crate(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("documented-git", &root, &s));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Real-world projects: `WRONGINDEX_DIFF_PROJECTS` lists directories
/// (separated by `;`) that are copied and indexed both ways.
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE and WRONGINDEX_DIFF_PROJECTS"]
fn native_matches_wrongstack_on_listed_projects() {
    let Ok(list) = std::env::var("WRONGINDEX_DIFF_PROJECTS") else {
        eprintln!("NOT RUN: set WRONGINDEX_DIFF_PROJECTS to compare real projects");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    for (i, dir) in list.split(';').filter(|s| !s.is_empty()).enumerate() {
        let s = base.join(format!("p{i}"));
        let name = Path::new(dir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        failures.extend(compare(&name, Path::new(dir), &s));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
