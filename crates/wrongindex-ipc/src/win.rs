//! Windows named-pipe transport and owner-private security descriptors.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// String SID of the user that owns this process token.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: standard token query sequence; every handle/buffer is checked
    // and released on all paths.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                return Err(err);
            }
            // u64 backing keeps the TOKEN_USER pointer suitably aligned.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str: *mut u16 = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut n = 0usize;
            while *sid_str.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, n));
            LocalFree(sid_str.cast());
            Ok(s)
        })();
        CloseHandle(token);
        result
    }
}

/// A self-relative security descriptor allocated by
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
pub struct OwnerOnlySecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is immutable after creation and only read by the OS.
unsafe impl Send for OwnerOnlySecurity {}
unsafe impl Sync for OwnerOnlySecurity {}

impl OwnerOnlySecurity {
    /// Protected DACL granting full access to the current user and
    /// LocalSystem only. `inherit` adds object/container inheritance for
    /// directories.
    pub fn new(inherit: bool) -> io::Result<Self> {
        let sid = current_user_sid()?;
        let flags = if inherit { "OICI" } else { "" };
        let sddl = wide(&format!("D:P(A;{flags};GA;;;{sid})(A;{flags};GA;;;SY)"));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: valid NUL-terminated SDDL; output freed in Drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        // SAFETY: allocated with LocalAlloc by the conversion API.
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let sec = OwnerOnlySecurity::new(true)?;
    let attrs = sec.attributes();
    let path = wide_path(dir);
    // SAFETY: valid path and attributes for the duration of the call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attrs) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Creates pipe instances that only the current user / LocalSystem can open.
pub struct PipeListener {
    name: String,
    security: OwnerOnlySecurity,
}

impl PipeListener {
    /// Bind the first instance. Fails if another process already owns the
    /// name: `FILE_FLAG_FIRST_PIPE_INSTANCE` is the OS-backed singleton guard.
    pub fn bind(name: &str) -> io::Result<(Self, NamedPipeServer)> {
        let security = OwnerOnlySecurity::new(false)?;
        let listener = Self {
            name: name.to_string(),
            security,
        };
        let first = listener.create(true)?;
        Ok((listener, first))
    }

    /// Create the next instance to accept a further client.
    pub fn next(&self) -> io::Result<NamedPipeServer> {
        self.create(false)
    }

    fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
        let mut attrs = self.security.attributes();
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `attrs` points at a live SECURITY_ATTRIBUTES whose descriptor
        // outlives the call.
        unsafe {
            opts.create_with_security_attributes_raw(
                &self.name,
                (&mut attrs as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }
}

/// Connect to a pipe, retrying while all instances are busy.
pub async fn connect(name: &str, deadline: Duration) -> io::Result<NamedPipeClient> {
    let start = Instant::now();
    loop {
        match ClientOptions::new().open(name) {
            Ok(c) => return Ok(c),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                if start.elapsed() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "all pipe instances busy",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_looks_like_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
    }

    #[tokio::test]
    async fn second_bind_of_same_name_fails() {
        let name = format!(r"\\.\pipe\wrongindex-test-{}", std::process::id());
        let (_l, _first) = PipeListener::bind(&name).unwrap();
        let err = PipeListener::bind(&name)
            .err()
            .expect("second bind must fail");
        // ERROR_ACCESS_DENIED for FILE_FLAG_FIRST_PIPE_INSTANCE on an existing name.
        assert_eq!(err.raw_os_error(), Some(5), "{err}");
    }
}
