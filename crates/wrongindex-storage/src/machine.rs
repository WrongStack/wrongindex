//! The WrongStack indexes on this machine, found without changing anything.
//!
//! WrongStack keeps each checkout's index at
//! `<home>/projects/<slug>/codebase-index/index.db`, and a linked worktree's at
//! `<home>/projects/<slug>/codebase-index-worktrees/<wt>/index.db`
//! ([`crate::wrongstack_paths`]). Most of them are written by WrongStack's own
//! engine, never by WrongIndex; this module lists them all so a user can see
//! which projects are indexed, how large and how recent each index is, and
//! what is writing it now.
//!
//! Strictly read-only. Every database is opened with SQLite's `immutable=1`:
//! a WAL database opened even read-only leaves `-wal` and `-shm` files behind
//! ([`crate::wal`]), which would change WrongStack's directories. The price is
//! that rows still only in a write-ahead log are not counted (sizes and times
//! come from the file system and include the log). `server.json`, which
//! WrongStack's index server writes, carries an authentication token: only
//! the process id, project root, start time and runtime version are read
//! from it; the token is never deserialized.

use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::owner_lock;

/// More indexes than this are not listed (the scan says it stopped).
pub const MAX_INDEXES: usize = 5000;
const WORKTREES_DIR: &str = "codebase-index-worktrees";
const INDEX_DIR: &str = "codebase-index";
const SERVER_FILE: &str = "server.json";
const META_FILE: &str = "meta.json";
/// Programs WrongStack's index server runs as: Bun or Node.js, or a
/// WrongStack build compiled with Bun (`wrongstack-engine.exe`).
const WRONGSTACK_PROGRAMS: &[&str] = &[
    "bun.exe",
    "node.exe",
    "wrongstack.exe",
    "wrongstack-engine.exe",
    "bun",
    "node",
    "wrongstack",
    "wrongstack-engine",
];

/// Where a checkout path came from: WrongStack's project record, its index
/// server's record, or the indexed files' common folder (approximate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckoutSource {
    Meta,
    Server,
    Files,
}

/// A process recorded as working on an index, and whether it still runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedProcess {
    pub pid: u32,
    /// The recorded process id runs a program of the expected kind now.
    pub running: bool,
    /// Its program file name, when it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// When it started or took the index (ms since the epoch), as recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    /// WrongStack's runtime version, for an index server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineIndex {
    pub index_dir: String,
    /// WrongStack's project folder name (`<slug>-<hash>`).
    pub project_dir: String,
    /// The worktree folder name, for a linked worktree's index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    pub checkout_root: Option<String>,
    pub checkout_source: Option<CheckoutSource>,
    /// Whether the checkout folder exists now (`None` when it is not known).
    pub checkout_exists: Option<bool>,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    /// The latest modification of `index.db` or its log (ms since the epoch).
    pub last_write_ms: Option<i64>,
    /// `metadata.last_indexed`: the last run that changed something.
    pub last_indexed_ms: Option<i64>,
    pub files: Option<i64>,
    pub symbols: Option<i64>,
    pub schema_version: Option<String>,
    /// Why the database could not be read (it is listed anyway).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    /// WrongStack's own index server for this index (`server.json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrongstack_server: Option<RecordedProcess>,
    /// The WrongIndex service recorded as owning it (`.wrongindex-owner.json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrongindex_owner: Option<RecordedProcess>,
    /// Programs that have the index's files open now (Windows Restart
    /// Manager; names and process ids only). Asked only for an index with a
    /// write-ahead log or written in the last [`RECENT_MS`]: a program with
    /// a WrongStack index open keeps a log beside it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub open_by: Vec<crate::holders::Holder>,
}

/// Indexes written this recently are asked who has them open.
pub const RECENT_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineScan {
    pub wrongstack_home: String,
    /// Project folders looked at.
    pub project_dirs: usize,
    pub indexes: Vec<MachineIndex>,
    /// More than [`MAX_INDEXES`] were found; the rest are not listed.
    pub truncated: bool,
    pub total_bytes: u64,
    pub took_ms: u64,
}

