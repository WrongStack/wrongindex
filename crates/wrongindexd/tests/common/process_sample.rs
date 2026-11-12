/// (working set bytes, handles, threads) of a process.
#[cfg(windows)]
pub fn sample(pid: u32) -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: each successfully opened handle has one owner here.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: read-only process access; sized output buffers outlive every
    // call, and both handles are closed on every return path. No child
    // process or shell startup can block the observer's edit loop.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let process = Handle(process);
        let mut memory = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let mut handles = 0;
        if K32GetProcessMemoryInfo(process.0, &mut memory, memory.cb) == 0
            || GetProcessHandleCount(process.0, &mut handles) == 0
        {
            return None;
        }
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = Handle(snapshot);
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut more = Thread32First(snapshot.0, &mut entry) != 0;
        let mut threads = 0u64;
        while more {
            if entry.th32OwnerProcessID == pid {
                threads += 1;
            }
            more = Thread32Next(snapshot.0, &mut entry) != 0;
        }
        (threads > 0).then_some((memory.WorkingSetSize as u64, handles as u64, threads))
    }
}

#[cfg(not(windows))]
pub fn sample(_pid: u32) -> Option<(u64, u64, u64)> {
    None // This Windows soak has no additional-platform acceptance claim.
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn samples_without_a_shell() {
        const CHILD: &str = "WRONGINDEX_SAMPLE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let (working_set, handles, threads) =
                super::sample(std::process::id()).expect("sampling requires an external shell");
            assert!(working_set > 0 && handles > 0 && threads > 0);
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
            let count = || {
                let mut n = 0;
                // SAFETY: the pseudo-handle is borrowed, and the output is valid.
                assert_ne!(
                    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) },
                    0
                );
                n
            };
            let before = count();
            let started = std::time::Instant::now();
            for _ in 0..16 {
                assert!(super::sample(std::process::id()).is_some());
            }
            assert!(count() <= before + 2, "the observer leaks handles");
            println!("16 native samples took {:?}", started.elapsed());
            assert!(super::sample(0).is_none());
            assert!(super::sample(u32::MAX).is_none());
            return;
        }
        let empty_path = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_sample::tests::samples_without_a_shell",
                "--nocapture",
            ])
            .env("PATH", empty_path.path())
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("sampler child exceeded its deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let child = child.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&child.stdout);
        println!("{out}");
        assert!(
            child.status.success() && out.contains("1 passed; 0 failed"),
            "sampler child failed: {out}\n{}",
            String::from_utf8_lossy(&child.stderr)
        );
    }
}
