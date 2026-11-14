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
    let total = physical_memory();
    let budget = (total / 2).max(MIN);
    if mb > 0 {
        return usize::try_from(mb.max(256).saturating_mul(MIB as u64).min(budget))
            .unwrap_or(usize::MAX);
    }
    usize::try_from((total / 4).max(MIN).min(budget)).unwrap_or(usize::MAX)
}

/// Conservative aggregate admission: all simultaneously active run trees
/// fit within half physical RAM (2 GiB minimum). Reserve the remainder for
/// the host, query/SQLite readers and the desktop. Existing jobs drain when
/// settings lower the limit; new jobs wait rather than oversubscribe it.
pub fn parallel_jobs(requested: usize, native_mb: u64, worker_mb: u64) -> usize {
    parallel_for_budget(
        requested,
        cap_bytes(native_mb).max(cap_bytes(worker_mb)),
        (physical_memory() / 2).max(2 << 30) as usize,
    )
}

fn parallel_for_budget(requested: usize, per_run: usize, budget: usize) -> usize {
    requested.clamp(1, 8).min((budget / per_run.max(1)).max(1))
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

/// Unix: pages times page size (`sysconf`, on Linux and macOS alike). It
/// returned 0 here before, which left the 2 GiB floor as the budget: one
/// job at a time, so a slow project held up every other.
#[cfg(unix)]
fn physical_memory() -> u64 {
    // SAFETY: sysconf has no preconditions; it returns -1 when unknown.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if pages <= 0 || size <= 0 {
        return 0;
    }
    (pages as u64).saturating_mul(size as u64)
}

#[cfg(not(any(windows, unix)))]
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

/// How often a native child on Unix checks that the service still lives.
#[cfg(unix)]
const PARENT_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Unix: no memory cap yet (WI-ADR-024 records it), but a native child must
/// not outlive the service. On Windows the job object kills it with the
/// service; here a watchdog thread notices the service gone (this process
/// re-parented) and kills this child's own process group (itself and its
/// parser helpers) at once, so nothing is published after the ownership
/// lock died with the service (INV-01). The service's pid comes from
/// `WRONGINDEX_SERVICE_PID` (set by the worker, which also starts the child
/// in a process group of its own), so a service that died
/// before this ran is noticed too. Returns the cap: 0, none.
#[cfg(unix)]
pub fn confine_current_process() -> Result<usize, String> {
    // SAFETY: getppid has no preconditions.
    let parent = std::env::var("WRONGINDEX_SERVICE_PID")
        .ok()
        .and_then(|p| p.trim().parse::<libc::pid_t>().ok())
        .unwrap_or_else(|| unsafe { libc::getppid() });
    let gone = move || unsafe { libc::getppid() } != parent;
    if gone() {
        return Err(format!("the service (pid {parent}) is gone"));
    }
    std::thread::Builder::new()
        .name("wrongindex-service-watch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(PARENT_POLL);
                if gone() {
                    // SAFETY: signals to this process's own group (it leads
                    // it: the worker starts it in a new one) and an
                    // immediate exit; nothing here touches shared state.
                    unsafe {
                        if libc::getpgrp() == libc::getpid() {
                            libc::kill(-libc::getpid(), libc::SIGKILL);
                        }
                        libc::_exit(70);
                    }
                }
            }
        })
        .map_err(|e| format!("cannot start the service watch: {e}"))?;
    Ok(0)
}

#[cfg(not(any(windows, unix)))]
pub fn confine_current_process() -> Result<usize, String> {
    Err("process confinement is implemented for Windows and Unix only".into())
}

/// Stop `child` and, on Unix, the process group it leads (it was started
/// in one of its own), then reap it. The group is signalled only while the
/// child is not reaped yet (`id()` turns `None` once it is), so its id
/// cannot have passed to anyone else. On Windows the job object ends the
/// tree.
pub async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        kill_group(pid);
    }
    let _ = child.kill().await;
}

