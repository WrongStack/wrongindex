//! `wrongindex setup stop-installed <dir>` (installer support).
//!
//! The installer and the uninstaller stop the service gracefully first
//! (`daemon stop`), which reaches only the service of the data root they see
//! (`WRONGINDEX_HOME` or `~/.wrongindex`). A service started with another
//! data root (`--data-root`, a `WRONGINDEX_HOME` set in one shell) kept
//! running from the install directory: an uninstall left `wrongindexd.exe`
//! behind with the service still running, and an upgrade could not replace
//! it (found 2026-11-04 by `scripts/release/smoke-uninstall-running.ps1`).
//!
//! This ends every process whose program lies in the install directory,
//! other than this one and the processes that started it, and waits until
//! each has exited (so its files are no longer locked). The starters matter:
//! `wrongindex self-update` waits in the install directory (moved aside as
//! `wrongindex.exe.old`) for the installer that runs this. `self-update`
//! also runs it itself before the installer, so it can name what it ended.
//! A service ended this way recovers as after a crash: its native child dies
//! with it (job object), and at the next start interrupted jobs are requeued
//! (checks) or failed.

use std::path::Path;

/// A process this ended: its id and program.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Stopped {
    pub pid: u32,
    pub program: String,
}

/// The comparable spelling of a path: no verbatim prefix, backslashes,
/// lower case, no trailing separator.
fn key(p: &str) -> String {
    let s = p.strip_prefix(r"\\?\").unwrap_or(p).replace('/', "\\");
    s.trim_end_matches('\\').to_lowercase()
}

/// Whether `program` lies in (or under) `dir`, both as [`key`].
fn inside(dir: &str, program: &str) -> bool {
    program
        .strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('\\'))
}

#[cfg(windows)]
pub fn stop_installed(dir: &Path) -> Result<Vec<Stopped>, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject,
    };

    // The directory as named and as it really is: a path with 8.3 short
    // names (`RUNNER~1`) and its long spelling are the same files, and a
    // process's program is reported the way it was started.
    let real = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dirs = [key(&dir.to_string_lossy()), key(&real.to_string_lossy())];
    let me = unsafe { GetCurrentProcessId() };
    // Every process with its parent.
    let mut parents = std::collections::HashMap::new();
    // SAFETY: a snapshot handle this function owns and closes; the entry is
    // plain data with its size set as the API requires.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(format!(
                "process snapshot: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snap, &mut entry) != 0;
        while more {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            more = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    let spared = starters(me, &parents);
    let mut stopped = Vec::new();
    for &pid in parents.keys().filter(|p| **p != 0 && !spared.contains(p)) {
        // SAFETY: a process handle this loop owns and closes; the name
        // buffer outlives the call that fills it.
        unsafe {
            let h = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            );
            if h.is_null() {
                continue; // another user's, or gone already
            }
            let mut buf = [0u16; 32_768];
            let mut len = buf.len() as u32;
            let named =
                QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
            let program = String::from_utf16_lossy(&buf[..len as usize]);
            if named && from(&dirs, &program) && TerminateProcess(h, 1) != 0 {
                // Until it is gone its files stay locked.
                WaitForSingleObject(h, 10_000);
                stopped.push(Stopped { pid, program });
            }
            CloseHandle(h);
        }
    }
    Ok(stopped)
}

/// " Ended beforehand, as they ran from the install directory: name (pid
/// n), ..." for a message, or nothing.
pub fn ended_note(stopped: &[Stopped]) -> String {
    if stopped.is_empty() {
        return String::new();
    }
    let names: Vec<String> = stopped
        .iter()
        .map(|s| format!("{} (pid {})", file_name(&s.program), s.pid))
        .collect();
    format!(
        " Ended beforehand, as they ran from the install directory: {}.",
        names.join(", ")
    )
}

/// The file name of a program path (either separator).
pub fn file_name(program: &str) -> &str {
    program.rsplit(['\\', '/']).next().unwrap_or(program)
}

/// Wait up to `budget` for process `pid` to exit; `true` once it has (or
/// cannot be found).
#[cfg(windows)]
pub fn wait_exit(pid: u32, budget: std::time::Duration) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: a process handle this function opens and closes.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return true;
        }
        let ms = u32::try_from(budget.as_millis()).unwrap_or(u32::MAX);
        let gone = WaitForSingleObject(h, ms) == WAIT_OBJECT_0;
        CloseHandle(h);
        gone
    }
}

#[cfg(not(windows))]
pub fn wait_exit(_pid: u32, _budget: std::time::Duration) -> bool {
    true
}

/// Whether `program` lies in one of `dirs` ([`key`] spellings), as reported
/// or as it really is (its long spelling).
#[cfg(windows)]
fn from(dirs: &[String], program: &str) -> bool {
    let within = |p: &str| dirs.iter().any(|d| inside(d, &key(p)));
    within(program)
        || std::fs::canonicalize(program).is_ok_and(|real| within(&real.to_string_lossy()))
}

