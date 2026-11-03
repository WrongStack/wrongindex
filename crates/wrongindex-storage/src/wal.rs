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

use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

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
