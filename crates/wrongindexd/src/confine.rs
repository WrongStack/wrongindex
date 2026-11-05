//! Resource confinement of the processes this service starts (security
//! review WI-604, WI-301).
//!
//! The isolated native indexer child puts itself, before it starts any
//! parser process, into a fresh Windows Job Object; the compatibility
//! worker (WrongStack's own engine under Bun) is started suspended and put
//! into one by the service before it runs a single instruction. Either job
//! - kills every process in it when its last handle closes: the handle is
//!   private (not inheritable), so when the daemon kills the child or the
//!   worker (deadline, cancellation, restart, daemon exit) or it crashes,
//!   the processes it started die with it;
//! - caps the committed memory of the whole tree, so a hostile repository
//!   that inflates parser output cannot exhaust the machine. Allocation
//!   failure aborts the process, which the daemon reports as a failed run.

/// One mebibyte.
const MIB: usize = 1 << 20;

/// A memory setting in MiB as a cap in bytes: 0 = automatic (a quarter of
/// physical memory, at least 2 GiB: two indexing runs may be under way at
/// once, see the `parallelJobs` setting), else at least 256 MiB.
pub fn cap_bytes(mb: u64) -> usize {
    const MIN: u64 = 2 << 30;
    if mb > 0 {
        return usize::try_from(mb.max(256))
            .unwrap_or(usize::MAX)
            .saturating_mul(MIB);
    }
    usize::try_from((physical_memory() / 4).max(MIN)).unwrap_or(usize::MAX)
}

/// Bytes of physical memory (0 when that cannot be told).
#[cfg(windows)]
fn physical_memory() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain data; dwLength set as required.
    unsafe {
        let mut s: MEMORYSTATUSEX = std::mem::zeroed();
        s.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut s) != 0 {
            s.ullTotalPhys
        } else {
            0
        }
    }
}

#[cfg(not(windows))]
fn physical_memory() -> u64 {
    0
}

/// Memory cap for the native child's whole tree: `WRONGINDEX_NATIVE_MEMORY_MB`
/// (the `nativeMemoryMb` setting, passed by the daemon), else automatic.
#[cfg(windows)]
fn memory_cap() -> usize {
    cap_bytes(
        std::env::var("WRONGINDEX_NATIVE_MEMORY_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0),
    )
}

/// Confine the current process (and everything it starts afterwards).
/// Returns the cap in bytes.
#[cfg(windows)]
pub fn confine_current_process() -> Result<usize, String> {
    let cap = memory_cap();
    join_new_job(Some(cap))?;
    Ok(cap)
}

/// The daemon: every process it starts (native children, the WrongStack
/// worker, git) dies with it. Otherwise a native child could keep writing
/// and publish `index.db` after the daemon died and its ownership lock was
/// released (INV-01).
#[cfg(windows)]
pub fn kill_children_with_me() -> Result<(), String> {
    join_new_job(None)
}

/// Kill-on-close, and with `memory` a cap on the job's committed memory
/// (an allocation over it fails; a process that does not handle that dies
/// without an error dialog).
#[cfg(windows)]
fn set_limits(
    job: windows_sys::Win32::Foundation::HANDLE,
    memory: Option<usize>,
) -> Result<(), String> {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    // SAFETY: the limit struct is plain data and outlives the call; `job`
    // is a job handle the caller owns.
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(cap) = memory {
            info.BasicLimitInformation.LimitFlags |=
                JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            info.JobMemoryLimit = cap;
        }
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(format!(
                "job object limits: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Put the current process into a new kill-on-close Job Object (nested
/// under any job it is already in). The handle is deliberately leaked: it
/// must live exactly as long as this process.
#[cfg(windows)]
fn join_new_job(memory: Option<usize>) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls on a handle this function owns.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!(
                "CreateJobObjectW: {}",
                std::io::Error::last_os_error()
            ));
        }
        if let Err(e) = set_limits(job, memory) {
            CloseHandle(job);
            return Err(e);
        }
        if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(format!("job object: {e}"));
        }
    }
    Ok(())
}

