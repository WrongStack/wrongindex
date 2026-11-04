//! Disk pressure (architecture §9 "Disk full"; indexing-pipeline §12).
//!
//! * Before a write job runs, the volume holding the project's index must
//!   have room for it: [`FULL_BYTES`] always, plus the index's current size
//!   for a run that may write a complete new copy (full, reconcile, rebuild,
//!   restore). Otherwise the job fails with `DISK_FULL` before anything is
//!   written.
//! * A write that runs out of space anyway (SQLite's `SQLITE_FULL`, Windows'
//!   `ERROR_DISK_FULL`) rolls back - the index keeps its last published
//!   generation - and is reported as `DISK_FULL` too.
//! * The watcher neither retries a batch refused for space on its usual
//!   backoff nor counts it toward parking: the project waits (`watchHealth:
//!   disk_full`) until the volume has the room the batch needs, then runs it
//!   at once.
//! * The volumes holding the data root and every index are looked at every
//!   few seconds. A change of level (`ok`; `low` under [`LOW_BYTES`]; `full`
//!   under [`FULL_BYTES`]) is published as a `resource.warning` event, and
//!   the volumes that are not `ok` are part of `events.snapshot`.
//!
//! Nothing is ever deleted to make room (indexing-pipeline §12).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};
use wrongindex_contracts::{ErrorBody, ErrorCode, JobKind};

/// Below this much free space a volume is `full`: no write job starts.
pub const FULL_BYTES: u64 = 256 * 1024 * 1024;
/// Below this much free space a volume is `low` (reported, nothing refused).
pub const LOW_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Low,
    Full,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Low => "low",
            Level::Full => "full",
        }
    }

    /// The level for `free` bytes, coming from `previous`. Leaving a level
    /// takes a margin above its threshold (1/16), so a volume hovering at a
    /// threshold does not publish an event every few seconds.
    pub fn of(free: u64, previous: Level) -> Level {
        let full_exit = FULL_BYTES + FULL_BYTES / 16;
        let low_exit = LOW_BYTES + LOW_BYTES / 16;
        if free < FULL_BYTES || (previous == Level::Full && free < full_exit) {
            Level::Full
        } else if free < LOW_BYTES || (previous != Level::Ok && free < low_exit) {
            Level::Low
        } else {
            Level::Ok
        }
    }
}

/// Free bytes available to this user on the volume holding `path` (or its
/// nearest existing ancestor: an index directory may not exist yet). `None`
/// when it cannot be told; nothing is refused then.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(debug_assertions)]
    if let Some(n) = fault_free_bytes() {
        return Some(n);
    }
    os_free_bytes(path.ancestors().find(|p| p.exists())?)
}

