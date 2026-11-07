//! Starting the per-user daemon from a client (CLI or desktop).
//!
//! The daemon must outlive the process that started it (INV-12): it is
//! detached from the caller's console and, when the job object allows it,
//! broken away from the caller's job so closing a terminal or the desktop
//! application never stops indexing.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
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
pub fn spawn_detached(exe: &Path, root: &DataRoot) -> io::Result<Detached> {
    if !exe.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("daemon binary not found at {}", exe.display()),
        ));
    }
    spawn_detached_program(exe, &[OsStr::new("--data-root"), root.path().as_os_str()])
}

/// A process started detached: its id, and whether it has ended.
#[derive(Debug)]
pub struct Detached {
    #[cfg(windows)]
    pid: u32,
    #[cfg(windows)]
    process: std::os::windows::io::OwnedHandle,
    #[cfg(not(windows))]
    child: Child,
}

impl Detached {
    pub fn id(&self) -> u32 {
        #[cfg(windows)]
        return self.pid;
        #[cfg(not(windows))]
        return self.child.id();
    }

    /// The exit code once the process has ended (`None` while it runs).
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::STILL_ACTIVE;
            use windows_sys::Win32::System::Threading::GetExitCodeProcess;
            let mut code = 0u32;
            // SAFETY: a process handle this value owns.
            if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((code != STILL_ACTIVE as u32).then_some(code as i32))
        }
        #[cfg(not(windows))]
        Ok(self.child.try_wait()?.and_then(|s| s.code()))
    }
}

/// Start `exe args` detached from this process: no console, its own process
/// group, out of this process's job when the job allows it, standard
/// handles on `NUL`, and the program's own folder as its working directory
/// (it never keeps the caller's folder in use).
///
/// On Windows the child inherits **only** that `NUL` handle
/// (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). `std::process::Command` always
/// creates children with handle inheritance on, so a long-lived service or
/// desktop started through it received every inheritable handle of its
/// starter: its standard handles (cleared by [`no_inherit_std_handles`]),
/// but also any other inheritable handle the starter had inherited itself,
/// such as a pipe of the script or installer that started the CLI. The
/// reader of such a pipe then waited until the service exited.
pub fn spawn_detached_program(exe: &Path, args: &[&OsStr]) -> io::Result<Detached> {
    let cwd = exe.parent().filter(|p| !p.as_os_str().is_empty());
    #[cfg(windows)]
    {
        win_spawn::spawn(exe, args, cwd)
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        Ok(Detached {
            child: cmd.spawn()?,
        })
    }
}

