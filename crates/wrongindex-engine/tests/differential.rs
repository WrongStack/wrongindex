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

/// Real-world projects: `WRONGINDEX_DIFF_PROJECTS` lists directories
/// (separated by `;`) that are copied and indexed both ways.
#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE and WRONGINDEX_DIFF_PROJECTS"]
fn native_matches_wrongstack_on_listed_projects() {
    let list = std::env::var("WRONGINDEX_DIFF_PROJECTS").expect("WRONGINDEX_DIFF_PROJECTS");
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