/// Test hook, debug builds only: `WRONGINDEX_FAULT_FREE_BYTES_FILE` names a
/// file holding a byte count; while it exists, every volume reports that
/// much free space (read on each check, so a test can change it).
#[cfg(debug_assertions)]
fn fault_free_bytes() -> Option<u64> {
    let file = std::env::var_os("WRONGINDEX_FAULT_FREE_BYTES_FILE")?;
    std::fs::read_to_string(file).ok()?.trim().parse().ok()
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn os_free_bytes(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let name = wide(path);
    let mut available: u64 = 0;
    // SAFETY: `name` is NUL-terminated and outlives the call; the one out
    // pointer is valid, the others may be null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            name.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(not(windows))]
fn os_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// The volume (mount point, e.g. `C:\`) holding `path` or its nearest
/// existing ancestor.
#[cfg(windows)]
pub fn volume_of(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let name = wide(path.ancestors().find(|p| p.exists())?);
    let mut buf = vec![0u16; 1024];
    // SAFETY: `name` is NUL-terminated; `buf` is writable for its length.
    let ok = unsafe { GetVolumePathNameW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
}

#[cfg(not(windows))]
pub fn volume_of(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.exists())
        .map(|p| p.ancestors().last().unwrap_or(p).to_path_buf())
}

/// Bytes of `index.db` and its WAL.
fn index_bytes(index_dir: &Path) -> u64 {
    ["index.db", "index.db-wal"]
        .iter()
        .filter_map(|f| std::fs::metadata(index_dir.join(f)).ok())
        .map(|m| m.len())
        .sum()
}

/// Free space a job of this kind needs on the index's volume: the floor,
/// plus a complete copy of the index for a run that may write one (a fresh
/// build in staging, a maintenance rebuild, a restore). A targeted or
/// in-place incremental run only grows the WAL.
pub fn needed(kind: JobKind, index_dir: &Path) -> u64 {
    let copy = match kind {
        JobKind::Incremental | JobKind::Check => 0,
        // A backup is a complete copy too.
        JobKind::Full
        | JobKind::Reconcile
        | JobKind::Rebuild
        | JobKind::Restore
        | JobKind::Backup => index_bytes(index_dir),
        // The log's pages move into the index, which may grow by as much.
        JobKind::Checkpoint => wrongindex_storage::wal::wal_bytes(index_dir),
    };
    FULL_BYTES.saturating_add(copy)
}

/// `Err(DISK_FULL)` when the index's volume has less free space than
/// `need`. Unknown free space is never a refusal.
pub fn ensure_room(index_dir: &Path, need: u64) -> Result<(), ErrorBody> {
    match free_bytes(index_dir) {
        Some(free) if free < need => Err(ErrorBody::new(
            ErrorCode::DiskFull,
            format!(
                "only {} free on {}; indexing needs at least {} there, so nothing was written. Free disk space: watched projects resume by themselves",
                human(free),
                volume_of(index_dir)
                    .map(|v| v.display().to_string())
                    .unwrap_or_else(|| "the index's volume".into()),
                human(need),
            ),
        )),
        _ => Ok(()),
    }
}

/// Whether an error message says the disk is full, as SQLite ("database or
/// disk is full"), Node (`ENOSPC`) and Windows ("(os error 112)", the
/// message itself is localized) put it. For errors that arrive as text (the
/// compatibility worker's).
pub fn says_disk_full(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("disk is full")
        || m.contains("sqlite_full")
        || m.contains("enospc")
        || m.contains("not enough space on the disk")
        || (cfg!(windows) && (m.contains("(os error 112)") || m.contains("(os error 39)")))
}

/// `DISK_FULL` for an error whose message says the disk is full.
pub fn classify(e: ErrorBody) -> ErrorBody {
    if e.code != ErrorCode::DiskFull && says_disk_full(&e.message) {
        ErrorBody::new(ErrorCode::DiskFull, e.message)
    } else {
        e
    }
}

pub fn human(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= 1024.0 * MIB {
        format!("{:.1} GB", b / (1024.0 * MIB))
    } else {
        format!("{:.0} MB", b / MIB)
    }
}

#[derive(Debug, Clone)]
struct VolumeState {
    level: Level,
    free: u64,
    projects: Vec<String>,
    /// WrongIndex's own files there by category, while not `ok`.
    usage: Option<Value>,
}

/// The volumes in use and their last level.
#[derive(Default)]
pub struct Monitor {
    volumes: Mutex<HashMap<PathBuf, VolumeState>>,
    /// Directory -> volume, looked up once per directory.
    volume_cache: Mutex<HashMap<PathBuf, PathBuf>>,
    /// A check is running (one at a time: a volume that does not answer,
    /// such as a share that went away, must not pile up blocked threads).
    busy: std::sync::atomic::AtomicBool,
}

/// Clears the monitor's busy flag when a check ends.
pub struct CheckSlot<'a>(&'a Monitor);

impl Drop for CheckSlot<'_> {
    fn drop(&mut self) {
        self.0
            .busy
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Monitor {
    /// The right to run a check, unless one is running.
    pub fn slot(&self) -> Option<CheckSlot<'_>> {
        (!self.busy.swap(true, std::sync::atomic::Ordering::SeqCst)).then_some(CheckSlot(self))
    }

    fn volume(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(v) = self.volume_cache.lock().ok()?.get(dir) {
            return Some(v.clone());
        }
        let v = volume_of(dir)?;
        if let Ok(mut c) = self.volume_cache.lock() {
            if c.len() > 4096 {
                c.clear();
            }
            c.insert(dir.to_path_buf(), v.clone());
        }
        Some(v)
    }

    /// Look at the volumes holding `data_root` and the given index
    /// directories (project id, index dir); returns the `resource.warning`
    /// payloads for volumes whose level changed. A volume first seen at
    /// `ok` is not news.
    pub fn check(&self, data_root: &Path, indexes: &[(String, PathBuf)]) -> Vec<Value> {
        let mut by_volume: HashMap<PathBuf, OnVolume> = HashMap::new();
        if let Some(v) = self.volume(data_root) {
            by_volume
                .entry(v)
                .or_insert_with(|| OnVolume::new(data_root))
                .data_root = Some(data_root.to_path_buf());
        }
        for (pid, dir) in indexes {
            if let Some(v) = self.volume(dir) {
                let on = by_volume.entry(v).or_insert_with(|| OnVolume::new(dir));
                on.projects.push(pid.clone());
                on.index_dirs.push(dir.clone());
            }
        }
        let Ok(mut known) = self.volumes.lock() else {
            return Vec::new();
        };
        let seen: Vec<PathBuf> = by_volume.keys().cloned().collect();
        let mut changes = Vec::new();
        for (volume, mut on) in by_volume {
            let Some(free) = free_bytes(&on.probe) else {
                continue;
            };
            on.projects.sort();
            let previous = known.get(&volume).map(|s| s.level).unwrap_or(Level::Ok);
            let level = Level::of(free, previous);
            // What WrongIndex itself keeps there, only when it matters.
            let usage =
                (level != Level::Ok).then(|| usage(on.data_root.as_deref(), &on.index_dirs));
            let state = VolumeState {
                level,
                free,
                projects: on.projects,
                usage,
            };
            if level != previous {
                changes.push(payload(&volume, &state));
            }
            known.insert(volume, state);
        }
        known.retain(|v, _| seen.contains(v));
        changes
    }

    /// Volumes not at `ok`, for `events.snapshot`.
    pub fn warnings(&self) -> Vec<Value> {
        let Ok(known) = self.volumes.lock() else {
            return Vec::new();
        };
        let mut out: Vec<(&PathBuf, &VolumeState)> =
            known.iter().filter(|(_, s)| s.level != Level::Ok).collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out.into_iter().map(|(v, s)| payload(v, s)).collect()
    }
}

fn payload(volume: &Path, s: &VolumeState) -> Value {
    let mut v = json!({
        "resource": "disk",
        "volume": volume.display().to_string(),
        "level": s.level.as_str(),
        "freeBytes": s.free,
        "lowBytes": LOW_BYTES,
        "fullBytes": FULL_BYTES,
        "projects": s.projects,
    });
    if let Some(u) = &s.usage {
        v["usage"] = u.clone();
    }
    v
}

/// The data root and the index directories on one volume.
struct OnVolume {
    /// An existing directory on it, to ask for its free space.
    probe: PathBuf,
    data_root: Option<PathBuf>,
    index_dirs: Vec<PathBuf>,
    projects: Vec<String>,
}

impl OnVolume {
    fn new(probe: &Path) -> Self {
        Self {
            probe: probe.to_path_buf(),
            data_root: None,
            index_dirs: Vec::new(),
            projects: Vec::new(),
        }
    }
}

/// Bytes of the files under `dir` (recursive, links not followed).
fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_bytes(&e.path()),
            Ok(t) if t.is_file() => file_bytes(&e.path()),
            _ => 0,
        })
        .sum()
}

