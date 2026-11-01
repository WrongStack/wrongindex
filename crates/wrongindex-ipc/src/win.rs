//! Windows named-pipe transport and owner-private security descriptors.
//!
//! Trust model (architecture §10): same-user local access. Two things are
//! enforced here beyond the pipe DACL:
//!
//! * A client verifies that the process serving the pipe runs as the current
//!   user before it sends any credential, so a pipe name pre-created by another
//!   local account (squatting) never receives the token.
//! * Private directories and the token file carry an explicit protected DACL
//!   (current user + LocalSystem). A pre-existing directory owned by another
//!   account is refused rather than trusted.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Convert a SID to its string form.
///
/// # Safety
/// `sid` must point at a valid SID.
unsafe fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut sid_str: *mut u16 = null_mut();
    // SAFETY: caller guarantees a valid SID; output freed below.
    unsafe {
        if ConvertSidToStringSidW(sid, &mut sid_str) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut n = 0usize;
        while *sid_str.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, n));
        LocalFree(sid_str.cast());
        Ok(s)
    }
}

/// String SID of the user an access token belongs to.
///
/// # Safety
/// `token` must be a valid token handle opened with `TOKEN_QUERY`.
unsafe fn token_user_sid(token: HANDLE) -> io::Result<String> {
    // SAFETY: standard two-call size query; buffer is u64-aligned for TOKEN_USER.
    unsafe {
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(err);
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_to_string(user.User.Sid)
    }
}

/// Default owner SID the token stamps on objects it creates. For a normal
/// user this is the user SID; for an elevated administrator it is usually
/// BUILTIN\Administrators.
///
/// # Safety
/// `token` must be a valid token handle opened with `TOKEN_QUERY`.
unsafe fn token_default_owner_sid(token: HANDLE) -> io::Result<String> {
    // SAFETY: as for token_user_sid.
    unsafe {
        let mut len = 0u32;
        GetTokenInformation(token, TokenOwner, null_mut(), 0, &mut len);
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(err);
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token, TokenOwner, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(io::Error::last_os_error());
        }
        let owner = &*(buf.as_ptr() as *const TOKEN_OWNER);
        sid_to_string(owner.Owner)
    }
}

/// SIDs that legitimately own objects this process creates: the user SID and
/// the token's default owner (they differ for an elevated administrator).
pub fn self_owner_sids() -> io::Result<Vec<String>> {
    // SAFETY: token handle checked and closed on all paths.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = token_user_sid(token);
        let owner = token_default_owner_sid(token);
        CloseHandle(token);
        let mut sids = vec![user?];
        let owner = owner?;
        if !sids.contains(&owner) {
            sids.push(owner);
        }
        Ok(sids)
    }
}

/// String SID of the user that owns this process token.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: token handle checked and closed on all paths.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let r = token_user_sid(token);
        CloseHandle(token);
        r
    }
}

/// String SID of the user running process `pid`.
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    // SAFETY: handles checked and closed on all paths.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut token: HANDLE = null_mut();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        CloseHandle(process);
        if opened == 0 {
            return Err(io::Error::last_os_error());
        }
        let r = token_user_sid(token);
        CloseHandle(token);
        r
    }
}

/// Process ID and user SID of the process serving the pipe behind `handle`.
pub fn pipe_server_identity(client: &NamedPipeClient) -> io::Result<(u32, String)> {
    let mut pid = 0u32;
    // SAFETY: the handle belongs to a live client end of a named pipe.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((pid, process_user_sid(pid)?))
}

