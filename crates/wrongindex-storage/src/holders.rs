//! Which programs have an index open (Windows Restart Manager, Linux
//! `/proc`, macOS `lsof`), for the
//! messages that ask the user to close one: a write-ahead log a reader keeps
//! from being cut (`resource.warning`, `resource: "wal"`), and a restore or
//! a checkpoint refused with `DATABASE_BUSY`; and the list of this
//! machine's indexes ([`crate::machine`]), which says what has each open.
//!
//! SQLite cannot say which connection holds a read transaction. This names
//! every program that has the index's files open (`index.db`, `-wal`,
//! `-shm`) other than this service, which includes that one. Only names
//! and process ids, never paths or anything a program has open besides.

use std::path::Path;

/// The most programs named.
const MAX: usize = 16;

/// A program that has an index's files open.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
}

/// The programs other than this service that have the index in
/// `index_dir` open (none when that cannot be told).
pub fn of(index_dir: &Path) -> Vec<Holder> {
    let files: Vec<std::path::PathBuf> = ["index.db", "index.db-wal", "index.db-shm"]
        .iter()
        .map(|f| index_dir.join(f))
        .filter(|p| p.exists())
        .collect();
    list(&files, Some(std::process::id()))
}

/// `holders` as text for a message: "bun.exe (pid 812), node.exe (pid 9)".
pub fn describe(holders: &[Holder]) -> String {
    holders
        .iter()
        .map(|h| format!("{} (pid {})", h.name, h.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// " Open by: ..." for a message, or nothing.
pub fn suffix(holders: &[Holder]) -> String {
    if holders.is_empty() {
        String::new()
    } else {
        format!(" Open by: {}.", describe(holders))
    }
}

#[cfg(windows)]
fn list(files: &[std::path::PathBuf], except: Option<u32>) -> Vec<Holder> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::RestartManager::{
        CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
        RmStartSession,
    };
    if files.is_empty() {
        return Vec::new();
    }
    let wide: Vec<Vec<u16>> = files
        .iter()
        .map(|p| p.as_os_str().encode_wide().chain(Some(0)).collect())
        .collect();
    let names: Vec<*const u16> = wide.iter().map(|w| w.as_ptr()).collect();
    let mut out = Vec::new();
    // SAFETY: a Restart Manager session this function starts and ends; the
    // file names and the result buffer outlive the calls that use them, and
    // the count passed is the buffer's length.
    unsafe {
        let mut session = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        if RmStartSession(&mut session, 0, key.as_mut_ptr()) != 0 {
            return out;
        }
        if RmRegisterResources(
            session,
            names.len() as u32,
            names.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        ) == 0
        {
            let mut infos: Vec<RM_PROCESS_INFO> = vec![RM_PROCESS_INFO::default(); MAX];
            for _ in 0..2 {
                let (mut needed, mut count, mut reasons) = (0u32, infos.len() as u32, 0u32);
                let r = RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    infos.as_mut_ptr(),
                    &mut reasons,
                );
                if r == ERROR_MORE_DATA {
                    // More holders than room: once more with enough (they
                    // may change in between; then the first MAX are named).
                    infos.resize(
                        (needed as usize).clamp(MAX, 256),
                        RM_PROCESS_INFO::default(),
                    );
                    continue;
                }
                if r == 0 {
                    for i in infos.iter().take(count as usize) {
                        let pid = i.Process.dwProcessId;
                        if Some(pid) == except || out.iter().any(|h: &Holder| h.pid == pid) {
                            continue;
                        }
                        let len = i.strAppName.iter().position(|&c| c == 0).unwrap_or(0);
                        out.push(Holder {
                            pid,
                            name: String::from_utf16_lossy(&i.strAppName[..len]),
                        });
                    }
                }
                break;
            }
        }
        RmEndSession(session);
    }
    out.truncate(MAX);
    out
}

#[cfg(not(windows))]
fn list(files: &[std::path::PathBuf], except: Option<u32>) -> Vec<Holder> {
    lookup(files, except).unwrap_or_default()
}

/// Whether any process, this one included, has `file` open; `None` when
/// that cannot be told here.
#[cfg(not(windows))]
pub(crate) fn anyone_has_open(file: &Path) -> Option<bool> {
    lookup(&[file.to_path_buf()], None).map(|h| !h.is_empty())
}

/// The processes (but `except`) that have one of `files` open, matched by
/// device and inode, or `None` when this system cannot say.
///
/// Linux reads `/proc/<pid>/fd`: it sees the processes of this user (which
/// is where WrongStack runs), takes no lock, and opens none of the files.
/// SQLite's own locks on the `-shm` are not probed: POSIX drops a process's
/// locks on a file when any descriptor of it to that file closes, so a
/// probe would release the service's own connections' locks.
#[cfg(target_os = "linux")]
fn lookup(files: &[std::path::PathBuf], except: Option<u32>) -> Option<Vec<Holder>> {
    use std::os::unix::fs::MetadataExt;
    let ids: Vec<(u64, u64)> = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| (m.dev(), m.ino()))
        .collect();
    if ids.is_empty() {
        return Some(Vec::new());
    }
    let procs = std::fs::read_dir("/proc").ok()?;
    let mut out = Vec::new();
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if Some(pid) == except {
            continue;
        }
        // Another user's process: not readable, and not ours to name.
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            std::fs::metadata(fd.path()).is_ok_and(|m| ids.contains(&(m.dev(), m.ino())))
        });
        if holds {
            let name = std::fs::read_to_string(p.path().join("comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            out.push(Holder { pid, name });
            if out.len() >= MAX {
                break;
            }
        }
    }
    Some(out)
}

