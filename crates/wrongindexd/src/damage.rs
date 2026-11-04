//! A damaged index (architecture §9 "Corruption": stop writes, retain
//! diagnostic evidence, offer a verified rebuild, never silently erase).
//!
//! SQLite reporting an index damaged (`SQLITE_CORRUPT`, `SQLITE_NOTADB`) in
//! a run, a read or a check marks the project: the watcher stops sending it
//! work (no retries on a timer, nothing counted toward parking), and write
//! jobs are refused before anything opens the file for writing - including
//! the compatibility worker, which would otherwise write into it. The file
//! itself is never touched: it stays as it is until the user rebuilds the
//! index (a maintenance rebuild keeps it as `index.db.wrongindex-backup-<ms>`
//! and checks the new one) or restores a backup. A write job finding the file
//! sound again (`PRAGMA quick_check`; say it was replaced by hand), a check
//! that passes, or a rebuild or restore that succeeds ends the stop.

use std::path::Path;

use wrongindex_contracts::{ErrorBody, ErrorCode};

/// What the user can do, in every message about a damaged index.
pub const REMEDY: &str = "writes to it are stopped and the file is kept as it is. Rebuild the index (the damaged file is kept as a backup) or restore a backup";

/// The compatibility worker reports SQLite's errors as text.
pub fn says_corrupt(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("database disk image is malformed")
        || m.contains("file is not a database")
        || m.contains("sqlite_corrupt")
        || m.contains("sqlite_notadb")
}

/// `DATABASE_CORRUPT` with the remedy, for what SQLite said.
pub fn corrupt(message: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::DatabaseCorrupt, format!("{message}; {REMEDY}"))
}

/// `DATABASE_CORRUPT` for an error whose message says the index is damaged.
pub fn classify(e: ErrorBody) -> ErrorBody {
    if e.code != ErrorCode::DatabaseCorrupt && says_corrupt(&e.message) {
        corrupt(&e.message)
    } else {
        e
    }
}

/// Whether the index at `index_dir` is damaged after all: SQLite's own check
/// of every page and of the full-text index. `None` when the check could not
/// run (the file is locked, an I/O error): nothing learned.
pub fn still_damaged(index_dir: &Path) -> Option<(bool, String)> {
    let db = index_dir.join("index.db");
    if !db.exists() {
        return Some((false, String::new()));
    }
    let i = wrongindex_storage::integrity::quick_check(&db).ok()?;
    Some((!i.ok, i.summary()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_text_is_classified() {
        let e = classify(ErrorBody::new(
            ErrorCode::Internal,
            "SqliteError: database disk image is malformed",
        ));
        assert_eq!(e.code, ErrorCode::DatabaseCorrupt);
        assert!(e.message.contains("Rebuild the index"), "{}", e.message);
        assert!(!e.retryable);
        let other = classify(ErrorBody::new(ErrorCode::Internal, "parse failed"));
        assert_eq!(other.code, ErrorCode::Internal);
        assert!(says_corrupt("Error: file is not a database"));
        // Not doubled.
        let again = classify(e.clone());
        assert_eq!(again, e);
    }

    #[test]
    fn damage_is_looked_at_again() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(still_damaged(dir.path()), Some((false, String::new())));
        std::fs::write(dir.path().join("index.db"), vec![b'x'; 8192]).unwrap();
        let (bad, why) = still_damaged(dir.path()).unwrap();
        assert!(bad);
        assert!(why.contains("not a database"), "{why}");
        std::fs::remove_file(dir.path().join("index.db")).unwrap();
        let c = rusqlite::Connection::open(dir.path().join("index.db")).unwrap();
        c.execute_batch("CREATE TABLE t (x)").unwrap();
        drop(c);
        assert_eq!(still_damaged(dir.path()).map(|d| d.0), Some(false));
    }
}