/// Refuse a pipe whose server does not run as the current user.
pub fn verify_same_user_server(client: &NamedPipeClient) -> io::Result<()> {
    let (pid, server_sid) = pipe_server_identity(client)?;
    let me = current_user_sid()?;
    if server_sid != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "pipe server (pid {pid}) runs as {server_sid}, not the current user {me}; refusing to send credentials"
            ),
        ));
    }
    Ok(())
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

    fn dacl(&self) -> io::Result<*mut ACL> {
        let mut present = 0i32;
        let mut defaulted = 0i32;
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: descriptor is valid for the lifetime of self.
        if unsafe {
            GetSecurityDescriptorDacl(self.descriptor, &mut present, &mut dacl, &mut defaulted)
        } == 0
            || present == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(dacl)
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

/// Owner SID string and DACL facts for a file or directory.
pub struct PathSecurity {
    pub owner_sid: String,
    pub dacl_protected: bool,
    /// SIDs granted access by allow ACEs. `None` entries are ACE types this
    /// module does not interpret (treated as not owner-only).
    pub allowed_sids: Vec<Option<String>>,
}

/// LocalSystem.
const SYSTEM_SID: &str = "S-1-5-18";
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;

pub fn path_security(path: &Path) -> io::Result<PathSecurity> {
    let wpath = wide_path(path);
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: outputs point into `sd`, which is freed below.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    // SAFETY: owner/dacl/sd valid until LocalFree(sd).
    let result = unsafe {
        let owner_sid = sid_to_string(owner);
        let mut control = 0u16;
        let mut revision = 0u32;
        GetSecurityDescriptorControl(sd, &mut control, &mut revision);
        let mut allowed_sids = Vec::new();
        if dacl.is_null() {
            // A NULL DACL grants everyone full access.
            allowed_sids.push(None);
        } else {
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace: *mut c_void = null_mut();
                if GetAce(dacl, i, &mut ace) == 0 {
                    allowed_sids.push(None);
                    continue;
                }
                let header = &*(ace as *const ACE_HEADER);
                match header.AceType {
                    ACCESS_ALLOWED_ACE_TYPE => {
                        let allowed = ace as *const ACCESS_ALLOWED_ACE;
                        let sid = std::ptr::addr_of!((*allowed).SidStart) as PSID;
                        allowed_sids.push(sid_to_string(sid).ok());
                    }
                    ACCESS_DENIED_ACE_TYPE => {}
                    _ => allowed_sids.push(None),
                }
            }
        }
        owner_sid.map(|owner_sid| PathSecurity {
            owner_sid,
            dacl_protected: control & SE_DACL_PROTECTED != 0,
            allowed_sids,
        })
    };
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe {
        LocalFree(sd);
    }
    result
}

impl PathSecurity {
    /// Owned by one of `owners`, DACL protected from inheritance, and every
    /// allow ACE names the user `me` or LocalSystem.
    pub fn is_owner_only(&self, me: &str, owners: &[String]) -> bool {
        owners.contains(&self.owner_sid)
            && self.dacl_protected
            && self
                .allowed_sids
                .iter()
                .all(|sid| matches!(sid.as_deref(), Some(s) if s == me || s == SYSTEM_SID))
    }
}

/// Whether `path` is owned by the current user and only the current user and
/// LocalSystem are granted access.
pub fn is_owner_only(path: &Path) -> io::Result<bool> {
    Ok(path_security(path)?.is_owner_only(&current_user_sid()?, &self_owner_sids()?))
}

/// Make an existing directory private. Refuses a directory owned by another
/// account instead of trusting or silently re-owning it.
pub fn harden_private_dir(dir: &Path) -> io::Result<()> {
    let me = current_user_sid()?;
    let owners = self_owner_sids()?;
    let s = path_security(dir)?;
    if !owners.contains(&s.owner_sid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by {}, not by the current user ({}); refusing to use it as a private WrongIndex directory",
                dir.display(),
                s.owner_sid,
                owners.join(" / ")
            ),
        ));
    }
    if s.is_owner_only(&me, &owners) {
        return Ok(());
    }
    let sec = OwnerOnlySecurity::new(true)?;
    let dacl = sec.dacl()?;
    let mut wpath = wide_path(dir);
    // SAFETY: valid path and DACL; owner/group untouched.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Create `path` (which must not exist) with an explicit owner-only DACL and
/// write `bytes`. The content is never readable through an inherited ACL.
pub fn write_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let sec = OwnerOnlySecurity::new(false)?;
    let attrs = sec.attributes();
    let wpath = wide_path(path);
    // SAFETY: valid path/attributes; handle ownership moves into File.
    let handle = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            GENERIC_WRITE,
            0,
            &attrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: freshly created, exclusively owned handle.
    let mut file = unsafe { File::from_raw_handle(handle) };
    file.write_all(bytes)?;
    file.sync_all()
}

/// Creates pipe instances that only the current user / LocalSystem can open.
pub struct PipeListener {
    name: String,
    security: OwnerOnlySecurity,
}

/// Who holds a pipe name we could not bind.
#[derive(Debug, PartialEq, Eq)]
pub enum ExistingOwner {
    /// A server running as the current user.
    SameUser { pid: u32 },
    /// A server running as another account, or one we cannot open at all.
    Foreign { detail: String },
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