/// A file's size, asked of the file itself: on Windows the directory
/// listing's size of a file another handle is still writing (the log, a
/// WAL) can be far behind.
fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// What WrongIndex keeps on a volume, by category (indexing-pipeline §12:
/// disk-pressure reporting names what holds the space): the indexes, their
/// write-ahead logs, backups (claim backups and the files a maintenance
/// rebuild or restore kept), staging directories a run left, the service's
/// logs, and its other data (registry, downloaded updates).
fn usage(data_root: Option<&Path>, index_dirs: &[PathBuf]) -> Value {
    let (mut index, mut wal, mut backups, mut staging) = (0u64, 0u64, 0u64, 0u64);
    for dir in index_dirs {
        index += file_bytes(&dir.join("index.db"));
        wal += file_bytes(&dir.join("index.db-wal")) + file_bytes(&dir.join("index.db-shm"));
        backups += tree_bytes(&dir.join(".wrongindex-backups"));
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("index.db.wrongindex-backup-") {
                    backups += file_bytes(&e.path());
                } else if name.starts_with(".wrongindex-staging-") {
                    staging += tree_bytes(&e.path());
                }
            }
        }
    }
    let (logs, service) = match data_root {
        Some(root) => {
            let logs = tree_bytes(&root.join("logs"));
            (logs, tree_bytes(root).saturating_sub(logs))
        }
        None => (0, 0),
    };
    json!({
        "indexBytes": index,
        "walBytes": wal,
        "backupBytes": backups,
        "stagingBytes": staging,
        "logBytes": logs,
        "serviceBytes": service,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn levels_and_their_margins() {
        assert_eq!(Level::of(10 * 1024 * MIB, Level::Ok), Level::Ok);
        assert_eq!(Level::of(LOW_BYTES - 1, Level::Ok), Level::Low);
        assert_eq!(Level::of(FULL_BYTES - 1, Level::Ok), Level::Full);
        assert_eq!(Level::of(FULL_BYTES - 1, Level::Low), Level::Full);
        // Leaving `full` takes a 16 MiB margin above 256 MiB...
        assert_eq!(Level::of(FULL_BYTES + MIB, Level::Full), Level::Full);
        assert_eq!(Level::of(FULL_BYTES + 16 * MIB, Level::Full), Level::Low);
        // ... and leaving `low` 64 MiB above 1 GiB; entering needs none.
        assert_eq!(Level::of(LOW_BYTES + MIB, Level::Low), Level::Low);
        assert_eq!(Level::of(LOW_BYTES + 64 * MIB, Level::Low), Level::Ok);
        assert_eq!(Level::of(LOW_BYTES + MIB, Level::Ok), Level::Ok);
        assert_eq!(Level::of(LOW_BYTES + 64 * MIB, Level::Full), Level::Ok);
    }

    #[test]
    fn disk_full_messages_are_recognised() {
        for m in [
            "native engine: sqlite: database or disk is full",
            "SqliteError: SQLITE_FULL: database or disk is full",
            "ENOSPC: no space left on device, write",
            "io: There is not enough space on the disk. (os error 112)",
        ] {
            assert!(says_disk_full(m), "{m}");
        }
        #[cfg(windows)]
        assert!(says_disk_full(
            "io: Diskte yeterli alan yok. (os error 112)"
        ));
        for m in [
            "the index is in use elsewhere",
            "database is locked",
            "os error 5",
        ] {
            assert!(!says_disk_full(m), "{m}");
        }
        let e = classify(ErrorBody::new(
            ErrorCode::Internal,
            "native engine: sqlite: database or disk is full",
        ));
        assert_eq!(e.code, ErrorCode::DiskFull);
        assert!(!e.retryable);
        assert_eq!(
            classify(ErrorBody::new(ErrorCode::Internal, "boom")).code,
            ErrorCode::Internal
        );
    }

    #[test]
    fn usage_names_what_holds_the_space() {
        let t = tempfile::tempdir().unwrap();
        let put = |rel: &str, n: usize| {
            let p = t.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![0u8; n]).unwrap();
        };
        put("a/index.db", 1000);
        put("a/index.db-wal", 300);
        put("a/index.db-shm", 32);
        put("a/.wrongindex-backups/b1.db", 900);
        put("a/index.db.wrongindex-backup-17", 800);
        put("a/.wrongindex-staging-x/index.db", 50);
        put("a/.wrongindex-owner.json", 7);
        put("b/index.db", 2000);
        put("home/logs/wrongindexd.log", 400);
        put("home/logs/wrongindexd.log.1", 100);
        put("home/registry.db", 60);
        put("home/updates/0.2.0/setup.exe", 5);
        let u = usage(
            Some(&t.path().join("home")),
            &[
                t.path().join("a"),
                t.path().join("b"),
                t.path().join("gone"),
            ],
        );
        assert_eq!(
            u,
            json!({
                "indexBytes": 3000, "walBytes": 332, "backupBytes": 1700,
                "stagingBytes": 50, "logBytes": 500, "serviceBytes": 65,
            })
        );
        assert_eq!(usage(None, &[])["logBytes"], 0);
    }

    #[test]
    fn a_copy_is_needed_only_for_runs_that_may_write_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.db"), vec![0u8; 3000]).unwrap();
        std::fs::write(dir.path().join("index.db-wal"), vec![0u8; 1000]).unwrap();
        assert_eq!(needed(JobKind::Incremental, dir.path()), FULL_BYTES);
        assert_eq!(needed(JobKind::Full, dir.path()), FULL_BYTES + 4000);
        assert_eq!(needed(JobKind::Rebuild, dir.path()), FULL_BYTES + 4000);
        // No index yet.
        assert_eq!(needed(JobKind::Full, &dir.path().join("new")), FULL_BYTES);
    }

    #[cfg(windows)]
    #[test]
    fn free_space_and_volume_of_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not").join("yet");
        let v = volume_of(&missing).unwrap();
        assert!(dir.path().starts_with(&v), "{v:?}");
        if std::env::var_os("WRONGINDEX_FAULT_FREE_BYTES_FILE").is_none() {
            assert!(free_bytes(&missing).unwrap() > 0);
        }
        assert_eq!(human(300 * MIB), "300 MB");
        assert_eq!(human(LOW_BYTES + LOW_BYTES / 2), "1.5 GB");
    }
}