/// Every WrongStack index under `home` (`~/.wrongstack` or `WRONGSTACK_HOME`),
/// most recently written first. A missing `projects` folder is an empty list.
pub fn scan(home: &Path) -> MachineScan {
    let started = Instant::now();
    let mut found: Vec<(PathBuf, String, Option<String>)> = Vec::new();
    let mut project_dirs = 0;
    let mut truncated = false;
    let projects = home.join("projects");
    if let Ok(rd) = std::fs::read_dir(&projects) {
        'outer: for entry in rd.flatten() {
            // Links are not followed: a project folder is a plain directory.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            project_dirs += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            if is_file(&dir.join(INDEX_DIR).join("index.db")) {
                found.push((dir.join(INDEX_DIR), name.clone(), None));
            }
            if let Ok(wts) = std::fs::read_dir(dir.join(WORKTREES_DIR)) {
                for wt in wts.flatten() {
                    if wt.file_type().is_ok_and(|t| t.is_dir())
                        && is_file(&wt.path().join("index.db"))
                    {
                        found.push((
                            wt.path(),
                            name.clone(),
                            Some(wt.file_name().to_string_lossy().into_owned()),
                        ));
                    }
                }
            }
            if found.len() > MAX_INDEXES {
                truncated = true;
                found.truncate(MAX_INDEXES);
                break 'outer;
            }
        }
    }
    // Asking Windows which programs have a file open walks every handle on
    // the machine (about a second each here), so indexes are described a
    // few at a time.
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let chunk = found.len().div_ceil(threads).max(1);
    let mut indexes: Vec<MachineIndex> = std::thread::scope(|s| {
        let handles: Vec<_> = found
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|(dir, project_dir, worktree)| {
                            let meta = if worktree.is_none() {
                                read_meta_root(&dir.parent().unwrap_or(dir).join(META_FILE))
                            } else {
                                None
                            };
                            describe(dir, project_dir.clone(), worktree.clone(), meta)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    indexes.sort_by(|a, b| {
        b.last_write_ms
            .cmp(&a.last_write_ms)
            .then_with(|| a.index_dir.cmp(&b.index_dir))
    });
    MachineScan {
        wrongstack_home: home.display().to_string(),
        project_dirs,
        total_bytes: indexes.iter().map(|i| i.db_bytes + i.wal_bytes).sum(),
        indexes,
        truncated,
        took_ms: started.elapsed().as_millis() as u64,
    }
}

fn is_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

fn modified_ms(m: &std::fs::Metadata) -> Option<i64> {
    m.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// `meta.json` of a WrongStack project folder: `{ "root": "<checkout>" }`.
fn read_meta_root(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Meta {
        root: Option<String>,
    }
    let bytes = std::fs::read(path).ok()?;
    let m: Meta = serde_json::from_slice(&bytes).ok()?;
    m.root.filter(|r| !r.trim().is_empty())
}

/// The fields of WrongStack's `server.json` this module reads. Anything else
/// in the file (its `authToken` above all) is skipped by the parser and never
/// held in memory as a value.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerRecord {
    pid: Option<u32>,
    project_root: Option<String>,
    started_at: Option<String>,
    runtime_version: Option<String>,
}

fn describe(
    dir: &Path,
    project_dir: String,
    worktree: Option<String>,
    meta_root: Option<String>,
) -> MachineIndex {
    let db = dir.join("index.db");
    let wal = dir.join("index.db-wal");
    let db_meta = std::fs::metadata(&db).ok();
    let wal_meta = std::fs::metadata(&wal).ok();
    let last_write_ms = [db_meta.as_ref(), wal_meta.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(modified_ms)
        .max();
    let server: Option<ServerRecord> = std::fs::read(dir.join(SERVER_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let read = read_db(&db);
    let (mut checkout_root, mut checkout_source) = (None, None);
    if let Some(r) = meta_root {
        checkout_root = Some(r);
        checkout_source = Some(CheckoutSource::Meta);
    } else if let Some(r) = server
        .as_ref()
        .and_then(|s| s.project_root.clone())
        .filter(|r| !r.trim().is_empty())
    {
        checkout_root = Some(r);
        checkout_source = Some(CheckoutSource::Server);
    } else if let Ok(r) = &read
        && let Some(prefix) = &r.common_folder
    {
        checkout_root = Some(prefix.clone());
        checkout_source = Some(CheckoutSource::Files);
    }
    let checkout_exists = checkout_root.as_deref().map(|r| Path::new(r).is_dir());
    let now_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    // A recorded process id counts as still working on the index when it
    // runs a program of the expected kind (below: or has the index open).
    let recorded = |pid: u32, names: &[&str]| -> (bool, Option<String>) {
        let program = process_program(pid);
        let running = program.as_deref().is_some_and(|p| is_one_of(p, names));
        (running, program)
    };
    let mut wrongstack_server = server.and_then(|s| {
        let pid = s.pid?;
        let (running, program) = recorded(pid, WRONGSTACK_PROGRAMS);
        Some((
            RecordedProcess {
                pid,
                running,
                program: program.clone().filter(|_| running),
                since_ms: s.started_at.as_deref().and_then(parse_iso_ms),
                version: s.runtime_version,
            },
            program,
        ))
    });
    let mut wrongindex_owner = owner_lock::read_owner(dir).map(|o| {
        let (running, program) = recorded(o.pid, &["wrongindexd.exe", "wrongindexd"]);
        (
            RecordedProcess {
                pid: o.pid,
                running,
                program: program.clone().filter(|_| running),
                since_ms: Some(o.acquired_at_ms),
                version: None,
            },
            program,
        )
    });
    // Who has the index open is asked only when no recorded writer runs and
    // the index looks in use (a log beside it, or a recent write): the
    // question walks every handle on the machine.
    let writer_known = wrongstack_server.as_ref().is_some_and(|(p, _)| p.running)
        || wrongindex_owner.as_ref().is_some_and(|(p, _)| p.running);
    let in_use = wal_meta.is_some() || last_write_ms.is_some_and(|t| now_ms - t < RECENT_MS);
    let open_by = if !writer_known && in_use {
        crate::holders::of(dir)
    } else {
        Vec::new()
    };
    // A recorded process that has the index open is working on it, whatever
    // its program is called; a process id reused by another program has not.
    for (p, program) in wrongstack_server
        .iter_mut()
        .chain(wrongindex_owner.iter_mut())
    {
        if !p.running && program.is_some() && open_by.iter().any(|h| h.pid == p.pid) {
            p.running = true;
            p.program = program.clone();
        }
    }
    let wrongstack_server = wrongstack_server.map(|(p, _)| p);
    let wrongindex_owner = wrongindex_owner.map(|(p, _)| p);
    let (files, symbols, schema_version, last_indexed_ms, read_error) = match read {
        Ok(r) => (
            r.files,
            r.symbols,
            r.schema_version,
            r.last_indexed_ms,
            None,
        ),
        Err(e) => (None, None, None, None, Some(e)),
    };
    MachineIndex {
        index_dir: dir.display().to_string(),
        project_dir,
        worktree,
        checkout_root,
        checkout_source,
        checkout_exists,
        db_bytes: db_meta.map_or(0, |m| m.len()),
        wal_bytes: wal_meta.map_or(0, |m| m.len()),
        last_write_ms,
        last_indexed_ms,
        files,
        symbols,
        schema_version,
        read_error,
        wrongstack_server,
        wrongindex_owner,
        open_by,
    }
}

/// Whether `program` (a full path or a name) has one of these file names.
fn is_one_of(program: &str, names: &[&str]) -> bool {
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    names.iter().any(|n| name.eq_ignore_ascii_case(n))
}

struct DbFacts {
    files: Option<i64>,
    symbols: Option<i64>,
    schema_version: Option<String>,
    last_indexed_ms: Option<i64>,
    common_folder: Option<String>,
}

/// `file:` URI for SQLite with every byte outside the unreserved set and
/// `/:` percent-encoded (a `?` or `#` in a folder name stays part of the path).
fn file_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:");
    if !s.starts_with('/') {
        out.push_str("///");
    }
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.push_str("?immutable=1");
    out
}

/// Read a few facts from `index.db` without creating or changing any file.
fn read_db(db: &Path) -> Result<DbFacts, String> {
    let conn = Connection::open_with_flags(
        file_uri(db),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|e| e.to_string())?;
    let meta = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    };
    let schema_version = meta("version");
    let last_indexed_ms = meta("last_indexed").and_then(|v| v.trim().parse().ok());
    let (files, symbols): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT count(*), coalesce(sum(symbol_count), 0) FROM files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())
        .map(|(a, b): (i64, i64)| (Some(a), Some(b)))?;
    let (first, last): (Option<String>, Option<String>) = conn
        .query_row("SELECT min(file), max(file) FROM files", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap_or((None, None));
    let common_folder = match (first, last) {
        (Some(a), Some(b)) => common_folder(&a, &b),
        _ => None,
    };
    Ok(DbFacts {
        files,
        symbols,
        schema_version,
        last_indexed_ms,
        common_folder,
    })
}

/// The deepest folder both absolute file paths lie in.
fn common_folder(a: &str, b: &str) -> Option<String> {
    let parts =
        |s: &str| -> Vec<String> { s.split(['\\', '/']).map(str::to_string).collect::<Vec<_>>() };
    let (pa, pb) = (parts(a), parts(b));
    // The last part of each is a file name, never a shared folder.
    let n = pa.len().min(pb.len()).saturating_sub(1);
    let mut shared = 0;
    while shared < n && pa[shared].eq_ignore_ascii_case(&pb[shared]) {
        shared += 1;
    }
    if shared == 0 || (shared == 1 && pa[0].is_empty()) {
        return None;
    }
    let sep = if a.contains('\\') { "\\" } else { "/" };
    let mut s = pa[..shared].join(sep);
    if s.ends_with(':') {
        s.push_str(sep);
    }
    Some(s)
}

/// `2026-11-06T23:18:55.701Z` (what JavaScript's `toISOString` writes) to ms.
fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let ms = match s.get(19..20) {
        Some(".") => s[20..s.len() - 1]
            .get(..3)
            .and_then(|f| f.parse::<i64>().ok())
            .unwrap_or(0),
        _ => 0,
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}

/// The program a running process executes, or `None` when the process id is
/// not running (or belongs to another account and cannot be asked).
#[cfg(windows)]
pub fn process_program(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    if pid == 0 {
        return None;
    }
    // SAFETY: a handle this function owns and closes; the buffers outlive
    // the calls that fill them.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let mut buf = [0u16; 32_768];
        let mut len = buf.len() as u32;
        let named =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        (alive && named).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(not(windows))]
pub fn process_program(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_index(dir: &Path, files: &[(&str, i64)]) {
        std::fs::create_dir_all(dir).unwrap();
        let c = Connection::open(dir.join("index.db")).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE files (file TEXT PRIMARY KEY, lang TEXT, mtime_ms INTEGER,
               content_hash TEXT, symbol_count INTEGER, last_indexed INTEGER,
               package TEXT, git_blob TEXT);
             INSERT INTO metadata VALUES ('version', '5'), ('last_indexed', '1794006770919');",
        )
        .unwrap();
        for (f, n) in files {
            c.execute(
                "INSERT INTO files (file, symbol_count) VALUES (?1, ?2)",
                rusqlite::params![f, n],
            )
            .unwrap();
        }
        // Fold the log in and close: no -wal/-shm remain.
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        drop(c);
        for leftover in ["index.db-wal", "index.db-shm"] {
            let _ = std::fs::remove_file(dir.join(leftover));
        }
    }

    /// Every file under `root` with its size and bytes' hash.
    fn tree(root: &Path) -> BTreeMap<PathBuf, (u64, String)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let bytes = std::fs::read(&p).unwrap();
                    out.insert(p, (bytes.len() as u64, crate::xxhash64_hex(&bytes)));
                }
            }
        }
        out
    }

    #[test]
    fn indexes_are_listed_with_checkout_writer_and_size_and_nothing_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("wrongstack-home");
        let checkout = tmp.path().join("my checkout #1");
        std::fs::create_dir_all(&checkout).unwrap();
        let projects = home.join("projects");

        // 1. A project WrongStack knows (meta.json), with its index server
        //    recorded as this test process (a running program, not bun).
        let a = projects.join("my-checkout-1-abc123");
        make_index(&a.join(INDEX_DIR), &[(r"C:\x\a.ts", 3), (r"C:\x\b.ts", 4)]);
        std::fs::write(
            a.join(META_FILE),
            serde_json::json!({"root": checkout.display().to_string(), "slug": "my-checkout-1-abc123"})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            a.join(INDEX_DIR).join(SERVER_FILE),
            serde_json::json!({
                "pid": std::process::id(),
                "projectRoot": checkout.display().to_string(),
                "startedAt": "2026-11-06T23:18:55.701Z",
                "runtimeVersion": "1.5.0",
                "authToken": "fake-token-for-this-test-only",
                "endpoint": r"\\.\pipe\wrongstack-codebase-index-v1-x",
            })
            .to_string(),
        )
        .unwrap();
        // A write-ahead log beside it counts toward the size.
        std::fs::write(a.join(INDEX_DIR).join("index.db-wal"), [0u8; 100]).unwrap();

        // 2. No meta.json, no server: the checkout is the files' common folder,
        //    which no longer exists. Owned by a WrongIndex that is not running.
        let b = projects.join("gone-def456");
        make_index(
            &b.join(INDEX_DIR),
            &[
                (r"Z:\gone\proj\src\a.rs", 2),
                (r"Z:\gone\proj\tests\b.rs", 1),
            ],
        );
        std::fs::write(
            b.join(INDEX_DIR).join(owner_lock::OWNER_FILE),
            serde_json::json!({"owner": "wrongindexd", "instanceId": "d_x", "pid": 4_000_000_000u32,
              "epoch": 2, "profileId": "p", "acquiredAtMs": 1_794_000_000_000i64})
                .to_string(),
        )
        .unwrap();

        // 3. A linked worktree's index, and a file that is not an index.
        make_index(
            &a.join(WORKTREES_DIR).join("feature-1a2b3c"),
            &[(r"D:\wt\feature\x.go", 5)],
        );
        let c = projects.join("broken-000000");
        std::fs::create_dir_all(c.join(INDEX_DIR)).unwrap();
        std::fs::write(c.join(INDEX_DIR).join("index.db"), b"not sqlite at all").unwrap();
        // 4. A project folder without an index is looked at, not listed.
        std::fs::create_dir_all(projects.join("no-index-111111")).unwrap();

        let before = tree(&home);
        let s = scan(&home);
        assert_eq!(tree(&home), before, "the scan changed WrongStack's folders");

        assert_eq!(s.project_dirs, 4);
        assert_eq!(s.indexes.len(), 4, "{:#?}", s.indexes);
        assert!(!s.truncated);
        let by = |name: &str, wt: bool| {
            s.indexes
                .iter()
                .find(|i| i.project_dir == name && i.worktree.is_some() == wt)
                .unwrap()
        };

        let ia = by("my-checkout-1-abc123", false);
        assert_eq!(
            ia.checkout_root.as_deref(),
            Some(&*checkout.display().to_string())
        );
        assert_eq!(ia.checkout_source, Some(CheckoutSource::Meta));
        assert_eq!(ia.checkout_exists, Some(true));
        assert_eq!((ia.files, ia.symbols), (Some(2), Some(7)));
        assert_eq!(ia.schema_version.as_deref(), Some("5"));
        assert_eq!(ia.last_indexed_ms, Some(1_794_006_770_919));
        assert_eq!(ia.wal_bytes, 100);
        let srv = ia.wrongstack_server.as_ref().unwrap();
        assert_eq!(srv.pid, std::process::id());
        // This test is not bun or node: recorded, but not an index server.
        assert!(!srv.running && srv.program.is_none());
        assert_eq!(srv.since_ms, Some(1_794_007_135_701));
        assert_eq!(srv.version.as_deref(), Some("1.5.0"));
        assert!(ia.wrongindex_owner.is_none());

        let ib = by("gone-def456", false);
        assert_eq!(ib.checkout_root.as_deref(), Some(r"Z:\gone\proj"));
        assert_eq!(ib.checkout_source, Some(CheckoutSource::Files));
        assert_eq!(ib.checkout_exists, Some(false));
        let own = ib.wrongindex_owner.as_ref().unwrap();
        assert!(!own.running);
        assert_eq!(own.since_ms, Some(1_794_000_000_000));

        let iw = by("my-checkout-1-abc123", true);
        assert_eq!(iw.worktree.as_deref(), Some("feature-1a2b3c"));
        assert_eq!(iw.checkout_root.as_deref(), Some(r"D:\wt\feature"));

        let ic = by("broken-000000", false);
        assert!(ic.read_error.is_some() && ic.files.is_none());

        // The index server's token is never read, so never shown.
        let out = serde_json::to_string(&s).unwrap();
        assert!(!out.contains("fake-token-for-this-test-only"));
        assert!(!out.contains("authToken") && !out.contains("pipe"));
        assert_eq!(
            s.total_bytes,
            s.indexes
                .iter()
                .map(|i| i.db_bytes + i.wal_bytes)
                .sum::<u64>()
        );
    }

    #[test]
    fn a_missing_home_is_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let s = scan(&tmp.path().join("nowhere"));
        assert_eq!((s.project_dirs, s.indexes.len(), s.total_bytes), (0, 0, 0));
    }

    #[cfg(windows)]
    #[test]
    fn a_running_program_is_named_and_a_free_pid_is_not() {
        let me = process_program(std::process::id()).unwrap();
        assert!(me.to_lowercase().ends_with(".exe"), "{me}");
        assert!(process_program(4_000_000_000).is_none());
        assert!(is_one_of(r"C:\Users\x\.bun\bin\bun.exe", &["bun.exe"]));
        assert!(is_one_of("BUN.EXE", &["bun.exe"]));
        assert!(!is_one_of(r"C:\bun.exe\other.exe", &["bun.exe"]));
    }

    #[test]
    fn helpers() {
        assert_eq!(
            common_folder(r"C:\p\src\a.ts", r"C:\p\tests\b.ts").as_deref(),
            Some(r"C:\p")
        );
        assert_eq!(common_folder(r"C:\a.ts", r"D:\b.ts"), None);
        assert_eq!(
            common_folder(r"C:\a.ts", r"C:\b.ts").as_deref(),
            Some(r"C:\")
        );
        assert_eq!(
            common_folder("/home/u/p/a.rs", "/home/u/p/b/c.rs").as_deref(),
            Some("/home/u/p")
        );
        assert_eq!(
            common_folder(r"C:\p\only.ts", r"C:\p\only.ts").as_deref(),
            Some(r"C:\p")
        );
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-11-06T23:18:55Z"),
            Some(1_794_007_135_000)
        );
        assert_eq!(parse_iso_ms("not a date"), None);
        assert_eq!(
            file_uri(Path::new(r"C:\Users\x\my dir #1\index.db")),
            "file:///C:/Users/x/my%20dir%20%231/index.db?immutable=1"
        );
    }
}