/// `SIGKILL` to the process group `leader` leads. The caller makes sure the
/// leader is not reaped yet.
#[cfg(unix)]
pub fn kill_group(leader: u32) {
    if let Ok(pid) = libc::pid_t::try_from(leader) {
        // SAFETY: a signal to a process group; no memory is involved.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(windows))]
pub fn kill_children_with_me() -> Result<(), String> {
    Ok(())
}

/// Unix: a job watches the process group its tree runs in (the child leads
/// it) from outside. Every [`WATCH_EVERY`] it sums the group's resident
/// memory; over the cap, it notes the hit and kills the group. Windows caps
/// committed memory instead (the allocation fails); here the tree is
/// stopped once it is over. The cap may change while it runs.
#[cfg(unix)]
pub struct Job {
    state: std::sync::Arc<JobState>,
}

#[cfg(unix)]
#[derive(Default)]
struct JobState {
    cap: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    hit: std::sync::atomic::AtomicBool,
    done: std::sync::atomic::AtomicBool,
}

/// How often a job looks at its tree's memory.
#[cfg(unix)]
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(unix)]
impl Job {
    /// Watch the group `leader` leads, capped at `cap` bytes (0: no cap).
    /// The watch ends when the group is gone or the job is dropped.
    pub fn watch(leader: u32, cap: usize) -> Job {
        use std::sync::atomic::Ordering;
        let state = std::sync::Arc::new(JobState::default());
        state.cap.store(cap, Ordering::Relaxed);
        let s = state.clone();
        let _ = std::thread::Builder::new()
            .name("wrongindex-memory-watch".into())
            .spawn(move || {
                while !s.done.load(Ordering::Relaxed) {
                    let Some((members, bytes)) = group_memory(leader) else {
                        return;
                    };
                    s.peak.fetch_max(bytes, Ordering::Relaxed);
                    let cap = s.cap.load(Ordering::Relaxed);
                    // Killed only while the leader is still listed in its
                    // group: then the group id is still this tree's.
                    if cap > 0 && bytes > cap && members.contains(&leader) {
                        s.hit.store(true, Ordering::Relaxed);
                        kill_group(leader);
                        return;
                    }
                    std::thread::sleep(WATCH_EVERY);
                }
            });
        Job { state }
    }

    pub fn cap(&self) -> usize {
        self.state.cap.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Change the cap; the watch applies it at its next look.
    pub fn set_cap(&self, cap: usize) -> Result<(), String> {
        self.state
            .cap
            .store(cap, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Most resident memory the tree has had at one look, in bytes.
    pub fn peak(&self) -> usize {
        self.state.peak.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the cap stopped the tree.
    pub fn cap_hit(&self) -> bool {
        self.state.hit.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(unix)]
impl Drop for Job {
    fn drop(&mut self) {
        self.state
            .done
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The processes in the group `leader` leads and their resident memory in
/// bytes; `None` when the group has no process left or cannot be read.
/// Linux reads `/proc/<pid>/stat` (`pgrp`, `rss` in pages).
#[cfg(target_os = "linux")]
fn group_memory(leader: u32) -> Option<(Vec<u32>, usize)> {
    // SAFETY: sysconf has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|p| *p > 0)?;
    let (mut members, mut bytes) = (Vec::new(), 0usize);
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // After the command name (in parentheses, which may hold anything):
        // state, ppid, pgrp, ...; rss is the 22nd field from there.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.get(2).and_then(|g| g.parse::<u32>().ok()) != Some(leader) {
            continue;
        }
        members.push(pid);
        let rss: usize = fields.get(21).and_then(|r| r.parse().ok()).unwrap_or(0);
        bytes = bytes.saturating_add(rss.saturating_mul(page));
    }
    (!members.is_empty()).then_some((members, bytes))
}

/// macOS: `ps -A -o pid=,pgid=,rss=` (rss in KiB).
#[cfg(target_os = "macos")]
fn group_memory(leader: u32) -> Option<(Vec<u32>, usize)> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,pgid=,rss="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let (mut members, mut bytes) = (Vec::new(), 0usize);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split_whitespace().map(|x| x.parse::<u64>().ok());
        if let (Some(Some(pid)), Some(Some(pgid)), Some(Some(kib))) = (f.next(), f.next(), f.next())
            && pgid == u64::from(leader)
            && let Ok(pid) = u32::try_from(pid)
        {
            members.push(pid);
            let b = usize::try_from(kib.saturating_mul(1024)).unwrap_or(usize::MAX);
            bytes = bytes.saturating_add(b);
        }
    }
    (!members.is_empty()).then_some((members, bytes))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn group_memory(_leader: u32) -> Option<(Vec<u32>, usize)> {
    None
}

/// Unix: the tree runs in a process group the child leads, watched by a
/// [`Job`].
#[cfg(unix)]
pub fn spawn_confined(
    cmd: &mut tokio::process::Command,
    cap: usize,
) -> std::io::Result<(tokio::process::Child, Job)> {
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let leader = child
        .id()
        .ok_or_else(|| std::io::Error::other("the process ended before it was watched"))?;
    Ok((child, Job::watch(leader, cap)))
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
        // Known on every platform the service runs on (0 meant one job slot).
        #[cfg(any(windows, unix))]
        assert!(physical_memory() > 1 << 30);
    }

    #[test]
    fn aggregate_admission_reserves_memory_for_the_host() {
        assert_eq!(parallel_for_budget(8, 4 << 30, 8 << 30), 2);
        assert_eq!(parallel_for_budget(8, 512 << 20, 8 << 30), 8);
        assert_eq!(parallel_for_budget(0, 4 << 30, 8 << 30), 1);
        assert_eq!(parallel_for_budget(8, 4 << 30, 2 << 30), 1);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        // A zombie is not alive: `ps` shows it as `Z`.
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    /// Killing a child that leads its own group takes its own children too;
    /// `kill` alone leaves them running.
    #[tokio::test]
    async fn kill_tree_takes_the_whole_group() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 600 & echo $!; wait"])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut out = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut out, &mut line)
            .await
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        assert!(alive(grandchild));
        kill_tree(&mut child).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(grandchild) {
            assert!(
                std::time::Instant::now() < deadline,
                "the group's other process survived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(child.id().is_none(), "reaped");
    }

    /// A tree over its cap is stopped and the hit noted; without a cap it
    /// runs on, and the peak is what it used.
    #[tokio::test]
    async fn a_tree_over_its_cap_is_stopped() {
        // bash keeps a 48 MB string in memory while it waits. `x` is used
        // after the wait: bash runs a `-c` script's last command with `exec`,
        // which would replace bash (and free the string) with `sleep`; the
        // first version of this test saw only the spike while it was built.
        let hog = |cap: usize| {
            let child = tokio::process::Command::new("bash")
                .args(["-c", "printf -v x '%*s' 48000000 ''; sleep 60; echo ${#x}"])
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let job = Job::watch(child.id().unwrap(), cap);
            (child, job)
        };
        let (mut over, job) = hog(16 << 20);
        let status = tokio::time::timeout(std::time::Duration::from_secs(20), over.wait())
            .await
            .expect("the tree over its cap was not stopped")
            .unwrap();
        assert!(!status.success());
        assert!(job.cap_hit());
        assert!(job.peak() > 16 << 20, "{}", job.peak());

        let (mut under, job) = hog(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while job.peak() < 40 << 20 {
            assert!(std::time::Instant::now() < deadline, "{}", job.peak());
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(under.try_wait().unwrap().is_none(), "no cap: still running");
        assert!(!job.cap_hit());
        kill_tree(&mut under).await;
    }

    #[test]
    fn a_missing_group_has_no_memory() {
        assert!(group_memory(u32::MAX - 7).is_none());
    }
}
