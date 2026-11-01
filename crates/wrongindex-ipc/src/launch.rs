//! Starting the per-user daemon from a client (CLI or desktop).
//!
//! The daemon must outlive the process that started it (INV-12): it is
//! detached from the caller's console and, when the job object allows it,
//! broken away from the caller's job so closing a terminal or the desktop
//! application never stops indexing.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::DataRoot;

pub const DAEMON_EXE: &str = if cfg!(windows) {
    "wrongindexd.exe"
} else {
    "wrongindexd"
};

/// The daemon binary installed next to the running client executable.
pub fn sibling_daemon_exe() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(DAEMON_EXE))
}

/// Spawn `exe` as a detached daemon for `root`.
pub fn spawn_detached(exe: &Path, root: &DataRoot) -> io::Result<Child> {
    if !exe.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("daemon binary not found at {}", exe.display()),
        ));
    }
    let mut cmd = Command::new(exe);
    cmd.arg("--data-root")
        .arg(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = cmd.spawn() {
            return Ok(child);
        }
        // Breakaway is refused inside some job objects; detach anyway.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}