#[cfg(windows)]
mod win_spawn {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
        CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    };

    use super::Detached;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    /// Append `arg` quoted as `CommandLineToArgvW` reads it back.
    pub(super) fn push_arg(out: &mut Vec<u16>, arg: &OsStr) {
        let a: Vec<u16> = arg.encode_wide().collect();
        let plain = !a.is_empty()
            && !a
                .iter()
                .any(|&c| c == ' ' as u16 || c == '\t' as u16 || c == '"' as u16);
        if !out.is_empty() {
            out.push(' ' as u16);
        }
        if plain {
            out.extend_from_slice(&a);
            return;
        }
        out.push('"' as u16);
        let mut backslashes = 0usize;
        for &c in &a {
            if c == '\\' as u16 {
                backslashes += 1;
                continue;
            }
            if c == '"' as u16 {
                out.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
            } else {
                out.extend(std::iter::repeat_n('\\' as u16, backslashes));
            }
            backslashes = 0;
            out.push(c);
        }
        out.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
        out.push('"' as u16);
    }

    pub(super) fn spawn(exe: &Path, args: &[&OsStr], cwd: Option<&Path>) -> io::Result<Detached> {
        let app = wide(exe.as_os_str());
        let mut line = Vec::new();
        push_arg(&mut line, exe.as_os_str());
        for a in args {
            push_arg(&mut line, a);
        }
        line.push(0);
        let cwd = cwd.map(|d| wide(d.as_os_str()));
        // SAFETY: every handle and buffer below is owned by this function,
        // outlives the calls that use it, and is released on all paths.
        unsafe {
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 1,
            };
            let nul_name = wide(OsStr::new("NUL"));
            let nul: HANDLE = CreateFileW(
                nul_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                0,
                null_mut(),
            );
            if nul == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0usize;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
            let mut buf = vec![0u64; size.div_ceil(8)];
            let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
                let e = io::Error::last_os_error();
                CloseHandle(nul);
                return Err(e);
            }
            let handles = [nul];
            let result = (|| {
                if UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(&handles),
                    null_mut(),
                    null(),
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let mut si: STARTUPINFOEXW = std::mem::zeroed();
                si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = nul;
                si.StartupInfo.hStdOutput = nul;
                si.StartupInfo.hStdError = nul;
                si.lpAttributeList = list;
                let base = DETACHED_PROCESS
                    | CREATE_NEW_PROCESS_GROUP
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT;
                let mut last = None;
                // Breakaway is refused inside some job objects; detach anyway.
                for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
                    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
                    let mut cmdline = line.clone();
                    let ok = CreateProcessW(
                        app.as_ptr(),
                        cmdline.as_mut_ptr(),
                        null(),
                        null(),
                        1,
                        flags,
                        null(),
                        cwd.as_ref().map_or(null(), |c| c.as_ptr()),
                        &si.StartupInfo,
                        &mut pi,
                    );
                    if ok != 0 {
                        CloseHandle(pi.hThread);
                        return Ok(Detached {
                            pid: pi.dwProcessId,
                            process: OwnedHandle::from_raw_handle(pi.hProcess),
                        });
                    }
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
                        return Err(e);
                    }
                    last = Some(e);
                }
                Err(last.unwrap_or_else(|| io::Error::other("process not started")))
            })();
            DeleteProcThreadAttributeList(list);
            CloseHandle(nul);
            result
        }
    }
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use std::time::{Duration, Instant};

    fn quoted(args: &[&str]) -> String {
        let mut out = Vec::new();
        for a in args {
            win_spawn::push_arg(&mut out, OsStr::new(a));
        }
        String::from_utf16(&out).unwrap()
    }

    #[test]
    fn arguments_are_quoted_as_windows_reads_them() {
        assert_eq!(
            quoted(&["a.exe", "--data-root", r"C:\x"]),
            r"a.exe --data-root C:\x"
        );
        assert_eq!(
            quoted(&[r"C:\Program Files\w.exe"]),
            r#""C:\Program Files\w.exe""#
        );
        assert_eq!(quoted(&[""]), r#""""#);
        assert_eq!(quoted(&[r"C:\my dir\"]), r#""C:\my dir\\""#);
        assert_eq!(quoted(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(quoted(&[r#"a\"b c"#]), r#""a\\\"b c""#);
    }

    fn system32(exe: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or(r"C:\Windows".into()))
            .join("System32")
            .join(exe)
    }

    fn kill(pid: u32) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        // SAFETY: a handle this function owns and closes.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    /// An inheritable pipe: (reader, raw write handle).
    fn inheritable_pipe() -> (std::fs::File, windows_sys::Win32::Foundation::HANDLE) {
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Pipes::CreatePipe;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: out-pointers to locals; the read end moves into a File.
        unsafe {
            assert_ne!(CreatePipe(&mut r, &mut w, &sa, 0), 0);
            (std::fs::File::from_raw_handle(r), w)
        }
    }

    /// Whether the pipe's reader reaches end-of-file within `wait`, i.e.
    /// nobody holds the write end any more.
    fn eof_within(mut reader: std::fs::File, wait: Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = reader.read_to_end(&mut b);
            let _ = tx.send(());
        });
        rx.recv_timeout(wait).is_ok()
    }

    /// The regression the handle list closes: a detached child must not
    /// keep an inheritable handle of its starter. The control shows the test
    /// sees it: the same child started through `Command` keeps the pipe.
    #[test]
    fn a_detached_child_inherits_no_handle_of_its_starter() {
        let ping = system32("PING.EXE");
        let args = [OsStr::new("-n"), OsStr::new("30"), OsStr::new("127.0.0.1")];

        // Control: std's Command (inheritance on) hands the pipe down.
        let (reader, w) = inheritable_pipe();
        let mut control = std::process::Command::new(&ping)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // SAFETY: closing our own write end.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(w) };
        let control_eof = eof_within(reader, Duration::from_secs(2));
        let _ = control.kill();
        let _ = control.wait();
        assert!(
            !control_eof,
            "control: Command did not hand the pipe down; the test proves nothing"
        );

        // Detached through the handle list: the pipe is not handed down.
        let (reader, w) = inheritable_pipe();
        let started = Instant::now();
        let mut child = spawn_detached_program(&ping, &args).unwrap();
        // SAFETY: closing our own write end.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(w) };
        let eof = eof_within(reader, Duration::from_secs(5));
        assert_eq!(
            child.try_wait().unwrap(),
            None,
            "the child should still run"
        );
        kill(child.id());
        assert!(
            eof,
            "the detached child kept the starter's pipe open ({:?})",
            started.elapsed()
        );
    }

    #[test]
    fn the_exit_code_is_reported_and_the_folder_is_the_programs() {
        let cmd = system32("cmd.exe");
        let run = |line: &str| {
            let mut child =
                spawn_detached_program(&cmd, &[OsStr::new("/c"), OsStr::new(line)]).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(c) = child.try_wait().unwrap() {
                    break c;
                }
                assert!(Instant::now() < deadline, "cmd did not end");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        assert_eq!(run("exit 7"), 7);
        // Run in the program's folder (System32), not this test's: there a
        // relative `cmd.exe` exists.
        assert!(!std::path::Path::new("cmd.exe").exists());
        assert_eq!(run("if exist cmd.exe (exit 5) else (exit 6)"), 5);
    }
}
