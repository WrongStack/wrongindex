//! Content verification (indexing-pipeline §10, the deeper verification):
//! every file the index has rows for is read and hashed, and the hash
//! compared with the one stored when it was indexed.
//!
//! Nothing cheaper is trusted. A whole-project run skips a file Git reports
//! clean whose stored stamp matches, and Git decides "clean" from the file's
//! size and timestamps: a file rewritten with the same size and its old
//! timestamp (a restore, a sync tool, a build step that keeps times) looks
//! unchanged to both. Here only the bytes count.
//!
//! Read-only: the index is opened with `query_only`; files are only read.
//! Only regular files under the checkout root are read - never a link, a
//! device or a path outside the checkout, whatever the index names.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

/// Paths named in a result; the full lists stay with the caller.
const SAMPLE: usize = 10;

/// What a verification found.
#[derive(Debug, Default)]
pub struct Contents {
    /// Files read and compared.
    pub checked: usize,
    /// Indexed files whose bytes are no longer the ones indexed.
    pub changed: Vec<String>,
    /// Indexed files that are gone.
    pub missing: Vec<String>,
    /// Not compared: larger than the indexer reads, not a regular file, a
    /// link, outside the checkout, or unreadable.
    pub skipped: usize,
    pub duration_ms: u64,
}

/// The summary a job result carries.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub checked: usize,
    pub changed: usize,
    pub missing: usize,
    pub skipped: usize,
    pub duration_ms: u64,
    /// Up to ten of the files that differ, changed ones first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sample: Vec<String>,
}

impl Contents {
    /// Every file that differs from the index, changed ones first.
    pub fn differing(&self) -> impl Iterator<Item = &String> {
        self.changed.iter().chain(&self.missing)
    }

    pub fn agrees(&self) -> bool {
        self.changed.is_empty() && self.missing.is_empty()
    }

    pub fn summary(&self) -> Summary {
        Summary {
            checked: self.checked,
            changed: self.changed.len(),
            missing: self.missing.len(),
            skipped: self.skipped,
            duration_ms: self.duration_ms,
            sample: self.differing().take(SAMPLE).cloned().collect(),
        }
    }
}

/// Compare every indexed file of `db` under `checkout_root` with what was
/// indexed. Files larger than `max_bytes` are not read (the indexer does not
/// read them either).
pub fn verify(db: &Path, checkout_root: &Path, max_bytes: u64) -> rusqlite::Result<Contents> {
    let started = Instant::now();
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "query_only", true)?;
    let stored: Vec<(String, String)> = conn
        .prepare("SELECT file, content_hash FROM files WHERE content_hash IS NOT NULL AND content_hash != '' ORDER BY file")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(conn);

    let root = key(&crate::wrongstack_paths::node_resolve(checkout_root));
    let mut out = Contents::default();
    for (file, hash) in stored {
        let path = PathBuf::from(&file);
        if !under(&key(&crate::wrongstack_paths::node_resolve(&path)), &root) {
            out.skipped += 1;
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.missing.push(file),
            Ok(m) if m.is_file() && m.len() <= max_bytes => match std::fs::read(&path) {
                Ok(bytes) => {
                    out.checked += 1;
                    if crate::file_content_hash(&bytes) != hash {
                        out.changed.push(file);
                    }
                }
                Err(_) => out.skipped += 1,
            },
            _ => out.skipped += 1,
        }
    }
    out.duration_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

/// A path as compared for containment (case-insensitive on Windows).
fn key(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.replace('/', "\\").to_lowercase()
    } else {
        s.into_owned()
    }
}

fn under(path: &str, root: &str) -> bool {
    let sep = std::path::MAIN_SEPARATOR;
    path.strip_prefix(root)
        .is_some_and(|rest| rest.starts_with(sep) || root.ends_with(sep))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(dir: &Path, files: &[(&Path, &str)]) -> PathBuf {
        let db = dir.join("index.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE files(file TEXT PRIMARY KEY, content_hash TEXT)")
            .unwrap();
        for (f, h) in files {
            conn.execute(
                "INSERT INTO files VALUES (?1, ?2)",
                (f.to_string_lossy().into_owned(), h),
            )
            .unwrap();
        }
        db
    }

    fn hash(s: &str) -> String {
        crate::file_content_hash(s.as_bytes())
    }

    #[test]
    fn only_the_bytes_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("checkout");
        std::fs::create_dir_all(&root).unwrap();
        let same = root.join("same.ts");
        let rewritten = root.join("rewritten.ts");
        let gone = root.join("gone.ts");
        let big = root.join("big.ts");
        std::fs::write(&same, "export const a = 1;\n").unwrap();
        std::fs::write(&rewritten, "export const b = 1;\n").unwrap();
        std::fs::write(&big, "x".repeat(100)).unwrap();
        let outside = tmp.path().join("outside.ts");
        std::fs::write(&outside, "secret").unwrap();
        let db = index(
            tmp.path(),
            &[
                (&same, &hash("export const a = 1;\n")),
                (&rewritten, &hash("export const b = 1;\n")),
                (&gone, &hash("whatever")),
                (&big, &hash("y")),
                (&outside, &hash("not this")),
            ],
        );
        // The same size and the old timestamp: only the bytes changed.
        let before = std::fs::metadata(&rewritten).unwrap().modified().unwrap();
        std::fs::write(&rewritten, "export const b = 2;\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&rewritten)
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert_eq!(
            std::fs::metadata(&rewritten).unwrap().modified().unwrap(),
            before
        );

        let c = verify(&db, &root, 50).unwrap();
        assert_eq!(c.checked, 2, "{c:?}");
        assert_eq!(c.changed, vec![rewritten.to_string_lossy().into_owned()]);
        assert_eq!(c.missing, vec![gone.to_string_lossy().into_owned()]);
        // Too large to index, and outside the checkout: never read.
        assert_eq!(c.skipped, 2, "{c:?}");
        assert!(!c.agrees());
        let s = serde_json::to_value(c.summary()).unwrap();
        assert_eq!(s["changed"], 1);
        assert_eq!(s["missing"], 1);
        assert_eq!(s["sample"].as_array().unwrap().len(), 2);
        assert_eq!(c.differing().count(), 2);

        std::fs::write(&rewritten, "export const b = 1;\n").unwrap();
        std::fs::write(&gone, "whatever").unwrap();
        let c = verify(&db, &root, 50).unwrap();
        assert!(c.agrees(), "{c:?}");
        assert_eq!(c.checked, 3, "{c:?}");
        let s = serde_json::to_value(c.summary()).unwrap();
        assert!(s.get("sample").is_none(), "{s}");
    }

    #[test]
    fn containment_is_by_whole_components() {
        let sep = std::path::MAIN_SEPARATOR;
        let root = format!("{sep}work{sep}demo");
        assert!(under(&format!("{root}{sep}src{sep}a.ts"), &root));
        assert!(!under(&format!("{root}-other{sep}a.ts"), &root));
        assert!(!under(&root, &root));
    }
}
