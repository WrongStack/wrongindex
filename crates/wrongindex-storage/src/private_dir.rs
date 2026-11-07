//! A private temporary directory: unpredictable name, created exclusively,
//! and private from the moment it exists, then removed when dropped.
//!
//! Used for what WrongIndex writes to the temporary folder and then runs or
//! reads back: the parser programs (Python, Go, the tree-sitter and
//! TypeScript bundles) and the scan's copy of a small index. A temporary
//! folder can be shared by other accounts (`C:\Windows\Temp`, a
//! misconfigured `TEMP`); a directory there that only inherits its parent's
//! permissions would let another account read the copy, or replace a
//! program between its writing and its start.
//!
//! On Windows the directory is created by `CreateDirectoryW` with a security
//! descriptor in the same call: a protected DACL granting the current user
//! and LocalSystem full access, inherited by everything created inside, so
//! there is no moment with inherited permissions. Elsewhere it is created
//! with mode 0700. Either way creation fails when the name exists (a planted
//! directory or link is never used); a new name is tried instead.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// A new private directory `<temp>/<prefix><random>`.
    pub fn new(prefix: &str) -> io::Result<Self> {
        Self::new_in(&std::env::temp_dir(), prefix)
    }

    pub fn new_in(parent: &Path, prefix: &str) -> io::Result<Self> {
        let mut last = None;
        for _ in 0..16 {
            let path = parent.join(format!("{prefix}{}", random_suffix()));
            match create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no free private directory name")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 64 random bits as hex, from the standard library's randomly seeded hasher.
fn random_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("{:016x}", h.finish())
}

#[cfg(windows)]
fn create(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let sid = current_user_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;OICI;GA;;;{sid})(A;OICI;GA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: valid NUL-terminated strings; the descriptor is freed below and
    // outlives the call that uses it.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let ok = CreateDirectoryW(wpath.as_ptr(), &attrs);
        let err = io::Error::last_os_error();
        LocalFree(sd);
        if ok == 0 { Err(err) } else { Ok(()) }
    }
}

/// String SID of the account this process runs as.
#[cfg(windows)]
pub fn current_user_sid() -> io::Result<String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: handles checked and closed; standard two-call size query with a
    // u64-aligned buffer for TOKEN_USER; the SID string freed with LocalFree.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            CloseHandle(token);
            return Err(err);
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        Ok(out)
    }
}

#[cfg(not(windows))]
fn create(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The directory's DACL in SDDL form.
    #[cfg(windows)]
    fn dacl_sddl(p: &Path) -> String {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
        let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: test-only; outputs freed with LocalFree.
        unsafe {
            assert_eq!(
                GetNamedSecurityInfoW(
                    w.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut sd,
                ),
                0
            );
            let mut s: *mut u16 = null_mut();
            assert_ne!(
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    sd,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut s,
                    null_mut(),
                ),
                0
            );
            let n = (0..).take_while(|&i| *s.add(i) != 0).count();
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
            LocalFree(s.cast());
            LocalFree(sd);
            out
        }
    }

    #[test]
    fn private_from_creation_unique_and_removed() {
        let parent = tempfile::tempdir().unwrap();
        let a = PrivateDir::new_in(parent.path(), "wi-t-").unwrap();
        let b = PrivateDir::new_in(parent.path(), "wi-t-").unwrap();
        assert_ne!(a.path(), b.path());
        assert!(a.path().file_name().unwrap().to_string_lossy().len() > "wi-t-".len() + 8);
        std::fs::write(a.path().join("program.py"), b"print(1)").unwrap();
        #[cfg(windows)]
        {
            let me = current_user_sid().unwrap();
            for p in [a.path().to_path_buf(), a.path().join("program.py")] {
                let d = dacl_sddl(&p);
                // Protected (no inherited ACEs from the temp folder): exactly
                // the user and LocalSystem.
                let aces: Vec<&str> = d
                    .split('(')
                    .skip(1)
                    .map(|a| a.trim_end_matches(')'))
                    .collect();
                assert!(
                    aces.iter()
                        .all(|a| a.ends_with(&format!(";;;{me}")) || a.ends_with(";;;SY")),
                    "{p:?}: {d}"
                );
                assert!(aces.iter().any(|a| a.ends_with(&format!(";;;{me}"))), "{d}");
                if p == a.path() {
                    assert!(d.starts_with("D:P"), "not protected: {d}");
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(a.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let p = a.path().to_path_buf();
        drop(a);
        assert!(!p.exists());
    }

    #[test]
    fn an_existing_name_is_never_used() {
        let parent = tempfile::tempdir().unwrap();
        let taken = parent.path().join("taken");
        std::fs::create_dir(&taken).unwrap();
        let e = create(&taken).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    }
}