/// macOS: the system's `lsof` (`-F pc`: process id and command name).
/// It exits 1 when one of the files is open nowhere (the others are still
/// listed); a missing `lsof` or any other exit answers `None`.
#[cfg(target_os = "macos")]
fn lookup(files: &[std::path::PathBuf], except: Option<u32>) -> Option<Vec<Holder>> {
    let files: Vec<&std::path::PathBuf> = files.iter().filter(|f| f.exists()).collect();
    if files.is_empty() {
        return Some(Vec::new());
    }
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-w", "-F", "pc", "--"])
        .args(&files)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    match out.status.code() {
        Some(0 | 1) => Some(parse_lsof(&String::from_utf8_lossy(&out.stdout), except)),
        _ => None,
    }
}

/// `lsof -F pc` output: a `p<pid>` line, then `c<command>`, per process.
#[cfg(any(target_os = "macos", test))]
fn parse_lsof(text: &str, except: Option<u32>) -> Vec<Holder> {
    let mut out: Vec<Holder> = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse::<u32>().ok().filter(|p| Some(*p) != except);
        } else if let (Some(name), Some(p)) = (line.strip_prefix('c'), pid) {
            if !out.iter().any(|h| h.pid == p) && out.len() < MAX {
                out.push(Holder {
                    pid: p,
                    name: name.to_string(),
                });
            }
            pid = None;
        }
    }
    out
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn lookup(_files: &[std::path::PathBuf], _except: Option<u32>) -> Option<Vec<Holder>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_output_names_each_process_once() {
        let text = "p812\ncbun\nf12\np9\ncnode\np812\ncbun\np1\nclaunchd\n";
        let got = parse_lsof(text, Some(1));
        assert_eq!(
            got,
            vec![
                Holder {
                    pid: 812,
                    name: "bun".into()
                },
                Holder {
                    pid: 9,
                    name: "node".into()
                },
            ]
        );
        assert!(parse_lsof("", None).is_empty());
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_program_holding_a_file_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("index.db");
        std::fs::write(&f, b"x").unwrap();
        // Not held: nobody.
        assert!(list(std::slice::from_ref(&f), None).is_empty());
        let _open = std::fs::File::open(&f).unwrap();
        let held = list(std::slice::from_ref(&f), None);
        assert!(
            held.iter()
                .any(|h| h.pid == std::process::id() && !h.name.is_empty()),
            "{held:?}"
        );
        // This service itself is never named.
        assert!(of(tmp.path()).iter().all(|h| h.pid != std::process::id()));
        // Nothing there: nothing to ask.
        assert!(of(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn messages_name_programs_and_ids() {
        let h = [
            Holder {
                pid: 812,
                name: "bun.exe".into(),
            },
            Holder {
                pid: 9,
                name: "node.exe".into(),
            },
        ];
        assert_eq!(describe(&h), "bun.exe (pid 812), node.exe (pid 9)");
        assert_eq!(suffix(&h), " Open by: bun.exe (pid 812), node.exe (pid 9).");
        assert_eq!(suffix(&[]), "");
    }
}
