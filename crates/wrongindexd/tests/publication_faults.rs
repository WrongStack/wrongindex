//! Fault injection at the native publication boundary (WI-601).
//!
//! A native run writes a complete index in a private staging directory and
//! then renames it over `index.db`. A run that dies just before or just after
//! the rename (here: `abort()` through the debug-only
//! `WRONGINDEX_NATIVE_FAULT` hook, in the same child process the daemon uses)
//! must leave either the previous or the new complete index - never a partial
//! one - and the next run must clean up and converge.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use wrongindex_storage::inspect::Verdict;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
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

/// One `wrongindexd --native-rebuild` child; `None` when it did not exit
/// normally (the injected abort).
fn rebuild(root: &Path, index: &Path, fault: Option<&str>) -> Option<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wrongindexd"));
    cmd.arg("--native-rebuild").arg(root).arg(index);
    match fault {
        Some(f) => cmd.env("WRONGINDEX_NATIVE_FAULT", f),
        None => cmd.env_remove("WRONGINDEX_NATIVE_FAULT"),
    };
    let out = cmd.output().unwrap();
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn staging_dirs(index: &Path) -> usize {
    std::fs::read_dir(index)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".wrongindex-staging-")
        })
        .count()
}

fn files_table(index: &Path) -> Vec<(String, String)> {
    let c = rusqlite::Connection::open_with_flags(
        index.join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut rows: Vec<(String, String)> = c
        .prepare("SELECT file, content_hash FROM files")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.sort();
    rows
}

fn assert_complete(index: &Path) {
    let i = wrongindex_storage::inspect_index(&index.join("index.db"));
    assert_eq!(i.verdict, Verdict::Compatible, "{i:?}");
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the fault hook exists in debug builds only"
)]
fn a_run_dying_at_the_publication_boundary_never_leaves_a_partial_index() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let index = tmp.path().join("index");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    std::fs::create_dir_all(&index).unwrap();

    // First write (fresh, staging + rename). Die just before the rename:
    // no index at all, only the dead run's staging directory.
    assert!(rebuild(&root, &index, Some("before-publish")).is_none());
    assert!(!index.join("index.db").exists());
    assert_eq!(staging_dirs(&index), 1, "the dead run's staging directory");
    // Die just after the rename: the index is complete and published.
    assert!(rebuild(&root, &index, Some("after-publish")).is_none());
    assert_complete(&index);
    let before = files_table(&index);
    let next = rebuild(&root, &index, None).expect("recovery run");
    assert_eq!(next["result"]["outcome"], "unchanged", "{next}");
    assert_eq!(staging_dirs(&index), 0, "the next run sweeps leftovers");

    // Later runs update in place (WI-515); the commit is the publication.
    // Die just before it: the previous generation stays, complete.
    std::fs::write(
        root.join("extra.json"),
        "{\"added\": true}
",
    )
    .unwrap();
    assert!(rebuild(&root, &index, Some("before-commit")).is_none());
    assert_complete(&index);
    assert_eq!(files_table(&index), before);
    // Die just after it: the new generation is complete and published.
    assert!(rebuild(&root, &index, Some("after-commit")).is_none());
    assert_complete(&index);
    let after = files_table(&index);
    assert_ne!(after, before);
    assert!(
        after.iter().any(|(f, _)| f.ends_with("extra.json")),
        "{after:?}"
    );
    // The next run converges (no-op: same files).
    let next = rebuild(&root, &index, None).expect("recovery run");
    assert_eq!(next["result"]["outcome"], "unchanged", "{next}");
    assert_eq!(files_table(&index), after);

    // And a real change after the faults is written normally.
    std::fs::remove_file(root.join("extra.json")).unwrap();
    let last = rebuild(&root, &index, None).expect("write after recovery");
    assert_eq!(last["result"]["outcome"], "updated", "{last}");
    assert_eq!(files_table(&index), before);
    assert_eq!(staging_dirs(&index), 0);
    assert_complete(&index);
}

/// A maintenance rebuild dying between moving the old file aside and
/// publishing the new one leaves no `index.db` and the backup; the next run
/// writes a fresh index and the backup is still there.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the fault hook exists in debug builds only"
)]
fn a_maintenance_rebuild_dying_between_its_renames_keeps_the_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let index = tmp.path().join("index");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    std::fs::create_dir_all(&index).unwrap();
    let garbage = b"not an index".to_vec();
    std::fs::write(index.join("index.db"), &garbage).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--native-rebuild-replace")
        .arg(&root)
        .arg(&index)
        .env("WRONGINDEX_NATIVE_FAULT", "between-backup-and-publish")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the fault must abort the run");
    assert!(!index.join("index.db").exists());
    let backups: Vec<PathBuf> = std::fs::read_dir(&index)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("index.db.wrongindex-backup-")
        })
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(std::fs::read(&backups[0]).unwrap(), garbage);

    let next = rebuild(&root, &index, None).expect("recovery run");
    assert_eq!(next["result"]["outcome"], "written", "{next}");
    assert_complete(&index);
    assert_eq!(std::fs::read(&backups[0]).unwrap(), garbage);
    assert_eq!(staging_dirs(&index), 0);
}