    /// Identify the holder of a name whose bind failed. Never assumes the
    /// holder is our own daemon.
    pub async fn existing_owner(name: &str) -> ExistingOwner {
        match connect_unverified(name, Duration::from_secs(2)).await {
            Ok(client) => match pipe_server_identity(&client) {
                Ok((pid, sid)) if current_user_sid().ok().as_deref() == Some(sid.as_str()) => {
                    ExistingOwner::SameUser { pid }
                }
                Ok((pid, sid)) => ExistingOwner::Foreign {
                    detail: format!("pid {pid} running as {sid}"),
                },
                Err(e) => ExistingOwner::Foreign {
                    detail: format!("cannot identify server: {e}"),
                },
            },
            Err(e) => ExistingOwner::Foreign {
                detail: format!("cannot open existing pipe: {e}"),
            },
        }
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

async fn connect_unverified(name: &str, deadline: Duration) -> io::Result<NamedPipeClient> {
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

/// Connect to a pipe (retrying while all instances are busy) and verify that
/// its server runs as the current user before returning the stream.
pub async fn connect(name: &str, deadline: Duration) -> io::Result<NamedPipeClient> {
    let client = connect_unverified(name, deadline).await?;
    verify_same_user_server(&client)?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_owner_sids_start_with_the_user() {
        let sids = self_owner_sids().unwrap();
        assert_eq!(sids[0], current_user_sid().unwrap());
        assert!(sids.len() <= 2);
    }

    #[test]
    fn foreign_owner_is_not_owner_only() {
        let me = current_user_sid().unwrap();
        let owners = vec![me.clone()];
        let foreign = PathSecurity {
            owner_sid: "S-1-5-21-1-2-3-1001".into(),
            dacl_protected: true,
            allowed_sids: vec![Some(me.clone())],
        };
        assert!(!foreign.is_owner_only(&me, &owners));
        let broad = PathSecurity {
            owner_sid: me.clone(),
            dacl_protected: true,
            allowed_sids: vec![Some(me.clone()), Some("S-1-1-0".into())],
        };
        assert!(!broad.is_owner_only(&me, &owners));
        let unknown_ace = PathSecurity {
            owner_sid: me.clone(),
            dacl_protected: true,
            allowed_sids: vec![None],
        };
        assert!(!unknown_ace.is_owner_only(&me, &owners));
        let good = PathSecurity {
            owner_sid: me.clone(),
            dacl_protected: true,
            allowed_sids: vec![Some(me.clone()), Some(SYSTEM_SID.into())],
        };
        assert!(good.is_owner_only(&me, &owners));
    }

    #[test]
    fn sid_looks_like_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), sid);
    }

    #[tokio::test]
    async fn second_bind_fails_and_holder_is_identified_as_same_user() {
        let name = format!(r"\\.\pipe\wrongindex-test-{}", std::process::id());
        let (_l, first) = PipeListener::bind(&name).unwrap();
        let err = PipeListener::bind(&name)
            .err()
            .expect("second bind must fail");
        // ERROR_ACCESS_DENIED for FILE_FLAG_FIRST_PIPE_INSTANCE on an existing name.
        assert_eq!(err.raw_os_error(), Some(5), "{err}");
        let accept = tokio::spawn(async move {
            first.connect().await.unwrap();
            first
        });
        assert_eq!(
            PipeListener::existing_owner(&name).await,
            ExistingOwner::SameUser {
                pid: std::process::id()
            }
        );
        drop(accept.await.unwrap());
    }

    #[tokio::test]
    async fn verified_connect_accepts_own_server() {
        let name = format!(r"\\.\pipe\wrongindex-test-v-{}", std::process::id());
        let (_l, first) = PipeListener::bind(&name).unwrap();
        let accept = tokio::spawn(async move {
            first.connect().await.unwrap();
            first
        });
        let client = connect(&name, Duration::from_secs(2)).await.unwrap();
        let (pid, _) = pipe_server_identity(&client).unwrap();
        assert_eq!(pid, std::process::id());
        drop(accept.await.unwrap());
    }

    #[test]
    fn existing_dir_is_hardened_and_private_file_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pre-existing");
        std::fs::create_dir(&dir).unwrap();
        // Control: a std-created directory inherits a broader ACL.
        assert!(!is_owner_only(&dir).unwrap());
        harden_private_dir(&dir).unwrap();
        assert!(is_owner_only(&dir).unwrap());

        let file = dir.join("token");
        write_private_new(&file, b"secret").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"secret");
        assert!(is_owner_only(&file).unwrap());
        // CREATE_NEW: never overwrite an existing file in place.
        assert!(write_private_new(&file, b"x").is_err());
    }
}
