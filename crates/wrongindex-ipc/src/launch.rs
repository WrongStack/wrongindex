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
        // `Command` always creates children with handle inheritance on, so a
        // long-lived service would inherit this process's own standard
        // handles (e.g. the pipe a script reads the CLI's output from) and
        // keep that pipe open until it exits. Make them non-inheritable first.
        no_inherit_std_handles();
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

/// Clear `HANDLE_FLAG_INHERIT` on this process's standard handles.
///
/// `Command` passes a child its own inheritable duplicate of an inherited
/// standard handle, but with inheritance on the child also receives the
/// original: a grandchild that outlives everyone (the service) would keep a
/// caller's pipe open through it. A process that starts children which may
/// start the service (`self-update`) clears its own handles first.
#[cfg(windows)]
pub fn no_inherit_std_handles() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: querying and flagging this process's own std handles.
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
