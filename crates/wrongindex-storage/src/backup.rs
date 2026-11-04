//! Consistent index backups for ownership transfer (compatibility §6.2 step 5)
//! and their restoration.
//!
//! A backup is `VACUUM INTO` a new file: a transactionally consistent copy of
//! the live database, whatever its journal mode and whoever else has it open
//! (copying only `index.db` of a WAL database is not a backup). Backups live
//! in `<indexDir>/.wrongindex-backups/`; the newest [`KEEP`] are kept.
//!
//! Restoration replaces `index.db` with a backup's bytes. It is only safe when
//! no connection has the database open, which the caller (the owner) checks;
//! here it is refused while a connection has it open or its `-wal` holds
//! changes (`wal::in_use`), and the replaced file is itself kept as a backup
//! first.

use std::path::{Path, PathBuf};

pub const BACKUP_DIR: &str = ".wrongindex-backups";
/// Backups kept per index by default (the `backupKeep` setting).
pub const KEEP: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database is in use (WAL/shared-memory files present).
    #[error("the index is in use ({0} exists); close every connection first")]
    Busy(String),
    #[error("no backup named {0}")]
    NotFound(String),
    #[error("{0} is not a valid index backup: {1}")]
    Invalid(String, String),
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Backups of `index_dir`, newest first (by name: `index-<ms>-e<epoch>.db`).
pub fn list(index_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(index_dir.join(BACKUP_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("index-") && n.ends_with(".db"))
        })
        .collect();
    out.sort();
    out.reverse();
    out
}

fn is_unreadable(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

/// Snapshot `<index_dir>/index.db`; `None` when there is no database yet.
pub fn snapshot(index_dir: &Path, epoch: i64, keep: usize) -> Result<Option<PathBuf>, BackupError> {
    let db = index_dir.join("index.db");
    if !db.is_file() {
        return Ok(None);
    }
    let dir = index_dir.join(BACKUP_DIR);
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("index-{:013}-e{epoch}.db", now_ms()));
    let tmp = dir.join(format!(
        ".tmp-{}.db",
        target.file_stem().and_then(|s| s.to_str()).unwrap_or("x")
    ));
    let _ = std::fs::remove_file(&tmp);
    let vacuum = || -> rusqlite::Result<()> {
        // Read-write without create: a read-only connection to a WAL database
        // needs its shared-memory file. VACUUM INTO never changes the source.
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])?;
        Ok(())
    };
    match vacuum() {
        Ok(()) => {}
        // Not a database SQLite can read (a maintenance claim over a damaged
        // file): nothing writes it as SQLite, so its bytes are the backup.
        Err(e) if is_unreadable(&e) => {
            let _ = std::fs::remove_file(&tmp);
            std::fs::copy(&db, &tmp)?;
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    std::fs::rename(&tmp, &target)?;
    for old in list(index_dir).into_iter().skip(keep.max(1)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(Some(target))
}

/// Replace `<index_dir>/index.db` with the backup named `name` (a file name
/// from [`list`]); the current file is backed up first. Returns
/// `(restored_from, previous_kept_as)`.
pub fn restore(
    index_dir: &Path,
    name: &str,
    epoch: i64,
) -> Result<(PathBuf, Option<PathBuf>), BackupError> {
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err(BackupError::NotFound(name.into()));
    }
    let source = index_dir.join(BACKUP_DIR).join(name);
    if !source.is_file() || !list(index_dir).contains(&source) {
        return Err(BackupError::NotFound(name.into()));
    }
    // The backup must be a readable SQLite database that passes a quick check.
    {
        let conn = rusqlite::Connection::open_with_flags(
            &source,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let ok: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if ok != "ok" {
            return Err(BackupError::Invalid(name.into(), ok));
        }
    }
    let db = index_dir.join("index.db");
    // Nobody may have it open, and its log may hold nothing (it would be
    // handed to the restored file). What closed read-only connections leave
    // behind does not count (see `wal::in_use`).
    if let Some(p) = crate::wal::in_use(&db) {
        return Err(BackupError::Busy(p.display().to_string()));
    }
    // The replaced file is kept even beyond the usual retention.
    let previous = snapshot(index_dir, epoch, usize::MAX)?;
    let staged = index_dir.join(format!(".wrongindex-restore-{}.db", now_ms()));
    std::fs::copy(&source, &staged)?;
    if let Err(e) = crate::wal::remove_leftovers(&db) {
        let _ = std::fs::remove_file(&staged);
        return Err(BackupError::Busy(format!("{} ({e})", db.display())));
    }
    if let Err(e) = std::fs::rename(&staged, &db) {
        let _ = std::fs::remove_file(&staged);
        return Err(e.into());
    }
    Ok((source, previous))
}

/// `metadata.graph_stamp` of `<index_dir>/index.db`, read-only.
pub fn graph_stamp(index_dir: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        index_dir.join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'graph_stamp'",
        [],
        |r| r.get(0),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db(dir: &Path, rows: i64) {
        let c = rusqlite::Connection::open(dir.join("index.db")).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.execute_batch("CREATE TABLE IF NOT EXISTS t(x INTEGER); DELETE FROM t;")
            .unwrap();
        for i in 0..rows {
            c.execute("INSERT INTO t VALUES (?1)", [i]).unwrap();
        }
    }

    fn count(db: &Path) -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn snapshot_is_consistent_while_open_and_keeps_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            snapshot(tmp.path(), 1, KEEP).unwrap().is_none(),
            "no database yet"
        );
        make_db(tmp.path(), 5);
        // A writer still holds the WAL database open: the copy sees its commits.
        let live = rusqlite::Connection::open(tmp.path().join("index.db")).unwrap();
        live.execute("INSERT INTO t VALUES (99)", []).unwrap();
        let b = snapshot(tmp.path(), 1, KEEP).unwrap().unwrap();
        assert_eq!(count(&b), 6);
        for e in 2..6 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            snapshot(tmp.path(), e, KEEP).unwrap();
        }
        assert_eq!(list(tmp.path()).len(), KEEP);
        drop(live);
    }

    #[test]
    fn an_unreadable_file_is_backed_up_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("index.db"), b"not a database").unwrap();
        let b = snapshot(tmp.path(), 1, KEEP).unwrap().unwrap();
        assert_eq!(std::fs::read(b).unwrap(), b"not a database");
    }

    #[test]
    fn restore_replaces_the_database_and_keeps_the_previous_one() {
        let tmp = tempfile::tempdir().unwrap();
        make_db(tmp.path(), 3);
        let b = snapshot(tmp.path(), 1, KEEP).unwrap().unwrap();
        make_db(tmp.path(), 10);
        let name = b.file_name().unwrap().to_str().unwrap().to_string();
        // Another connection open: refused.
        let open = rusqlite::Connection::open(tmp.path().join("index.db")).unwrap();
        open.pragma_update(None, "journal_mode", "WAL").unwrap();
        open.execute("INSERT INTO t VALUES (1)", []).unwrap();
        assert!(matches!(
            restore(tmp.path(), &name, 2),
            Err(BackupError::Busy(_))
        ));
        drop(open);
        let (from, previous) = restore(tmp.path(), &name, 2).unwrap();
        assert_eq!(from, b);
        assert_eq!(count(&tmp.path().join("index.db")), 3);
        assert_eq!(count(&previous.unwrap()), 11);
        assert!(matches!(
            restore(tmp.path(), "../index.db", 2),
            Err(BackupError::NotFound(_))
        ));
    }
}
