//! WI-502 gate: the native writer's `index.db` for a JSON/YAML-only fixture is
//! logically identical to the one WrongStack's own indexer wrote (every table,
//! FTS content by rowid, `sqlite_stat1`), apart from per-run values (times,
//! random epoch) and the absolute project root.

use std::path::{Path, PathBuf};

use wrongindex_engine::dbdump::{Volatile, diff, dump};
use wrongindex_engine::native::{NativeError, index_fresh};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Long-name absolute path (CI temp dirs use 8.3 short names).
fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

fn oracle_root(fixture: &str) -> (String, String) {
    (
        format!(r"C:\wrongindex-oracle\{fixture}\project"),
        format!(r"C:\wrongindex-oracle\{fixture}\index"),
    )
}

#[test]
fn config_only_matches_the_wrongstack_oracle() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    let index = base.join("index");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);

    let report = index_fresh(&root, &index).unwrap();
    assert_eq!(
        (
            report.files_indexed,
            report.files_empty,
            report.symbols_indexed
        ),
        (2, 1, 10)
    );

    let (oroot, oindex) = oracle_root("config-only");
    let mut want_v = Volatile::logical();
    want_v.rewrite = vec![(oroot, "<root>".into()), (oindex, "<indexDir>".into())];
    let mut got_v = Volatile::logical();
    got_v.rewrite = vec![
        (root.display().to_string(), "<root>".into()),
        (index.display().to_string(), "<indexDir>".into()),
    ];
    let want = dump(
        &repo().join("tests/fixtures/oracle/config-only/index.db"),
        &want_v,
    )
    .unwrap();
    let got = dump(&index.join("index.db"), &got_v).unwrap();
    let d = diff(&want, &got, 10);
    assert!(
        d.is_empty(),
        "native index differs from the oracle:\n{}",
        d.join("\n")
    );

    // Same physical page count as WrongStack's file (no STAT4, no free pages).
    let pages = |p: &Path| {
        let c =
            rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let n: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
        let f: i64 = c
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        (n, f)
    };
    assert_eq!(
        pages(&index.join("index.db")),
        pages(&repo().join("tests/fixtures/oracle/config-only/index.db"))
    );

    // The profile inspector accepts it as a WrongStack v5 index.
    let ins = wrongindex_storage::inspect_index(&index.join("index.db"));
    assert_eq!(
        serde_json::to_value(ins.verdict).unwrap(),
        "compatible",
        "{ins:?}"
    );
}

#[test]
fn unsupported_projects_and_existing_indexes_are_refused_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let ts_root = base.join("ts");
    copy_dir(&repo().join("tests/fixtures/projects/basic-ts"), &ts_root);
    let index = base.join("idx-ts");
    let err = index_fresh(&ts_root, &index).unwrap_err();
    assert!(matches!(err, NativeError::Unsupported(_)), "{err}");
    assert!(
        !index.exists(),
        "nothing may be created for an unsupported project"
    );

    let root = base.join("cfg");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("idx");
    index_fresh(&root, &index).unwrap();
    let before = std::fs::read(index.join("index.db")).unwrap();
    let err = index_fresh(&root, &index).unwrap_err();
    assert!(matches!(err, NativeError::Unsupported(_)), "{err}");
    assert_eq!(std::fs::read(index.join("index.db")).unwrap(), before);

    // A Kubernetes-looking YAML could carry infra refs: refused.
    let k8s = base.join("k8s-like");
    std::fs::create_dir_all(&k8s).unwrap();
    std::fs::write(k8s.join("deploy.yaml"), "apiVersion: v1\nkind: Pod\n").unwrap();
    let err = index_fresh(&k8s, &base.join("idx-k8s")).unwrap_err();
    assert!(matches!(err, NativeError::Unsupported(_)), "{err}");
}
