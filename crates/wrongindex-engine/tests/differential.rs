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
    wrongstack_index_with(root, index, None)
}

/// WrongStack's run, whole-project or (`files`) targeted.
fn wrongstack_index_with(root: &Path, index: &Path, files: Option<&[String]>) -> Value {
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
    let mut req = json!({"id": 1, "op": "index", "args": {"projectRoot": root, "indexDir": index}});
    if let Some(files) = files {
        req["args"]["files"] = json!(files);
    }
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

/// YAML infra artifacts (`infra-parser.ts` refs): Compose services and
/// images, workflow jobs/needs/local and remote actions, OpenAPI `$ref`s,
/// Kubernetes manifests (by directory and by content), and kinds a `.yaml`
/// name selects (`Dockerfile.yaml`, `.env.yaml`).
fn scenario_infra_yaml(root: &Path) {
    write(
        root,
        "package.json",
        "{\"name\": \"infra\", \"scripts\": {\"build\": \"tsc\"}}",
    );
    write(
        root,
        "docker-compose.yml",
        "services:\n  web:\n    build: .\n    image: \"infra/web:1\"\n    depends_on:\n      - db\n      - cache\n  db:\n    image: postgres:16\n  cache:\n    image: redis\n    depends_on:\n      db:\n        condition: service_healthy\n",
    );
    write(
        root,
        "Dockerfile",
        "FROM node:20 AS build\nCOPY --from=build /a /b\nEXPOSE 80\n",
    );
    write(
        root,
        ".github/workflows/ci.yml",
        "name: CI\non: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - uses: ./.github/actions/setup\n  test:\n    needs: build\n    steps:\n      - run: echo\n  release:\n    needs: [build, 'test']\n    uses: ./.github/workflows/release.yml\n",
    );
    write(
        root,
        ".github/workflows/release.yml",
        "on: workflow_call\njobs:\n  publish:\n    runs-on: ubuntu-latest\n",
    );
    write(
        root,
        ".github/actions/setup/action.yml",
        "name: setup\nruns:\n  using: composite\n  steps:\n    - uses: actions/setup-node@v4\n",
    );
    write(
        root,
        "api/openapi.yaml",
        "openapi: 3.1.0\npaths:\n  /users:\n    get:\n      responses:\n        '200':\n          content:\n            application/json:\n              schema:\n                $ref: '#/components/schemas/User'\n  /users/{id}:\n    get: {}\ncomponents:\n  schemas:\n    User:\n      properties:\n        pet:\n          $ref: '#/components/schemas/Pet'\n    Pet:\n      type: object\n",
    );
    write(
        root,
        "k8s/deploy.yaml",
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  template:\n    spec:\n      containers:\n        - name: web\n          image: infra/web:1\n      volumes:\n        - persistentVolumeClaim:\n            claimName: data\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web-svc\nspec:\n  backend:\n    serviceName: web\n",
    );
    write(
        root,
        "config/pvc.yml",
        "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: data\n",
    );
    write(root, "config/app.yaml", "server:\n  port: 8080\n");
    write(root, "deploy/Dockerfile.yaml", "FROM alpine:3 AS base\n");
    write(root, "deploy/.env.yaml", "A=1\nB=${A}\n");
    write(
        root,
        "README.md",
        "# Infra\n\nSee [API](api/openapi.yaml) and [k8s](k8s/deploy.yaml).\n",
    );
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_matches_wrongstack_on_generic_language_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    type Scenario = fn(&Path);
    let scenarios: [(&str, Scenario); 5] = [
        ("documented-crate", scenario_documented_crate),
        ("static-site", scenario_static_site),
        ("misc-languages", scenario_misc_languages),
        ("components", scenario_components),
        ("infra-yaml", scenario_infra_yaml),
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
    // The infra project in a clean Git checkout.
    let s = base.join("infra-git");
    let root = s.join("project");
    scenario_infra_yaml(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("infra-git", &root, &s));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// npm workspace with two TypeScript packages and some JavaScript: relative
/// and package imports, re-export barrels, classes/interfaces/enums, JSX,
/// CommonJS, docs, `<module>` owners.
fn scenario_npm_monorepo(root: &Path) {
    write(
        root,
        "package.json",
        "{\n  \"name\": \"demo-root\",\n  \"private\": true,\n  \"workspaces\": [\"packages/*\"]\n}\n",
    );
    write(
        root,
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"strict\": true,\n    \"jsx\": \"react-jsx\",\n    \"paths\": {\"@demo/*\": [\"packages/*/src\"]}\n  }\n}\n",
    );
    write(
        root,
        "packages/core/package.json",
        "{\n  \"name\": \"@demo/core\",\n  \"version\": \"1.0.0\",\n  \"main\": \"src/index.ts\",\n  \"exports\": {\".\": \"./src/index.ts\", \"./model\": \"./src/model/index.ts\"}\n}\n",
    );
    write(
        root,
        "packages/core/src/index.ts",
        "/** Core barrel. */\nexport * from './model';\nexport { Store, type StoreOptions } from './store';\nexport * as util from './util/strings';\nexport { default as createStore } from './store';\n",
    );
    write(
        root,
        "packages/core/src/model/index.ts",
        "export * from './user';\nexport * from './team';\nexport type { Id } from './ids';\n",
    );
    write(
        root,
        "packages/core/src/model/ids.ts",
        "/** Opaque identifier. */\nexport type Id = string & { readonly __brand: 'Id' };\n\nexport const enum Kind {\n  User = 'user',\n  Team = 'team',\n}\n\nexport function makeId(raw: string): Id {\n  return raw as Id;\n}\n",
    );
    write(
        root,
        "packages/core/src/model/user.ts",
        "import { makeId, type Id } from './ids';\nimport { capitalize } from '../util/strings';\n\n/** Anything with a display name. */\nexport interface Named {\n  readonly name: string;\n  label(): string;\n}\n\nexport abstract class Entity<T extends Id = Id> {\n  constructor(public readonly id: T) {}\n  abstract describe(): string;\n}\n\n/**\n * A user of the system.\n */\nexport class User extends Entity implements Named {\n  static count = 0;\n  #secret = '';\n  constructor(id: string, public name: string) {\n    super(makeId(id));\n    User.count++;\n  }\n  label(): string {\n    return capitalize(this.name);\n  }\n  get initials(): string {\n    return this.name.slice(0, 2);\n  }\n  describe = (): string => `${this.id}:${this.label()}`;\n}\n",
    );
    write(
        root,
        "packages/core/src/model/team.ts",
        "import { User, type Named } from './user';\n\nexport class Team implements Named {\n  private members: User[] = [];\n  constructor(public name: string) {}\n  add(user: User): this {\n    this.members.push(user);\n    return this;\n  }\n  label(): string {\n    return `${this.name} (${this.members.length})`;\n  }\n}\n\nexport namespace Team {\n  export const MAX = 10;\n}\n",
    );
    write(
        root,
        "packages/core/src/store.ts",
        "import type { Id } from './model/ids';\nimport { User } from './model';\n\nexport interface StoreOptions {\n  capacity?: number;\n}\n\nexport class Store<T extends { id: Id }> {\n  private readonly items = new Map<Id, T>();\n  constructor(private options: StoreOptions = {}) {}\n  put(item: T): void {\n    this.items.set(item.id, item);\n  }\n  get(id: Id): T | undefined {\n    return this.items.get(id);\n  }\n}\n\nexport default function createStore(): Store<User> {\n  return new Store<User>();\n}\n",
    );
    write(
        root,
        "packages/core/src/util/strings.ts",
        "export const capitalize = (s: string): string => s.charAt(0).toUpperCase() + s.slice(1);\n\nexport function pad(s: string, n = 2): string {\n  return s.padStart(n, '0');\n}\n",
    );
    write(
        root,
        "packages/core/test/user.test.ts",
        "import { describe, expect, it } from 'vitest';\nimport { User } from '../src';\n\ndescribe('User', () => {\n  it('labels', () => {\n    expect(new User('1', 'ada').label()).toBe('Ada');\n  });\n});\n",
    );
    write(
        root,
        "packages/app/package.json",
        "{\n  \"name\": \"@demo/app\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\"@demo/core\": \"workspace:*\", \"react\": \"^19.0.0\"}\n}\n",
    );
    write(
        root,
        "packages/app/src/main.tsx",
        "import React, { useState } from 'react';\nimport { createRoot } from 'react-dom/client';\nimport { User, Team, util } from '@demo/core';\nimport type { Named } from '@demo/core/model';\nimport { App } from './App';\nimport './styles.css';\n\nconst team = new Team('core').add(new User('1', 'ada'));\n\nexport function mount(el: HTMLElement): void {\n  createRoot(el).render(<App team={team} title={util.capitalize('demo')} />);\n}\n\nexport const label = (n: Named) => n.label();\n",
    );
    write(
        root,
        "packages/app/src/App.tsx",
        "import * as React from 'react';\nimport type { Team } from '@demo/core';\nimport { Layout } from './components/Layout';\n\nexport interface AppProps {\n  team: Team;\n  title: string;\n}\n\n/** Root component. */\nexport function App({ team, title }: AppProps) {\n  const [open, setOpen] = React.useState(false);\n  return (\n    <Layout.Page title={title}>\n      <Layout.Header onClick={() => setOpen(!open)}>{team.label()}</Layout.Header>\n      {open && <p>{title}</p>}\n    </Layout.Page>\n  );\n}\n\nexport default App;\n",
    );
    write(
        root,
        "packages/app/src/components/Layout.tsx",
        "import type { ReactNode } from 'react';\n\ntype Props = { title?: string; children?: ReactNode; onClick?: () => void };\n\nfunction Page({ title, children }: Props) {\n  return <main aria-label={title}>{children}</main>;\n}\n\nfunction Header({ children, onClick }: Props) {\n  return <header onClick={onClick}>{children}</header>;\n}\n\nexport const Layout = { Page, Header };\n",
    );
    write(
        root,
        "packages/app/src/styles.css",
        ".app {\n  color: red;\n}\n",
    );
    write(
        root,
        "packages/app/src/legacy.js",
        "'use strict';\nconst path = require('path');\nconst { pad } = require('@demo/core/src/util/strings');\n\nfunction Legacy(name) {\n  this.name = name;\n}\nLegacy.prototype.render = function () {\n  return path.join(pad(this.name), 'x');\n};\n\nmodule.exports = { Legacy };\n",
    );
    write(
        root,
        "packages/app/src/esm.mjs",
        "import { Legacy } from './legacy.js';\nexport const legacy = new Legacy('a');\nexport default class Widget {\n  render() {\n    return legacy.render();\n  }\n}\n",
    );
    write(
        root,
        "packages/app/src/view.jsx",
        "import React from 'react';\nimport Widget from './esm.mjs';\nexport function View(props) {\n  return <section>{new Widget().render()}{props.children}</section>;\n}\n",
    );
    write(
        root,
        "packages/app/scripts/build.cjs",
        "#!/usr/bin/env node\nconst fs = require('node:fs');\nif (!fs.existsSync('dist')) return;\nfs.rmSync('dist', { recursive: true });\n",
    );
    write(
        root,
        "packages/app/types/env.d.ts",
        "declare module '*.css';\ndeclare global {\n  interface Window {\n    demo?: string;\n  }\n}\nexport {};\n",
    );
    write(root, "dist/bundle.js", "function ignored() {}\n");
    write(root, ".gitignore", "dist/\n");
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_matches_wrongstack_on_typescript_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();

    let s = base.join("ts-walk");
    let root = s.join("project");
    scenario_npm_monorepo(&root);
    failures.extend(compare("ts-monorepo-walk", &root, &s));
    let idx = s.join("idx-native");
    let langs = scalar(
        &idx,
        "SELECT group_concat(lang) FROM (SELECT DISTINCT lang FROM files ORDER BY lang)",
    );
    let packages = scalar(
        &idx,
        "SELECT group_concat(DISTINCT package) FROM (SELECT package FROM files ORDER BY package)",
    );
    let refs = scalar(&idx, "SELECT count(*) FROM refs");
    let resolved = scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''");
    let bound = scalar(&idx, "SELECT count(*) FROM refs WHERE to_id IS NOT NULL");
    eprintln!(
        "ts-monorepo-walk: langs {langs}; packages {packages}; {refs} refs, {resolved} resolved to a file, {bound} bound to a symbol"
    );
    assert!(langs.contains("ts") && langs.contains("tsx") && langs.contains("js"));
    assert!(resolved.parse::<usize>().unwrap() >= 15, "{resolved}");
    assert!(bound.parse::<usize>().unwrap() >= 15, "{bound}");

    // Clean Git checkout, then a dirty one (modified, staged-new, untracked).
    let s = base.join("ts-git");
    let root = s.join("project");
    scenario_npm_monorepo(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("ts-monorepo-git", &root, &s));

    let s = base.join("ts-git-dirty");
    let root = s.join("project");
    scenario_npm_monorepo(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    write(
        &root,
        "packages/core/src/util/strings.ts",
        "export function capitalize(s: string): string {\n  return s.toUpperCase();\n}\nexport function pad(s: string): string {\n  return s;\n}\n",
    );
    write(
        &root,
        "packages/core/src/extra.ts",
        "export { pad as padLeft } from './util/strings';\n",
    );
    git(&root, &["add", "packages/core/src/extra.ts"]);
    write(
        &root,
        "packages/app/src/untracked.ts",
        "import { App } from './App';\nexport const app = App;\n",
    );
    failures.extend(compare("ts-monorepo-git-dirty", &root, &s));

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

/// Python package (interpreter `ast` route): relative and absolute imports,
/// classes/methods/properties/async/type aliases, constants, a syntax error
/// (regex import fallback), a BOM, non-ASCII identifiers and a script.
fn scenario_python(root: &Path) {
    write(
        root,
        "pyproject.toml",
        "[project]\nname = \"shop\"\nversion = \"0.1.0\"\n",
    );
    write(
        root,
        "shop/__init__.py",
        "from .models import Order, Item\nfrom . import util\n__all__ = [\"Order\"]\nVERSION = \"1\"\n",
    );
    write(
        root,
        "shop/models.py",
        "\"\"\"Models.\"\"\"\nfrom dataclasses import dataclass\nfrom typing import Optional\nfrom .util import slug as make_slug, clamp\n\n\n@dataclass\nclass Item:\n    \"\"\"An item.\n\n    More.\n    \"\"\"\n    name: str\n    price: float = 0.0\n\n    @property\n    def label(self) -> str:\n        return make_slug(self.name)\n\n\nclass Order(Item, object):\n    MAX: int = 10\n    _hidden = 1\n    __private = 2\n\n    def total(self, *items, scale=1, **kw) -> Optional[float]:\n        return clamp(sum(i.price for i in items) * scale)\n\n    async def fetch(self, /, a, *, b):\n        await self.total()\n\n    @staticmethod\n    def make(x: int) -> \"Order\":\n        return Order(str(x))\n\ntype Price = float\na, (b, *c) = 1, (2, 3)\n",
    );
    write(
        root,
        "shop/util.py",
        "import os.path\nimport re as regex\n\ndef slug(s):\n    return regex.sub(r'[^a-z]+', '-', s).strip('-').lower()\n\ndef clamp(x, lo=0, hi=100):\n    return max(lo, min(hi, x))\n\ndef ünïcode_name():\n    return os.path.join('a', 'b')\n",
    );
    write(
        root,
        "shop/broken.py",
        "import json\nfrom .models import Order\n\ndef oops(:\n    pass\n",
    );
    write(
        root,
        "shop/bom.py",
        "\u{feff}import shop.util\nX = shop.util.clamp(5)\n",
    );
    write(
        root,
        "scripts/run.py",
        "#!/usr/bin/env python3\nimport sys\nfrom shop import models\nfrom shop.models import Order\n\nif __name__ == \"__main__\":\n    Order.make(int(sys.argv[1]))\n",
    );
    write(
        root,
        "tests/test_util.py",
        "from shop.util import slug\n\ndef test_slug():\n    assert slug('A B') == 'a-b'\n",
    );
    write(
        root,
        "README.md",
        "# Shop\n\nSee [models](shop/models.py).\n",
    );
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine), bun and Python"]
fn native_matches_wrongstack_on_python_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    let s = base.join("python");
    let root = s.join("project");
    scenario_python(&root);
    failures.extend(compare("python", &root, &s));
    let idx = s.join("idx-native");
    eprintln!(
        "python: {} refs, {} resolved to a file, {} bound to a symbol",
        scalar(&idx, "SELECT count(*) FROM refs"),
        scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''"),
        scalar(&idx, "SELECT count(*) FROM refs WHERE to_id IS NOT NULL"),
    );
    let s = base.join("python-git");
    let root = s.join("project");
    scenario_python(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("python-git", &root, &s));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Go module (`go run` of WrongStack's `go/ast` program): packages and
/// module-path imports, methods with generic receivers, grouped declarations,
/// a syntax error (regex fallback), a test file, an aliased/dot import.
fn scenario_go(root: &Path) {
    write(root, "go.mod", "module example.com/shop\n\ngo 1.22\n");
    write(
        root,
        "main.go",
        "package main\n\nimport (\n\t\"fmt\"\n\tm \"example.com/shop/internal/models\"\n\t. \"example.com/shop/internal/util\"\n)\n\nfunc main() {\n\to := m.NewOrder(3)\n\tfmt.Println(o.Total(), Clamp(5))\n}\n",
    );
    write(
        root,
        "internal/models/order.go",
        "// Package models holds orders.\npackage models\n\nimport \"example.com/shop/internal/util\"\n\n// Order is an order.\ntype Order struct {\n\tItems []Item\n}\n\ntype (\n\tItem struct{ Price float64 }\n\tID   int\n)\n\nconst (\n\tMax, Min = 10, 0\n\t_        = 3\n)\n\nvar Default = NewOrder(0)\n\n// NewOrder makes one.\nfunc NewOrder(n int) *Order {\n\treturn &Order{Items: make([]Item, n)}\n}\n\n// Total sums.\nfunc (o *Order) Total() float64 {\n\tvar t float64\n\tfor _, i := range o.Items {\n\t\tt += i.Price\n\t}\n\treturn util.Clamp(t)\n}\n\ntype Set[T comparable] struct{ m map[T]struct{} }\n\nfunc (s *Set[T]) Has(x T) bool { _, ok := s.m[x]; return ok }\n\ntype Shape interface {\n\tArea() float64\n}\n",
    );
    write(
        root,
        "internal/util/util.go",
        "package util\n\nimport \"math\"\n\nfunc Clamp(x float64) float64 { return math.Max(0, math.Min(100, x)) }\n",
    );
    write(
        root,
        "internal/util/util_test.go",
        "package util\n\nimport \"testing\"\n\nfunc TestClamp(t *testing.T) {\n\tif Clamp(200) != 100 {\n\t\tt.Fatal(\"x\")\n\t}\n}\n",
    );
    write(
        root,
        "internal/broken/broken.go",
        "package broken\n\nimport \"fmt\"\n\nfunc Ok() { fmt.Println(1) }\n\nfunc Bad( {\n}\n",
    );
    write(
        root,
        "internal/broken/half.go",
        "package broken\n\nimport \"strings\"\n\nfunc Half() string { return strings.Repeat(\"a\", 2) \n",
    );
    // Balanced but invalid: `go/parser` rejects it, the regex fallback keeps
    // its declarations.
    write(
        root,
        "internal/broken/balanced.go",
        "package broken\n\nimport \"os\"\n\nfunc Exit() { os.Exit(1) }\n\nconst (\n\tA, B = 1, 2\n)\n\nvar x = = 1\n\nfunc (r *Recv) M() {}\n",
    );
    write(
        root,
        "README.md",
        "# Shop\n\nSee [models](internal/models/order.go).\n",
    );
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine), bun and Go"]
fn native_matches_wrongstack_on_go_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    let s = base.join("go");
    let root = s.join("project");
    scenario_go(&root);
    failures.extend(compare("go", &root, &s));
    let idx = s.join("idx-native");
    eprintln!(
        "go: {} refs, {} resolved to a file, {} bound to a symbol",
        scalar(&idx, "SELECT count(*) FROM refs"),
        scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''"),
        scalar(&idx, "SELECT count(*) FROM refs WHERE to_id IS NOT NULL"),
    );
    let s = base.join("go-git");
    let root = s.join("project");
    scenario_go(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("go-git", &root, &s));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The ten tree-sitter languages (WrongStack's own visitor, queries, runtime
/// and grammars), files the grammar yields no symbols for (regex fallback,
/// shell `source` infra refs), TS/JS the oxc port refuses (WrongStack's
/// TypeScript parser), and a file over 5 MiB (skipped, batch slot kept).
fn scenario_polyglot(root: &Path) {
    write(
        root,
        "src/util.h",
        "#ifndef UTIL_H\n#define UTIL_H\nint add(int a, int b);\nstruct point { int x, y; };\n#endif\n",
    );
    write(
        root,
        "src/util.c",
        "#include \"util.h\"\n#include <stdio.h>\n\nstatic int helper(int x) { return x * 2; }\nint add(int a, int b) { return helper(a) + b; }\ntypedef struct node { struct node *next; } node_t;\nenum color { RED, GREEN };\n",
    );
    write(
        root,
        "src/engine.cpp",
        "#include \"util.h\"\nnamespace eng {\nclass Engine : public Base {\npublic:\n  Engine();\n  void run() { add(1, 2); }\n};\ntemplate <typename T> T id(T x) { return x; }\n}\n",
    );
    write(
        root,
        "java/com/x/App.java",
        "package com.x;\n\nimport java.util.List;\nimport com.x.model.User;\n\n/** App. */\npublic class App extends Base implements Runnable {\n  private int n;\n  public void run() { helper(); new User(); }\n  interface Inner {}\n  enum E { A }\n  record R(int a) {}\n}\n",
    );
    write(
        root,
        "java/com/x/model/User.java",
        "package com.x.model;\npublic class User { public User() {} }\n",
    );
    write(
        root,
        "cs/Program.cs",
        "using System;\nusing App.Models;\nnamespace App {\n  /// <summary>Main.</summary>\n  public class Program : Base, IRun {\n    public static void Main(string[] args) { Console.WriteLine(Helper.Go()); }\n    public int Count { get; set; }\n  }\n  public interface IRun { void Run(); }\n  public struct S {}\n  public enum E { A }\n}\n",
    );
    write(
        root,
        "php/src/User.php",
        "<?php\nnamespace App\\Models;\n\nuse App\\Contracts\\HasName;\nrequire_once 'helpers.php';\n\ninterface Named {}\ntrait Greets { public function hi() {} }\nclass User extends Model implements Named {\n    use Greets;\n    const MAX = 3;\n    public function name(): string { return strtoupper($this->n); }\n}\nfunction helper() { return new User(); }\n",
    );
    write(
        root,
        "php/src/helpers.php",
        "<?php\nfunction format_name($n) { return trim($n); }\n",
    );
    write(
        root,
        "ruby/lib/shop.rb",
        "require 'json'\nrequire_relative 'shop/item'\n\nmodule Shop\n  class Cart < Base\n    include Enumerable\n    MAX = 10\n    def add(item) = items << item\n    def self.build; new; end\n    def total?\n      items.sum(&:price)\n    end\n  end\nend\n",
    );
    write(
        root,
        "ruby/lib/shop/item.rb",
        "module Shop\n  class Item\n    attr_reader :price\n  end\nend\n",
    );
    write(
        root,
        "swift/Sources/App.swift",
        "import Foundation\n\nprotocol Shape { func area() -> Double }\nstruct Circle: Shape {\n  let r: Double\n  func area() -> Double { return 3.14 * r * r }\n}\nclass View: NSObject {\n  var name = \"v\"\n  func draw() { print(Circle(r: 1).area()) }\n}\nenum Kind { case a, b }\nextension View {}\nactor Counter {}\n",
    );
    write(
        root,
        "kotlin/src/Main.kt",
        "package app\n\nimport app.model.User\n\ninterface Repo { fun all(): List<User> }\ndata class Item(val id: Int)\nobject Registry { val items = mutableListOf<Item>() }\nclass Service(private val repo: Repo) : Base(), Repo by repo {\n  fun run() { println(repo.all()) }\n}\nfun main() { Service(TODO()).run() }\nenum class Color { RED }\n",
    );
    write(
        root,
        "scripts/build.sh",
        "#!/usr/bin/env bash\nset -e\nsource ./scripts/env.sh\n. ./scripts/lib.sh\n\nbuild() {\n  echo building\n}\nfunction deploy {\n  build\n}\ndeploy\n",
    );
    write(root, "scripts/env.sh", "export A=1\n");
    write(
        root,
        "scripts/lib.sh",
        "# only comments\nsource ./scripts/env.sh\n",
    );
    write(
        root,
        "elixir/lib/shop.ex",
        "defmodule Shop.Cart do\n  alias Shop.Item\n  import Enum\n  @moduledoc \"Cart.\"\n  defstruct items: []\n  def add(cart, item), do: %{cart | items: [item | cart.items]}\n  defp total(cart), do: sum(map(cart.items, & &1.price))\n  defmacro m(x), do: x\nend\n",
    );
    write(
        root,
        "elixir/test/shop_test.exs",
        "defmodule ShopTest do\n  use ExUnit.Case\n  test \"x\" do\n    assert 1 == 1\n  end\nend\n",
    );
    // TS/JS the oxc port refuses: syntax errors TypeScript recovers from and a
    // script-level `await` TypeScript reads as an identifier.
    write(
        root,
        "web/broken.ts",
        "export class A {\n  m(: void {}\n}\nexport function ok() { return 1 }\n",
    );
    write(
        root,
        "web/script.js",
        "var await;\nawait (x);\nfunction f() { g(); }\n",
    );
    write(
        root,
        "web/fine.ts",
        "import { ok } from './broken';\nexport const v = ok();\n",
    );
    // Over 5 MiB: a per-file read error in WrongStack, never indexed.
    write(root, "data/huge.sql", &"-- x\n".repeat(1_100_000));
    write(root, "README.md", "# Polyglot\n");
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine), bun or node"]
fn native_matches_wrongstack_on_polyglot_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    let s = base.join("polyglot");
    let root = s.join("project");
    scenario_polyglot(&root);
    failures.extend(compare("polyglot", &root, &s));
    let idx = s.join("idx-native");
    eprintln!(
        "polyglot: langs {}; {} refs, {} resolved to a file, {} bound to a symbol",
        scalar(
            &idx,
            "SELECT group_concat(lang) FROM (SELECT DISTINCT lang FROM files ORDER BY lang)"
        ),
        scalar(&idx, "SELECT count(*) FROM refs"),
        scalar(&idx, "SELECT count(*) FROM refs WHERE to_file <> ''"),
        scalar(&idx, "SELECT count(*) FROM refs WHERE to_id IS NOT NULL"),
    );
    let s = base.join("polyglot-git");
    let root = s.join("project");
    scenario_polyglot(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("polyglot-git", &root, &s));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Experiment (WI-515 design input, prints instead of asserting): how far is
/// WrongStack's own incremental run from its own fresh run over the same
/// final files?
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn experiment_wrongstack_incremental_vs_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    scenario_npm_monorepo(&root);
    let inc = base.join("idx-incremental");
    wrongstack_index(&root, &inc);
    // Edit: change an exported function, add a file, delete a file.
    write(
        &root,
        "packages/core/src/util/strings.ts",
        "export function capitalize(s: string): string {\n  return s.toUpperCase();\n}\nexport function pad(s: string): string {\n  return s;\n}\n",
    );
    write(
        &root,
        "packages/core/src/extra.ts",
        "export { pad as padLeft } from './util/strings';\nimport { capitalize } from './util/strings';\nexport const big = capitalize('x');\n",
    );
    let victim = root.join("packages/core/src/model/team.ts");
    std::fs::remove_file(&victim).unwrap();
    let r = wrongstack_index(&root, &inc);
    eprintln!("incremental run: {r}");
    let fresh = base.join("idx-fresh");
    wrongstack_index(&root, &fresh);
    let v = Volatile::logical();
    let a = dump(&inc.join("index.db"), &v).unwrap();
    let b = dump(&fresh.join("index.db"), &v).unwrap();
    let d = diff(&b, &a, 40);
    eprintln!(
        "deleted {victim:?}; {} differences (fresh -> incremental):",
        d.len()
    );
    for l in &d {
        eprintln!("  {l}");
    }
}

fn copy_index(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with("index.db") {
            std::fs::copy(e.path(), to.join(name)).unwrap();
        }
    }
}

/// One incremental round: WrongStack's own run on `ws` vs the native
/// in-place update on `native`, both over the same edited checkout.
fn incremental_round(name: &str, root: &Path, ws: &Path, native: &Path) -> Vec<String> {
    let r = wrongstack_index(root, ws);
    let n = wrongindex_engine::native::rebuild(root, native)
        .unwrap_or_else(|e| panic!("{name}: native: {e}"));
    let v = Volatile::logical();
    let want = dump(&ws.join("index.db"), &v).unwrap();
    let got = dump(&native.join("index.db"), &v).unwrap();
    let mut out: Vec<String> = diff(&want, &got, 8)
        .into_iter()
        .map(|l| format!("{name}: {l}"))
        .collect();
    let outcome = serde_json::to_value(&n).unwrap();
    eprintln!(
        "{name}: WrongStack changedFiles {} | native {} | {} differences",
        r["changedFiles"],
        outcome,
        out.len()
    );
    if r["changedFiles"].as_u64().unwrap_or(0) > 0 && outcome["outcome"] != "updated" {
        out.push(format!("{name}: native did not update in place: {outcome}"));
    }
    out
}

/// A named edit applied to a checkout between two incremental runs.
type EditRound = (&'static str, Box<dyn Fn(&Path)>);

fn ts_edit_rounds() -> Vec<EditRound> {
    vec![
        (
            "edit+add+delete",
            Box::new(|root: &Path| {
                write(
                    root,
                    "packages/core/src/util/strings.ts",
                    "export function capitalize(s: string): string {\n  return s.toUpperCase();\n}\nexport function pad(s: string): string {\n  return s;\n}\n",
                );
                write(
                    root,
                    "packages/core/src/extra.ts",
                    "export { pad as padLeft } from './util/strings';\nimport { capitalize } from './util/strings';\nexport const big = capitalize('x');\n",
                );
                std::fs::remove_file(root.join("packages/core/src/model/team.ts")).unwrap();
            }),
        ),
        ("no-op", Box::new(|_root: &Path| {})),
        (
            "rename-and-empty",
            Box::new(|root: &Path| {
                let from = root.join("packages/core/src/store.ts");
                let text = std::fs::read_to_string(&from).unwrap();
                std::fs::remove_file(&from).unwrap();
                write(root, "packages/core/src/storage.ts", &text);
                write(root, "packages/app/src/esm.mjs", "");
                write(
                    root,
                    "packages/core/src/model/user.ts",
                    "import { makeId } from './ids';\nexport class Person {\n  constructor(public id = makeId('p')) {}\n}\nexport class User extends Person {}\n",
                );
            }),
        ),
    ]
}

/// WI-515: the native in-place incremental run leaves the same database as
/// WrongStack's own incremental run, round after round.
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_incremental_matches_wrongstack_incremental() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    for with_git in [false, true] {
        let label = if with_git { "git" } else { "walk" };
        let s = base.join(format!("inc-{label}"));
        let root = s.join("project");
        scenario_npm_monorepo(&root);
        if with_git {
            git(&root, &["init", "-q"]);
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "-m", "init"]);
        }
        let ws = s.join("idx-wrongstack");
        let native = s.join("idx-native");
        wrongstack_index(&root, &ws);
        copy_index(&ws, &native);
        for (round, edit) in ts_edit_rounds() {
            edit(&root);
            failures.extend(incremental_round(
                &format!("{label}/{round}"),
                &root,
                &ws,
                &native,
            ));
            if with_git {
                git(&root, &["add", "-A"]);
                git(&root, &["commit", "-q", "--allow-empty", "-m", round]);
                failures.extend(incremental_round(
                    &format!("{label}/{round}+commit"),
                    &root,
                    &ws,
                    &native,
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Source files of a checkout (no `.git`), sorted.
fn source_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_name() == ".git" {
                continue;
            }
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn appended_decl(p: &Path, n: usize) -> String {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => format!("\npub fn added_{n}() -> u32 {{ {n} }}\n"),
        "py" => format!("\n\ndef added_{n}():\n    return {n}\n"),
        "go" => format!("\nfunc Added{n}() int {{ return {n} }}\n"),
        "ts" | "tsx" | "js" | "jsx" | "mjs" => {
            format!("\nexport function added{n}() {{ return {n}; }}\n")
        }
        "java" | "cs" | "c" | "cpp" | "h" => format!("\n// added {n}\n"),
        _ => "\n".to_string(),
    }
}

fn is_marker(p: &Path) -> bool {
    matches!(
        p.file_name().and_then(|n| n.to_str()),
        Some("Cargo.toml" | "package.json" | "go.mod" | "pyproject.toml" | "__init__.py")
    )
}

/// Generic rounds for any scenario: append declarations, copy a file,
/// delete one, touch a structure marker, then a bulk edit past the rank
/// refresh threshold.
fn generic_rounds() -> Vec<EditRound> {
    vec![
        (
            "append",
            Box::new(|root: &Path| {
                for (i, p) in source_files(root)
                    .into_iter()
                    .filter(|p| !is_marker(p))
                    .take(2)
                    .enumerate()
                {
                    let mut t = std::fs::read_to_string(&p).unwrap_or_default();
                    t.push_str(&appended_decl(&p, i));
                    std::fs::write(&p, t).unwrap();
                }
            }),
        ),
        (
            "copy+delete",
            Box::new(|root: &Path| {
                let files: Vec<PathBuf> = source_files(root)
                    .into_iter()
                    .filter(|p| !is_marker(p))
                    .collect();
                let src = &files[0];
                let copy = src.with_file_name(format!(
                    "copy_{}",
                    src.file_name().unwrap().to_string_lossy()
                ));
                std::fs::copy(src, copy).unwrap();
                if files.len() > 3 {
                    std::fs::remove_file(&files[files.len() / 2]).unwrap();
                }
            }),
        ),
        (
            "marker",
            Box::new(|root: &Path| {
                if let Some(m) = source_files(root).into_iter().find(|p| is_marker(p)) {
                    let mut t = std::fs::read_to_string(&m).unwrap();
                    t.push('\n');
                    std::fs::write(&m, t).unwrap();
                }
            }),
        ),
        (
            "bulk",
            Box::new(|root: &Path| {
                for (i, p) in source_files(root).into_iter().take(30).enumerate() {
                    let mut t = std::fs::read_to_string(&p).unwrap_or_default();
                    t.push_str(&appended_decl(&p, 100 + i));
                    std::fs::write(&p, t).unwrap();
                }
            }),
        ),
    ]
}

fn incremental_scenario(
    base: &Path,
    label: &str,
    build: &dyn Fn(&Path),
    with_git: bool,
) -> Vec<String> {
    let s = base.join(label);
    let root = s.join("project");
    build(&root);
    if with_git {
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
    }
    let ws = s.join("idx-wrongstack");
    let native = s.join("idx-native");
    wrongstack_index(&root, &ws);
    copy_index(&ws, &native);
    let mut failures = Vec::new();
    for (round, edit) in generic_rounds() {
        edit(&root);
        failures.extend(incremental_round(
            &format!("{label}/{round}"),
            &root,
            &ws,
            &native,
        ));
        if with_git {
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "--allow-empty", "-m", round]);
            failures.extend(incremental_round(
                &format!("{label}/{round}+commit"),
                &root,
                &ws,
                &native,
            ));
        }
    }
    failures
}

/// WI-515 on real projects: each Git checkout in `WRONGINDEX_DIFF_PROJECTS`
/// (separated by `;`) is cloned (the original is never touched) and taken
/// through the generic edit rounds, each followed by a commit.
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE and WRONGINDEX_DIFF_PROJECTS"]
fn native_incremental_matches_wrongstack_on_listed_projects() {
    let Ok(list) = std::env::var("WRONGINDEX_DIFF_PROJECTS") else {
        eprintln!("NOT RUN: set WRONGINDEX_DIFF_PROJECTS to compare real projects");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    for (i, dir) in list.split(';').filter(|s| !s.is_empty()).enumerate() {
        let name = Path::new(dir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let clone = |root: &Path| {
            std::fs::create_dir_all(root.parent().unwrap()).unwrap();
            let ok = Command::new("git")
                .args(["clone", "-q", "--no-hardlinks", dir])
                .arg(root)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{name}: git clone failed");
        };
        let s = base.join(format!("p{i}"));
        let root = s.join("project");
        clone(&root);
        let ws = s.join("idx-wrongstack");
        let native = s.join("idx-native");
        wrongstack_index(&root, &ws);
        copy_index(&ws, &native);
        for (round, edit) in generic_rounds() {
            edit(&root);
            failures.extend(incremental_round(
                &format!("{name}/{round}"),
                &root,
                &ws,
                &native,
            ));
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "--allow-empty", "-m", round]);
            failures.extend(incremental_round(
                &format!("{name}/{round}+commit"),
                &root,
                &ws,
                &native,
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// WI-515 across languages and edit shapes (structure markers, the rank
/// refresh threshold, Git trust).
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE, bun, Python, Go and bun/node"]
fn native_incremental_matches_wrongstack_across_languages() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    let scenarios: Vec<EditRound> = vec![
        ("cargo", Box::new(scenario_cargo_workspace)),
        ("modules", Box::new(|r: &Path| scenario_many_modules(r, 40))),
        ("python", Box::new(scenario_python)),
        ("go", Box::new(scenario_go)),
        ("polyglot", Box::new(scenario_polyglot)),
        ("ts", Box::new(scenario_npm_monorepo)),
    ];
    for (label, build) in &scenarios {
        failures.extend(incremental_scenario(&base, label, build.as_ref(), false));
        failures.extend(incremental_scenario(
            &base,
            &format!("{label}-git"),
            build.as_ref(),
            true,
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// WI-512 against WrongStack itself: an enriched index (concepts, file
/// vectors, subsystems, concept edges) goes through WrongStack's runs and
/// the native ones - in place, and through a fresh native file - and the
/// databases stay identical, enrichment rows included.
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE and bun"]
fn native_keeps_enrichment_as_wrongstack_does() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    scenario_npm_monorepo(&root);
    let ws = base.join("idx-wrongstack");
    let native = base.join("idx-native");
    wrongstack_index(&root, &ws);
    {
        let c = rusqlite::Connection::open(ws.join("index.db")).unwrap();
        for sql in [
            "INSERT INTO file_concepts(file, content_hash, summary, state, model, updated_at) SELECT file, content_hash, 'purpose of ' || file, 'ready', 'm', 7 FROM files ORDER BY file LIMIT 5",
            "INSERT INTO file_vectors(file, vector, source_hash, provider) SELECT file, x'0001ff', 'h', 'p' FROM files ORDER BY file LIMIT 3",
            "INSERT INTO subsystems(id, name, summary, member_files, model, updated_at) VALUES ('core', 'Core', 's', '[]', 'm', 9)",
            "INSERT INTO concept_edges(from_id, to_id, relation) VALUES ('core', 'ui', 'uses')",
        ] {
            assert!(c.execute(sql, []).unwrap() > 0, "{sql}");
        }
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    copy_index(&ws, &native);
    let mut failures = Vec::new();
    // In place: an edit, then deleting a file that has a concept row.
    let first = source_files(&root)
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "ts") && !is_marker(p))
        .unwrap();
    let text = std::fs::read_to_string(&first).unwrap();
    std::fs::write(&first, format!("{text}\n{}", appended_decl(&first, 1))).unwrap();
    failures.extend(incremental_round("enriched/edit", &root, &ws, &native));
    std::fs::remove_file(&first).unwrap();
    failures.extend(incremental_round("enriched/delete", &root, &ws, &native));
    // A fresh native file (SAGE memories present) vs WrongStack in place.
    std::fs::create_dir_all(root.join(".wrongstack/memories")).unwrap();
    write(
        &root,
        "packages/extra/src/added.ts",
        "export const added = 1;\n",
    );
    wrongstack_index(&root, &ws);
    let n = wrongindex_engine::native::rebuild(&root, &native).unwrap();
    let outcome = serde_json::to_value(&n).unwrap();
    let v = Volatile::logical();
    let want = dump(&ws.join("index.db"), &v).unwrap();
    let got = dump(&native.join("index.db"), &v).unwrap();
    // The fresh file is the native fallback for a state WrongStack heals in
    // place (symbol ids and statistics of the rewritten tables differ by
    // design); what WI-512 needs is the enrichment layer itself.
    const ENRICHMENT: [&str; 4] = [
        "file_concepts",
        "file_vectors",
        "subsystems",
        "concept_edges",
    ];
    let only = |d: &wrongindex_engine::dbdump::Dump| {
        let mut d = d.clone();
        d.retain(|t, _| ENRICHMENT.contains(&t.as_str()) || t == "sqlite_stat1");
        if let Some(stat) = d.get_mut("sqlite_stat1") {
            stat.retain(|r| ENRICHMENT.contains(&r["tbl"].as_str().unwrap_or("")));
        }
        d
    };
    let d = diff(&only(&want), &only(&got), 8);
    eprintln!(
        "enriched/fresh: native {outcome} | {} enrichment differences",
        d.len()
    );
    failures.extend(d.into_iter().map(|l| format!("enriched/fresh: {l}")));
    let rows: i64 = rusqlite::Connection::open(native.join("index.db"))
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM file_concepts) + (SELECT COUNT(*) FROM file_vectors)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 8,
        "enrichment kept, orphans included (WrongStack prunes them in its enrichment pass)"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One targeted round: WrongStack's `runIndexer({ files })` on `ws` vs the
/// native targeted update on `native`, same list, same checkout.
fn targeted_round(
    name: &str,
    root: &Path,
    ws: &Path,
    native: &Path,
    targets: &[String],
) -> Vec<String> {
    if std::env::var_os("WRONGINDEX_TARGETED_DEBUG").is_some() {
        eprintln!("{name}: targets {targets:?}");
    }
    let r = wrongstack_index_with(root, ws, Some(targets));
    let n = wrongindex_engine::native::update_files(root, native, targets)
        .unwrap_or_else(|e| panic!("{name}: native: {e}"));
    let v = Volatile::logical();
    let want = dump(&ws.join("index.db"), &v).unwrap();
    let got = dump(&native.join("index.db"), &v).unwrap();
    let mut out: Vec<String> = diff(&want, &got, 8)
        .into_iter()
        .map(|l| format!("{name}: {l}"))
        .collect();
    let outcome = serde_json::to_value(&n).unwrap();
    eprintln!(
        "{name}: {} target(s) | WrongStack changedFiles {} | native {} | {} differences",
        targets.len(),
        r["changedFiles"],
        outcome,
        out.len()
    );
    if outcome["outcome"] != "updated" {
        out.push(format!(
            "{name}: native did not take the targeted path: {outcome}"
        ));
    } else if r["changedFiles"] != outcome["changed_files"] {
        out.push(format!(
            "{name}: changedFiles {} vs native {}",
            r["changedFiles"], outcome["changed_files"]
        ));
    }
    out
}

/// A targeted edit: mutate the checkout, return the list a watcher would
/// hand over (absolute, relative, directories, junk included).
type TargetRound = (&'static str, Box<dyn Fn(&Path, &Path) -> Vec<String>>);

fn abs(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Indexed source files (WrongStack's index of this scenario), markers
/// excluded, in file order: the files a watcher would report edits to.
fn indexed(base: &Path) -> Vec<PathBuf> {
    let c = rusqlite::Connection::open_with_flags(
        base.join("idx-wrongstack").join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let files: Vec<String> = c
        .prepare("SELECT file FROM files ORDER BY file")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    files
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| !is_marker(p) && p.exists())
        .collect()
}

fn targeted_rounds() -> Vec<TargetRound> {
    vec![
        (
            "edit",
            Box::new(|_: &Path, base: &Path| {
                let p = indexed(base).remove(0);
                let mut t = std::fs::read_to_string(&p).unwrap();
                t.push_str(&appended_decl(&p, 1));
                std::fs::write(&p, t).unwrap();
                vec![abs(&p)]
            }),
        ),
        (
            "echo",
            Box::new(|_: &Path, base: &Path| vec![abs(&indexed(base).remove(0))]),
        ),
        (
            "touch",
            Box::new(|_: &Path, base: &Path| {
                let p = indexed(base).remove(1);
                std::thread::sleep(std::time::Duration::from_millis(20));
                let t = std::fs::read(&p).unwrap();
                std::fs::write(&p, t).unwrap();
                vec![abs(&p)]
            }),
        ),
        (
            "add-relative",
            Box::new(|root: &Path, base: &Path| {
                let first = indexed(base).remove(0);
                let ext = first.extension().unwrap().to_string_lossy().into_owned();
                let p = first.with_file_name(format!("added_target.{ext}"));
                std::fs::write(&p, appended_decl(&p, 7).trim_start()).unwrap();
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                vec![rel]
            }),
        ),
        (
            "delete",
            Box::new(|_: &Path, base: &Path| {
                let files = indexed(base);
                let p = files[files.len() / 2].clone();
                std::fs::remove_file(&p).unwrap();
                vec![abs(&p)]
            }),
        ),
        (
            "rename",
            Box::new(|_: &Path, base: &Path| {
                let files = indexed(base);
                let p = files[files.len() / 3].clone();
                let to = p.with_file_name(format!(
                    "renamed_{}",
                    p.file_name().unwrap().to_string_lossy()
                ));
                std::fs::rename(&p, &to).unwrap();
                vec![abs(&p), abs(&to)]
            }),
        ),
        (
            "empty",
            Box::new(|_: &Path, base: &Path| {
                let first = indexed(base).remove(0);
                let ext = first.extension().unwrap().to_string_lossy().into_owned();
                let p = first.with_file_name(format!("empty_target.{ext}"));
                std::fs::write(&p, "").unwrap();
                vec![abs(&p)]
            }),
        ),
        (
            "dir-add",
            Box::new(|_: &Path, base: &Path| {
                let files = indexed(base);
                let dir = files[0].parent().unwrap().join("newdir");
                std::fs::create_dir_all(dir.join("inner")).unwrap();
                for (i, src) in files.iter().take(3).enumerate() {
                    let to = if i == 2 {
                        dir.join("inner").join(src.file_name().unwrap())
                    } else {
                        dir.join(format!(
                            "{i}_{}",
                            src.file_name().unwrap().to_string_lossy()
                        ))
                    };
                    std::fs::copy(src, to).unwrap();
                }
                std::fs::create_dir_all(dir.join("node_modules")).unwrap();
                std::fs::copy(
                    &files[0],
                    dir.join("node_modules").join(files[0].file_name().unwrap()),
                )
                .unwrap();
                vec![abs(&dir)]
            }),
        ),
        (
            "dir-delete",
            Box::new(|_: &Path, base: &Path| {
                let dir = indexed(base)
                    .iter()
                    .find_map(|p| {
                        p.ancestors()
                            .find(|a| a.file_name().is_some_and(|n| n == "newdir"))
                            .map(Path::to_path_buf)
                    })
                    .expect("newdir");
                std::fs::remove_dir_all(&dir).unwrap();
                vec![abs(&dir)]
            }),
        ),
        (
            "filtered",
            Box::new(|root: &Path, base: &Path| {
                let p = indexed(base).remove(0);
                let mut t = std::fs::read_to_string(&p).unwrap();
                t.push_str(&appended_decl(&p, 2));
                std::fs::write(&p, t).unwrap();
                let ext = p.extension().unwrap().to_string_lossy().into_owned();
                let dep = format!("node_modules/dep/index.{ext}");
                let out = format!("dist/out.{ext}");
                write(root, &dep, "export const dep = 1;\n");
                write(root, &out, "export const out = 1;\n");
                write(root, "notes.unknownext", "not source\n");
                write(
                    base,
                    &format!("outside.{ext}"),
                    "export const outside = 1;\n",
                );
                vec![
                    abs(&p),
                    abs(&root.join(dep)),
                    abs(&root.join(out)),
                    abs(&root.join("notes.unknownext")),
                    abs(&base.join(format!("outside.{ext}"))),
                    abs(&p),
                    abs(&root.join("does-not-exist-dir")),
                ]
            }),
        ),
        (
            "marker",
            Box::new(|root: &Path, base: &Path| {
                match source_files(root).into_iter().find(|p| is_marker(p)) {
                    Some(m) => {
                        let mut t = std::fs::read_to_string(&m).unwrap();
                        t.push('\n');
                        std::fs::write(&m, t).unwrap();
                        vec![abs(&m)]
                    }
                    None => vec![abs(&indexed(base).remove(0))],
                }
            }),
        ),
        (
            "bulk",
            Box::new(|_: &Path, base: &Path| {
                let mut out = Vec::new();
                for (i, p) in indexed(base).into_iter().take(60).enumerate() {
                    let mut t = std::fs::read_to_string(&p).unwrap_or_default();
                    t.push_str(&appended_decl(&p, 200 + i));
                    std::fs::write(&p, t).unwrap();
                    out.push(abs(&p));
                }
                out
            }),
        ),
    ]
}

/// WI-515 step 3: the watcher's targeted runs. Each scenario is indexed by
/// WrongStack, copied, and taken through targeted rounds (absolute and
/// relative paths, echoes, mtime-only touches, adds, deletes, renames, empty
/// files, directory adds and deletes, ignored/outside/unknown/duplicate
/// entries, structure markers, a bulk list past the optimize threshold),
/// walk and Git, then a whole-project run to check the two still converge.
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE, bun, Python, Go and bun/node"]
fn native_targeted_matches_wrongstack_targeted() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();
    let only = std::env::var("WRONGINDEX_TARGETED_ONLY").ok();
    let scenarios: Vec<EditRound> = vec![
        ("ts", Box::new(scenario_npm_monorepo)),
        ("modules", Box::new(|r: &Path| scenario_many_modules(r, 60))),
        ("polyglot", Box::new(scenario_polyglot)),
        ("python", Box::new(scenario_python)),
        ("cargo", Box::new(scenario_cargo_workspace)),
    ];
    for (label, build) in &scenarios {
        if only.as_deref().is_some_and(|o| o != *label) {
            continue;
        }
        for with_git in [false, true] {
            let label = format!("{label}{}", if with_git { "-git" } else { "" });
            let s = base.join(&label);
            let root = s.join("project");
            build(&root);
            if with_git {
                git(&root, &["init", "-q"]);
                git(&root, &["add", "-A"]);
                git(&root, &["commit", "-q", "-m", "init"]);
            }
            let ws = s.join("idx-wrongstack");
            let native = s.join("idx-native");
            wrongstack_index(&root, &ws);
            copy_index(&ws, &native);
            for (round, edit) in targeted_rounds() {
                let targets = edit(&root, &s);
                failures.extend(targeted_round(
                    &format!("{label}/{round}"),
                    &root,
                    &ws,
                    &native,
                    &targets,
                ));
            }
            if with_git {
                git(&root, &["add", "-A"]);
                git(&root, &["commit", "-q", "-m", "after"]);
            }
            failures.extend(incremental_round(
                &format!("{label}/whole-project"),
                &root,
                &ws,
                &native,
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
