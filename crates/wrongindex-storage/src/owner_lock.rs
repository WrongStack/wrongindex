//! OS-backed index ownership guard (WI-109, compatibility §6).
//!
//! The guard is a lock file beside the index, `<indexDir>/.wrongindex-owner.lock`,
//! held open without sharing for as long as the owner lives. Another process
//! (a second WrongIndex daemon with a different data root, or a cooperating
//! WrongStack adapter) cannot open it while it is held, and the OS releases it
//! when the owner exits or crashes. `.wrongindex-owner.json` beside it is
//! descriptive only and is never trusted as the lock.
//!
//! What this cannot do: stop an unmodified legacy writer that never looks at
//! the lock (see compatibility §6.1). That boundary is enforced by the
//! supported-client floor, not by this file.
//!
//! The guard is created only when ownership is explicitly acquired; inspecting
//! or registering a project never writes into its index directory.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const LOCK_FILE: &str = ".wrongindex-owner.lock";
pub const OWNER_FILE: &str = ".wrongindex-owner.json";

/// Descriptive owner record written next to the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerInfo {
    pub owner: String,
    pub instance_id: String,
    pub pid: u32,
    pub epoch: i64,
    pub profile_id: String,
    pub acquired_at_ms: i64,
}

/// A held ownership guard. Dropping it releases the OS lock.
#[derive(Debug)]
pub struct OwnerGuard {
    _lock: File,
    dir: PathBuf,
    info: OwnerInfo,
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another process holds the guard; `holder` is its descriptive record if readable.
    #[error("index is owned by another process{}", .holder.as_ref().map(|h| format!(" ({} pid {}, epoch {})", h.owner, h.pid, h.epoch)).unwrap_or_default())]
    Held { holder: Option<OwnerInfo> },
    #[error("cannot create ownership guard: {0}")]
    Io(#[from] io::Error),
}

impl OwnerGuard {
    /// Try to take exclusive ownership of the index in `index_dir`.
    /// The directory is created if missing (WrongStack creates it lazily too).
    pub fn acquire(index_dir: &Path, info: OwnerInfo) -> Result<Self, AcquireError> {
        std::fs::create_dir_all(index_dir)?;
        let lock_path = index_dir.join(LOCK_FILE);
        let lock = match open_exclusive(&lock_path) {
            Ok(f) => f,
            Err(e) if is_contention(&e) => {
                return Err(AcquireError::Held {
                    holder: read_owner(index_dir),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let guard = Self {
            _lock: lock,
            dir: index_dir.to_path_buf(),
            info,
        };
        guard.write_info()?;
        Ok(guard)
    }

    pub fn info(&self) -> &OwnerInfo {
        &self.info
    }

    pub fn index_dir(&self) -> &Path {
        &self.dir
    }

    fn write_info(&self) -> io::Result<()> {
        let tmp = self
            .dir
            .join(format!("{OWNER_FILE}.{}.tmp", std::process::id()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(&self.info).map_err(io::Error::other)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.dir.join(OWNER_FILE))
    }

    /// Release ownership explicitly: remove the descriptive record (only if it
    /// is still ours), then drop the lock.
    pub fn release(self) {
        if read_owner(&self.dir)
            .is_some_and(|i| i.instance_id == self.info.instance_id && i.epoch == self.info.epoch)
        {
            let _ = std::fs::remove_file(self.dir.join(OWNER_FILE));
        }
        // Lock released when `self._lock` drops; the lock file itself stays
        // (deleting it would race a concurrent acquirer opening it).
    }
}

/// Descriptive record of the current or last owner, if present and readable.
pub fn read_owner(index_dir: &Path) -> Option<OwnerInfo> {
    serde_json::from_slice(&std::fs::read(index_dir.join(OWNER_FILE)).ok()?).ok()
}

/// Whether some process currently holds the guard (probe without keeping it).
pub fn is_held(index_dir: &Path) -> bool {
    let p = index_dir.join(LOCK_FILE);
    if !p.exists() {
        return false;
    }
    match open_exclusive(&p) {
        Ok(_) => false,
        Err(e) => is_contention(&e),
    }
}

#[cfg(windows)]
fn open_exclusive(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // share_mode(0): no other handle may open the file while this one lives.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

#[cfg(windows)]
fn is_contention(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION
    e.raw_os_error() == Some(32)
}

#[cfg(unix)]
fn open_exclusive(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    unsafe extern "C" {
        fn flock(fd: i32, op: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // SAFETY: valid descriptor owned by `f`.
    if unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}

#[cfg(unix)]
fn is_contention(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(epoch: i64) -> OwnerInfo {
        OwnerInfo {
            owner: "wrongindexd".into(),
            instance_id: format!("d_test_{epoch}"),
            pid: std::process::id(),
            epoch,
            profile_id: "wrongstack-index-v5-baseline".into(),
            acquired_at_ms: 0,
        }
    }

    #[test]
    fn guard_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("idx");
        let g = OwnerGuard::acquire(&dir, info(1)).unwrap();
        assert!(is_held(&dir));
        match OwnerGuard::acquire(&dir, info(2)) {
            Err(AcquireError::Held { holder }) => assert_eq!(holder.unwrap().epoch, 1),
            other => panic!("expected Held, got {other:?}"),
        }
        drop(g);
        assert!(!is_held(&dir));
        let g2 = OwnerGuard::acquire(&dir, info(2)).unwrap();
        assert_eq!(read_owner(&dir).unwrap().epoch, 2);
        g2.release();
        assert!(read_owner(&dir).is_none());
        assert!(!is_held(&dir));
    }

    #[test]
    fn stale_owner_record_without_lock_is_not_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("idx");
        std::fs::create_dir_all(&dir).unwrap();
        // A crashed owner leaves its record; the lock is free.
        std::fs::write(dir.join(OWNER_FILE), serde_json::to_vec(&info(7)).unwrap()).unwrap();
        std::fs::write(dir.join(LOCK_FILE), b"").unwrap();
        assert!(!is_held(&dir));
        let g = OwnerGuard::acquire(&dir, info(8)).unwrap();
        assert_eq!(g.info().epoch, 8);
        assert_eq!(read_owner(&dir).unwrap().epoch, 8);
    }

    #[test]
    fn guard_held_by_another_process_is_detected() {
        // Hold the lock in a child process (PowerShell/sh keeps the handle open).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("idx");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join(LOCK_FILE);
        #[cfg(windows)]
        let mut child = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$f=[IO.File]::Open($env:WI_LOCK,'OpenOrCreate','ReadWrite','None'); Write-Output ready; Start-Sleep 30",
            ])
            .env("WI_LOCK", &lock)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        #[cfg(windows)]
        {
            use std::io::BufRead;
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "ready");
            assert!(is_held(&dir));
            assert!(matches!(
                OwnerGuard::acquire(&dir, info(1)),
                Err(AcquireError::Held { .. })
            ));
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(!is_held(&dir));
        }
    }
}
