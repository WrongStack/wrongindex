//! Write-ahead log care (indexing-pipeline §12; WI-603 read retention).
//!
//! An index runs in WAL mode. When the writer is the last connection,
//! SQLite checkpoints and removes the WAL as it closes. A reader that keeps
//! a connection open (WrongStack's query-only readers, the compatibility
//! worker's pooled stores) leaves the WAL behind at its high-water size, and
//! a reader that keeps a read transaction open makes it grow with every run:
//! frames that reader may still need cannot be overwritten.
//!
//! [`truncate`] copies the WAL into the database and cuts it to zero bytes
//! (`PRAGMA wal_checkpoint(TRUNCATE)`). It never waits: with no busy
//! handler, a reader's open transaction makes it give up at once and report
//! [`Checkpoint::Blocked`], leaving that reader undisturbed; the caller tries
//! again later. Only the index's owner may call it, outside its own writes.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// No WAL, or an empty one: nothing to do.
    Empty,
    /// The WAL (this many bytes) is in the database now and cut to zero.
    Truncated { bytes: u64 },
    /// A reader's open transaction still needs the WAL (this many bytes).
    Blocked { bytes: u64 },
}

/// Size of `index.db-wal`, asked of the file itself (a directory listing's
/// size of a file another process writes can lag on Windows).
pub fn wal_bytes(index_dir: &Path) -> u64 {
    std::fs::metadata(index_dir.join("index.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Checkpoint `index_dir/index.db` and truncate its WAL, without waiting
/// for anyone.
pub fn truncate(index_dir: &Path) -> Result<Checkpoint, rusqlite::Error> {
    let db = index_dir.join("index.db");
    let bytes = wal_bytes(index_dir);
    if bytes == 0 || !db.is_file() {
        return Ok(Checkpoint::Empty);
    }
    // No CREATE: a missing database is never made here.
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // rusqlite gives every connection a 5 s busy handler; this one never
    // waits on a reader (or anyone else).
    conn.busy_timeout(std::time::Duration::ZERO)?;
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    drop(conn);
    Ok(if busy == 0 {
        Checkpoint::Truncated { bytes }
    } else {
        Checkpoint::Blocked { bytes }
    })
}

fn side(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// What stops `db` from being replaced by another file: a `-wal` holding
/// changes not yet in `db` (they belong to it), or a connection that has it
/// open. `None` when replacing it is safe.
///
/// An empty `-wal` and a `-shm` are there while any connection has a WAL
/// database open - and also after a read-only connection closed, which may
/// not remove them (the service's own checks, `wrongindex inspect`). On
/// Windows every connection keeps the `-shm` open, so opening it without
/// sharing tells the two apart: that fails while anyone has it open. (A
/// SQLite connection opening it in that instant retries.) Elsewhere the
/// files' presence alone counts.
pub fn in_use(db: &Path) -> Option<PathBuf> {
    let (wal, shm) = (side(db, "-wal"), side(db, "-shm"));
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        return Some(wal);
    }
    let held = shm.exists() && shm_held(&shm);
    if held || (!cfg!(windows) && wal.exists()) {
        return Some(if held { shm } else { wal });
    }
    None
}

#[cfg(windows)]
fn shm_held(shm: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(shm)
    {
        Ok(_) => false,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

#[cfg(not(windows))]
fn shm_held(_: &Path) -> bool {
    true
}

/// Remove the empty `-wal` and the `-shm` that closed connections left next
/// to `db`, once [`in_use`] says nobody has it open (the next connection
/// makes new ones). An error means someone opened it meanwhile.
pub fn remove_leftovers(db: &Path) -> std::io::Result<()> {
    let wal = side(db, "-wal");
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() == 0) {
        remove_if_present(&wal)?;
    }
    remove_if_present(&side(db, "-shm"))
}

fn remove_if_present(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A read-only connection leaves an empty `-wal` and a `-shm` behind; they
    /// stop nothing once it is closed, while an open connection does.
    #[cfg(windows)]
    #[test]
    fn leftovers_of_closed_readers_do_not_count_as_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (x);")
                .unwrap();
        }
        assert_eq!(in_use(&db), None);
        {
            let ro = Connection::open_with_flags(
                &db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap();
            ro.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap();
            assert!(in_use(&db).is_some(), "open: in use");
        }
        assert!(
            side(&db, "-shm").exists(),
            "the read-only connection left it"
        );
        assert_eq!(in_use(&db), None, "closed: not in use");
        remove_leftovers(&db).unwrap();
        assert!(!side(&db, "-shm").exists() && !side(&db, "-wal").exists());

        // A log with changes in it belongs to the database.
        let w = Connection::open(&db).unwrap();
        w.execute_batch("PRAGMA wal_autocheckpoint = 0; INSERT INTO t VALUES (1);")
            .unwrap();
        assert_eq!(in_use(&db), Some(side(&db, "-wal")));
        // Held open: the -shm cannot be removed either.
        assert!(remove_leftovers(&db).is_err());
        assert!(side(&db, "-shm").exists());
        drop(w);
        assert_eq!(in_use(&db), None);
    }

    fn grow(writer: &Connection, from: i64) {
        for i in from..from + 200 {
            writer
                .execute("INSERT INTO t VALUES (?1, zeroblob(4000))", [i])
                .unwrap();
        }
    }

    #[test]
    fn truncates_unless_a_read_transaction_needs_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; CREATE TABLE t (i INTEGER, b BLOB);",
            )
            .unwrap();
        // The schema went to the WAL; nobody reads: cut, then nothing to do.
        assert!(matches!(
            truncate(dir.path()).unwrap(),
            Checkpoint::Truncated { .. }
        ));
        assert_eq!(truncate(dir.path()).unwrap(), Checkpoint::Empty);

        // A reader in a transaction: nothing waits, nothing is cut.
        let reader = Connection::open(&db).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let seen: i64 = reader
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        grow(&writer, 0);
        let before = wal_bytes(dir.path());
        assert!(before > 500_000, "{before}");
        let started = std::time::Instant::now();
        assert_eq!(
            truncate(dir.path()).unwrap(),
            Checkpoint::Blocked { bytes: before }
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(wal_bytes(dir.path()) >= before);
        // The reader keeps its snapshot.
        let still: i64 = reader
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(still, seen);

        // Transaction over, connection still open: copied and cut to zero.
        reader.execute_batch("COMMIT").unwrap();
        assert_eq!(
            truncate(dir.path()).unwrap(),
            Checkpoint::Truncated { bytes: before }
        );
        assert_eq!(wal_bytes(dir.path()), 0);
        let rows: i64 = reader
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 200);

        // No database: nothing created.
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join("index.db-wal"), b"x").unwrap();
        assert_eq!(truncate(empty.path()).unwrap(), Checkpoint::Empty);
        assert!(!empty.path().join("index.db").exists());
    }
}