/// The job of a process tree this service started and confines from
/// outside (the compatibility worker). Dropping it ends the whole tree.
#[cfg(windows)]
pub struct Job {
    job: windows_sys::Win32::Foundation::HANDLE,
    /// Completion port the job posts its notifications to.
    port: windows_sys::Win32::Foundation::HANDLE,
    cap: std::sync::atomic::AtomicUsize,
    /// The cap stopped an allocation (sticky).
    hit: std::sync::atomic::AtomicBool,
}

// SAFETY: kernel object handles may be used from any thread; the handles
// are closed only in `drop`.
#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
unsafe impl Sync for Job {}

/// `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT`: a process in the job tried to commit
/// more than the job-wide cap.
#[cfg(windows)]
const MSG_JOB_MEMORY_LIMIT: u32 = 10;

#[cfg(windows)]
impl Job {
    /// A kill-on-close job capped at `cap` bytes, whose notifications are
    /// kept for [`Job::cap_hit`].
    pub fn new(cap: usize) -> Result<Self, String> {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::IO::CreateIoCompletionPort;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
            JobObjectAssociateCompletionPortInformation, SetInformationJobObject,
        };
        // SAFETY: handles this function creates; on any failure the ones
        // made so far are closed (by `Job`'s drop once it exists). The
        // association struct is plain data and outlives the call.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(format!(
                    "CreateJobObjectW: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
            if port.is_null() {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(format!("CreateIoCompletionPort: {e}"));
            }
            let me = Self {
                job,
                port,
                cap: AtomicUsize::new(cap),
                hit: AtomicBool::new(false),
            };
            set_limits(job, Some(cap))?;
            let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: std::ptr::null_mut(),
                CompletionPort: port,
            };
            if SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                (&raw const assoc).cast(),
                std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            ) == 0
            {
                return Err(format!(
                    "job notifications: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(me)
        }
    }

    /// The cap in bytes.
    pub fn cap(&self) -> usize {
        self.cap.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Change the cap of the running tree. A cap below what it holds now
    /// fails its next allocation.
    pub fn set_cap(&self, cap: usize) -> Result<(), String> {
        set_limits(self.job, Some(cap))?;
        self.cap.store(cap, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn assign(&self, process: windows_sys::Win32::Foundation::HANDLE) -> Result<(), String> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
            return Err(format!(
                "AssignProcessToJobObject: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Most memory the tree has had committed at once, in bytes.
    pub fn peak(&self) -> usize {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };
        // SAFETY: the struct is plain data sized as passed.
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            if QueryInformationJobObject(
                self.job,
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            ) == 0
            {
                return 0;
            }
            info.PeakJobMemoryUsed
        }
    }

    /// Whether the cap has stopped an allocation in this tree (reads the
    /// job's notifications without waiting).
    pub fn cap_hit(&self) -> bool {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::System::IO::{GetQueuedCompletionStatus, OVERLAPPED};
        // SAFETY: out-parameters are locals; a zero timeout never blocks.
        unsafe {
            loop {
                let (mut message, mut key, mut detail) =
                    (0u32, 0usize, std::ptr::null_mut::<OVERLAPPED>());
                if GetQueuedCompletionStatus(self.port, &mut message, &mut key, &mut detail, 0) == 0
                {
                    break;
                }
                if message == MSG_JOB_MEMORY_LIMIT {
                    self.hit.store(true, Ordering::Relaxed);
                }
            }
        }
        self.hit.load(Ordering::Relaxed)
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: handles owned by this value, closed once. Closing the job
        // ends every process still in it (kill-on-close).
        unsafe {
            CloseHandle(self.job);
            CloseHandle(self.port);
        }
    }
}

/// Start `cmd` (without a console window) inside a new [`Job`] capped at
/// `cap` bytes. The process is created suspended and only resumed once it
/// is in the job, so nothing it starts escapes the job and no allocation
/// escapes the cap.
#[cfg(windows)]
pub fn spawn_confined(
    cmd: &mut tokio::process::Command,
    cap: usize,
) -> std::io::Result<(tokio::process::Child, Job)> {
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let job = Job::new(cap).map_err(std::io::Error::other)?;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = cmd.spawn()?;
    let started = match (child.raw_handle(), child.id()) {
        (Some(handle), Some(pid)) => job.assign(handle).and_then(|()| resume(pid)),
        _ => Err("the process ended before it was confined".to_string()),
    };
    if let Err(e) = started {
        let _ = child.start_kill();
        return Err(std::io::Error::other(e));
    }
    Ok((child, job))
}

/// Resume the threads of a process created suspended (its main thread).
#[cfg(windows)]
fn resume(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    let mut resumed = 0;
    // SAFETY: a snapshot and thread handles this function opens and closes;
    // THREADENTRY32 is plain data with dwSize set as required.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(format!(
                "thread snapshot: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut t: THREADENTRY32 = std::mem::zeroed();
        t.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snap, &mut t) != 0;
        while more {
            if t.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, t.th32ThreadID);
                if !thread.is_null() {
                    if ResumeThread(thread) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snap, &mut t) != 0;
        }
        CloseHandle(snap);
    }
    if resumed == 0 {
        return Err(format!("could not resume process {pid}"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn confine_current_process() -> Result<usize, String> {
    Err("process confinement is implemented for Windows only".into())
}

#[cfg(not(windows))]
pub fn kill_children_with_me() -> Result<(), String> {
    Ok(())
}

/// Elsewhere a job only remembers its cap; nothing is confined.
#[cfg(not(windows))]
pub struct Job {
    cap: std::sync::atomic::AtomicUsize,
}

#[cfg(not(windows))]
impl Job {
    pub fn cap(&self) -> usize {
        self.cap.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_cap(&self, cap: usize) -> Result<(), String> {
        self.cap.store(cap, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    pub fn peak(&self) -> usize {
        0
    }
    pub fn cap_hit(&self) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub fn spawn_confined(
    cmd: &mut tokio::process::Command,
    cap: usize,
) -> std::io::Result<(tokio::process::Child, Job)> {
    Ok((
        cmd.spawn()?,
        Job {
            cap: std::sync::atomic::AtomicUsize::new(cap),
        },
    ))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Run by [`a_tree_over_its_cap_is_stopped_and_reported`] in a child
    /// process: commit `WRONGINDEX_TEST_HOG_MB` MiB once a line arrives on
    /// stdin, then wait for stdin to close.
    #[test]
    #[ignore = "helper process for the confinement tests"]
    fn hog() {
        let Some(mb) = std::env::var("WRONGINDEX_TEST_HOG_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        else {
            return;
        };
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        let mut v = vec![1u8; mb * MIB];
        v[mb * MIB - 1] = 2;
        println!("held {}", v.len());
        std::io::stdin().read_line(&mut line).unwrap();
        std::hint::black_box(&v);
    }

    async fn hog_child(mb: usize, cap: usize) -> (tokio::process::Child, Job) {
        let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "confine::tests::hog", "--ignored", "--nocapture"])
            .env("WRONGINDEX_TEST_HOG_MB", mb.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        spawn_confined(&mut cmd, cap).unwrap()
    }

    async fn go(child: &mut tokio::process::Child) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"go\n").await.unwrap();
        // Closing stdin lets a hog that got its memory end.
        drop(stdin);
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .await
            .unwrap();
        out
    }

    #[tokio::test]
    async fn a_tree_over_its_cap_is_stopped_and_reported() {
        // Under the cap: it gets its memory and ends normally.
        let (mut child, job) = hog_child(300, 1024 * MIB).await;
        assert!(go(&mut child).await.contains("held"));
        assert!(child.wait().await.unwrap().success());
        assert!(!job.cap_hit());
        assert!(job.peak() >= 300 * MIB, "{}", job.peak());

        // Over it: the allocation fails, the process dies, the job says why.
        let (mut child, job) = hog_child(600, 400 * MIB).await;
        assert!(!go(&mut child).await.contains("held"));
        assert!(!child.wait().await.unwrap().success());
        assert!(job.cap_hit());

        // Lowered while it runs: the next allocation is over it.
        let (mut child, job) = hog_child(600, 1024 * MIB).await;
        job.set_cap(400 * MIB).unwrap();
        assert_eq!(job.cap(), 400 * MIB);
        assert!(!go(&mut child).await.contains("held"));
        assert!(!child.wait().await.unwrap().success());
        assert!(job.cap_hit());
    }

    #[test]
    fn settings_map_to_caps() {
        assert_eq!(cap_bytes(512), 512 * MIB);
        assert_eq!(cap_bytes(100), 256 * MIB);
        let auto = cap_bytes(0);
        assert!(auto >= 2 << 30);
        assert!(auto as u64 >= physical_memory() / 4);
    }
}
