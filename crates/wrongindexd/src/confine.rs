//! Resource confinement of the isolated native indexer child (security
//! review WI-604).
//!
//! The child puts itself, before it starts any parser process, into a fresh
//! Windows Job Object that
//! - kills every process in it when its last handle closes: the handle is
//!   private to the child (not inheritable), so when the daemon kills the
//!   child (deadline, cancellation, daemon exit) or it crashes, the Python,
//!   Go and Bun/Node parser processes it started die with it;
//! - caps the committed memory of the whole tree, so a hostile repository
//!   that inflates parser output cannot exhaust the machine. Allocation
//!   failure aborts the child, which the daemon reports as a failed run.

/// Memory cap for the whole child tree: `WRONGINDEX_NATIVE_MEMORY_MB` (at
/// least 256), else half of physical memory, at least 2 GiB.
#[cfg(windows)]
fn memory_cap() -> usize {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    const MIN: u64 = 2 << 30;
    if let Some(mb) = std::env::var("WRONGINDEX_NATIVE_MEMORY_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return mb.max(256).saturating_mul(1 << 20);
    }
    // SAFETY: MEMORYSTATUSEX is plain data; dwLength set as required.
    let total = unsafe {
        let mut s: MEMORYSTATUSEX = std::mem::zeroed();
        s.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut s) != 0 {
            s.ullTotalPhys
        } else {
            0
        }
    };
    usize::try_from((total / 2).max(MIN)).unwrap_or(usize::MAX)
}

/// Confine the current process (and everything it starts afterwards).
/// Returns the cap in bytes. The job handle is deliberately leaked: it must
/// live exactly as long as this process.
#[cfg(windows)]
pub fn confine_current_process() -> Result<usize, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let cap = memory_cap();
    // SAFETY: plain Win32 calls on a handle this function owns; the limit
    // struct is plain data and outlives the call.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!(
                "CreateJobObjectW: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        info.JobMemoryLimit = cap;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
            || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
        {
            let e = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(format!("job object: {e}"));
        }
    }
    Ok(cap)
}

#[cfg(not(windows))]
pub fn confine_current_process() -> Result<usize, String> {
    Err("process confinement is implemented for Windows only".into())
}