/// `me` and the processes that started it, as far as they are still known
/// (a parent id that is gone, or reused, ends the chain safely: at worst a
/// process is spared).
fn starters(
    me: u32,
    parents: &std::collections::HashMap<u32, u32>,
) -> std::collections::HashSet<u32> {
    let mut spared = std::collections::HashSet::from([me]);
    let mut at = me;
    while let Some(&parent) = parents.get(&at) {
        if parent == 0 || !spared.insert(parent) || spared.len() > 64 {
            break;
        }
        at = parent;
    }
    spared
}

#[cfg(not(windows))]
pub fn stop_installed(_dir: &Path) -> Result<Vec<Stopped>, String> {
    Ok(Vec::new()) // the installer is Windows-only
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_programs_in_the_directory_count() {
        let dir = key(r"\\?\C:\Users\me\AppData\Local\WrongIndex\");
        assert_eq!(dir, r"c:\users\me\appdata\local\wrongindex");
        for yes in [
            r"C:\Users\me\AppData\Local\WrongIndex\wrongindexd.exe",
            r"c:/users/me/appdata/local/wrongindex/sub/x.exe",
        ] {
            assert!(inside(&dir, &key(yes)), "{yes}");
        }
        for no in [
            r"C:\Users\me\AppData\Local\WrongIndex-dev\wrongindexd.exe",
            r"C:\Users\me\AppData\Local\wrongindexd.exe",
            r"D:\Users\me\AppData\Local\WrongIndex\wrongindexd.exe",
        ] {
            assert!(!inside(&dir, &key(no)), "{no}");
        }
    }

    #[test]
    fn the_starters_are_spared() {
        // 10 -> 20 -> 30 (me); 40 elsewhere; a loop does not hang it.
        let parents = std::collections::HashMap::from([
            (30, 20),
            (20, 10),
            (10, 0),
            (40, 10),
            (50, 51),
            (51, 50),
        ]);
        let spared = starters(30, &parents);
        assert_eq!(
            spared,
            std::collections::HashSet::from([30, 20, 10]),
            "{spared:?}"
        );
        assert_eq!(
            starters(50, &parents),
            std::collections::HashSet::from([50, 51])
        );
    }

    #[test]
    fn what_was_ended_is_named() {
        let s = [
            Stopped {
                pid: 812,
                program: r"C:\Users\me\AppData\Local\WrongIndex\wrongindexd.exe".into(),
            },
            Stopped {
                pid: 9,
                program: "C:/x/wrongindex.exe".into(),
            },
        ];
        assert_eq!(
            ended_note(&s),
            " Ended beforehand, as they ran from the install directory: wrongindexd.exe (pid 812), wrongindex.exe (pid 9)."
        );
        assert_eq!(ended_note(&[]), "");
    }

    /// A program run from a directory is ended; the same program run from
    /// elsewhere is not.
    #[cfg(windows)]
    #[test]
    fn processes_running_from_the_directory_are_ended() {
        let tmp = tempfile::tempdir().unwrap();
        // A name long enough to get an 8.3 short name where the volume
        // makes them (the CI runner's temp path is one: `RUNNER~1`).
        let app = tmp.path().join("installed application");
        std::fs::create_dir_all(&app).unwrap();
        let short = short_path(&app);
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let ping = std::path::Path::new(&system).join(r"System32\PING.EXE");
        let copy = app.join("ping.exe");
        std::fs::copy(&ping, &copy).unwrap();
        let spawn = |exe: &std::path::Path| {
            std::process::Command::new(exe)
                .args(["-n", "120", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut outside_child = spawn(&ping);
        // Started through one spelling, the directory named in the other:
        // the same files either way.
        for (started, named) in [
            (short.join("ping.exe"), app.clone()),
            (copy.clone(), short.clone()),
        ] {
            let mut inside_child = spawn(&started);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let stopped = stop_installed(&named).unwrap();
            assert_eq!(
                stopped.len(),
                1,
                "{} from {}: {stopped:?}",
                named.display(),
                started.display()
            );
            assert_eq!(stopped[0].pid, inside_child.id());
            // Ended and waited for: it has exited.
            assert!(inside_child.try_wait().unwrap().is_some());
        }
        // Its program can go.
        std::fs::remove_file(&copy).unwrap();
        // The same program elsewhere runs on.
        assert!(outside_child.try_wait().unwrap().is_none());
        let _ = outside_child.kill();
        let _ = outside_child.wait();
        // Nothing left there: nothing to do.
        assert!(stop_installed(&app).unwrap().is_empty());
    }

    /// The 8.3 spelling of `p` (the same path where the volume makes none).
    #[cfg(windows)]
    fn short_path(p: &std::path::Path) -> std::path::PathBuf {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: both buffers outlive the call; the length bounds the write.
        let n = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
                wide.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        } as usize;
        assert!(n > 0 && n < buf.len(), "GetShortPathNameW failed");
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
    }
}
