//! Data root, private directories, endpoint naming, and discovery files.
//!
//! Layout under the data root (architecture §3):
//!
//! ```text
//! registry.db
//! runtime/endpoint.json   discovery metadata, no secret
//! runtime/token           connection credential (owner-private directory)
//! logs/
//! staging/
//! ```

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use wrongindex_contracts::EndpointInfo;

pub const DATA_ROOT_ENV: &str = "WRONGINDEX_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRoot {
    root: PathBuf,
}

impl DataRoot {
    /// Explicit root, else `WRONGINDEX_HOME`, else `~/.wrongindex`.
    pub fn resolve(explicit: Option<&Path>) -> io::Result<Self> {
        let root = if let Some(p) = explicit {
            p.to_path_buf()
        } else if let Some(p) = std::env::var_os(DATA_ROOT_ENV).filter(|v| !v.is_empty()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory")
                })?;
            PathBuf::from(home).join(".wrongindex")
        };
        let root = std::path::absolute(root)?;
        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
    pub fn registry_db(&self) -> PathBuf {
        self.root.join("registry.db")
    }
    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }
    pub fn endpoint_file(&self) -> PathBuf {
        self.runtime_dir().join("endpoint.json")
    }
    pub fn token_file(&self) -> PathBuf {
        self.runtime_dir().join("token")
    }

    /// Create the root and its subdirectories. On Windows each directory that
    /// does not yet exist is created with a protected DACL (current user +
    /// LocalSystem only).
    pub fn ensure_private_dirs(&self) -> io::Result<()> {
        for dir in [
            self.root.clone(),
            self.runtime_dir(),
            self.logs_dir(),
            self.staging_dir(),
        ] {
            create_private_dir(&dir)?;
        }
        Ok(())
    }

    /// Endpoint name: unique per OS user and per data root, so test and
    /// portable roots never collide with the normal daemon.
    ///
    /// The name is the singleton guard, so it must be one per directory, not
    /// one per spelling: a trailing separator, an 8.3 short name, a junction
    /// or a `subst` drive used to give another name - and with it a second
    /// service on the same registry. The root is therefore named by what the
    /// file system resolves it to ([`one_spelling`]); for an ordinary path
    /// that is the path itself, so the name is the one earlier versions used.
    pub fn endpoint_name(&self) -> io::Result<String> {
        let user = user_identity()?;
        let mut key = one_spelling(&self.root).to_string_lossy().into_owned();
        if cfg!(windows) {
            key = key.replace('/', "\\").to_lowercase();
        }
        let digest = Sha256::digest(format!("{user}\0{key}").as_bytes());
        let short = hex::encode(&digest[..8]);
        Ok(if cfg!(windows) {
            format!(r"\\.\pipe\wrongindex-{short}")
        } else {
            self.runtime_dir()
                .join(format!("wrongindex-{short}.sock"))
                .to_string_lossy()
                .into_owned()
        })
    }
}

impl DataRoot {
    /// Where a service of this root that could not start leaves the reason:
    /// in the user's temporary folder, not in the data root (which may be
    /// what failed, and whose log is opened only after it is ready), named
    /// after the root's endpoint. A detached service has no console.
    pub fn start_error_file(&self) -> io::Result<PathBuf> {
        let name = self.endpoint_name()?;
        let short = name.rsplit('-').next().unwrap_or("x").to_string();
        Ok(std::env::temp_dir().join(format!("wrongindexd-start-error-{short}.txt")))
    }

    /// Record why a service of this root could not start (best effort).
    pub fn record_start_error(&self, reason: &str) {
        if let Ok(path) = self.start_error_file() {
            let _ = std::fs::write(path, reason);
        }
    }

    /// The reason the last start failed, if one is recorded; removed with it.
    pub fn take_start_error(&self) -> Option<String> {
        let path = self.start_error_file().ok()?;
        let reason = std::fs::read_to_string(&path).ok();
        let _ = std::fs::remove_file(&path);
        reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
    }
}

/// `path` (absolute) as the file system spells it: its deepest existing
/// ancestor resolved (links, 8.3 short names, `subst` drives), the rest as
/// written, without `.` components or a trailing separator. A data root that
/// does not exist yet is named through its parent.
fn one_spelling(path: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&cur) {
            let mut out = strip_verbatim(&real);
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (
            cur.file_name().map(|n| n.to_os_string()),
            cur.parent().map(Path::to_path_buf),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                cur = parent;
            }
            // Nothing of it exists (or it ends in `..`): the components as
            // written, which at least drops `.` and a trailing separator.
            _ => return path.components().collect(),
        }
    }
}

/// `\\?\C:\x` as `C:\x` and `\\?\UNC\server\share` as `\\server\share`.
fn strip_verbatim(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(plain) = s.strip_prefix(r"\\?\") {
        PathBuf::from(plain)
    } else {
        path.to_path_buf()
    }
}

/// Stable identity of the current OS user (SID on Windows).
pub fn user_identity() -> io::Result<String> {
    #[cfg(windows)]
    {
        crate::win::current_user_sid()
    }
    #[cfg(not(windows))]
    {
        Ok(std::env::var("USER").unwrap_or_default())
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        // Never trust a pre-existing directory's ACL: refuse one owned by
        // another account, re-apply the owner-only DACL to our own.
        #[cfg(windows)]
        return crate::win::harden_private_dir(dir);
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = std::fs::metadata(dir)?;
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc_getuid() };
            if meta.uid() != uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is not owned by the current user", dir.display()),
                ));
            }
            return std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(windows)]
    {
        match crate::win::create_private_dir(dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }
}

#[cfg(not(windows))]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn temp_sibling(path: &Path) -> io::Result<std::path::PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    Ok(dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    )))
}

/// Atomically write a secret: the temp file is created exclusively with an
/// owner-only DACL (Windows) or mode 0600 (Unix) before any byte is written,
/// then renamed over `path` (rename keeps the file's own security descriptor).
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path)?;
    let _ = std::fs::remove_file(&tmp);
    #[cfg(windows)]
    crate::win::write_private_new(&tmp, bytes)?;
    #[cfg(not(windows))]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write `bytes` to `path` atomically (temp file in the same directory + rename).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path)?;
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Generate a 256-bit connection credential.
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(hex::encode(bytes))
}

/// Constant-time comparison for credentials.
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Discovery information a client needs to connect.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub info: EndpointInfo,
    pub token: String,
}

impl Discovery {
    pub fn publish(root: &DataRoot, info: &EndpointInfo, token: &str) -> io::Result<()> {
        // Token first: a reader that sees the new endpoint must find its credential.
        write_private_atomic(&root.token_file(), token.as_bytes())?;
        write_atomic(
            &root.endpoint_file(),
            serde_json::to_vec_pretty(info)
                .map_err(io::Error::other)?
                .as_slice(),
        )
    }

    pub fn load(root: &DataRoot) -> io::Result<Self> {
        let info: EndpointInfo = serde_json::from_slice(&std::fs::read(root.endpoint_file())?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let token = std::fs::read_to_string(root.token_file())?
            .trim()
            .to_string();
        Ok(Self { info, token })
    }

    /// Remove discovery files only if they still describe `instance_id`; a
    /// newer daemon's metadata is never deleted by an exiting older one.
    pub fn retract(root: &DataRoot, instance_id: &str) -> io::Result<bool> {
        let current = match Self::load(root) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if current.info.daemon_instance_id != instance_id {
            return Ok(false);
        }
        let _ = std::fs::remove_file(root.endpoint_file());
        let _ = std::fs::remove_file(root.token_file());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str) -> EndpointInfo {
        EndpointInfo {
            endpoint: "e".into(),
            client_bridge: None,
            daemon_instance_id: id.into(),
            daemon_version: "0".into(),
            pid: 1,
            protocol_major: 1,
            protocol_minor: 0,
            started_at_ms: 0,
        }
    }

    #[test]
    fn endpoint_names_differ_per_root() {
        let a = DataRoot::resolve(Some(Path::new("C:/x/a"))).unwrap();
        let b = DataRoot::resolve(Some(Path::new("C:/x/b"))).unwrap();
        assert_ne!(a.endpoint_name().unwrap(), b.endpoint_name().unwrap());
        assert_eq!(a.endpoint_name().unwrap(), a.endpoint_name().unwrap());
    }

    /// The 8.3 spelling of `p` (`None` when the call fails).
    #[cfg(windows)]
    fn short_path(p: &Path) -> Option<PathBuf> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buf` is `buf.len()` units long.
        let n = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
                wide.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        } as usize;
        (n > 0 && n < buf.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
    }

    /// Every spelling of one data root names one endpoint: the endpoint is
    /// the singleton guard, and a second name meant a second service.
    #[test]
    fn one_directory_has_one_endpoint_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("Home Root");
        std::fs::create_dir(&home).unwrap();
        let name = |p: &Path| DataRoot::resolve(Some(p)).unwrap().endpoint_name().unwrap();
        let plain = name(&home);
        let s = home.to_string_lossy().into_owned();
        let sep = std::path::MAIN_SEPARATOR;
        for spelling in [
            format!("{s}{sep}"),
            format!("{s}{sep}."),
            format!("{s}{sep}sub{sep}.."),
        ] {
            assert_eq!(name(Path::new(&spelling)), plain, "{spelling}");
        }
        #[cfg(windows)]
        {
            assert_eq!(name(Path::new(&s.to_uppercase())), plain);
            assert_eq!(name(Path::new(&s.replace('\\', "/"))), plain);
            // A junction to the root is the same directory.
            let link = tmp.path().join("link");
            let made = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&home)
                .output()
                .unwrap();
            assert!(made.status.success(), "{made:?}");
            assert_eq!(name(&link), plain);
            // The 8.3 short name, where the volume has them.
            if let Some(short) = short_path(&home)
                && short != home
            {
                assert_eq!(name(&short), plain, "{}", short.display());
            }
        }
        // A root that does not exist yet is named through its parent.
        let later = home.join("later");
        let before = name(&later);
        assert_eq!(
            name(Path::new(&format!("{}{sep}", later.display()))),
            before
        );
        std::fs::create_dir(&later).unwrap();
        assert_eq!(name(&later), before, "creating the root keeps its name");
        assert_ne!(before, plain);
    }

    #[test]
    fn a_start_error_is_kept_outside_the_root_until_read() {
        let tmp = tempfile::tempdir().unwrap();
        // A root that cannot be a directory: the service would fail there.
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let root = DataRoot::resolve(Some(&file)).unwrap();
        assert!(!root.start_error_file().unwrap().starts_with(&file));
        let _ = root.take_start_error();
        root.record_start_error("creating data root: it is a file");
        assert_eq!(
            root.take_start_error().as_deref(),
            Some("creating data root: it is a file")
        );
        assert_eq!(root.take_start_error(), None, "read once");
    }

    #[test]
    fn discovery_round_trip_and_owner_only_retract() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&tmp.path().join("home"))).unwrap();
        root.ensure_private_dirs().unwrap();
        let token = generate_token().unwrap();
        assert_eq!(token.len(), 64);
        Discovery::publish(&root, &info("one"), &token).unwrap();
        #[cfg(windows)]
        {
            assert!(crate::win::is_owner_only(&root.token_file()).unwrap());
            assert!(crate::win::is_owner_only(&root.runtime_dir()).unwrap());
        }
        // Republishing replaces the token without weakening its ACL.
        Discovery::publish(&root, &info("one"), &token).unwrap();
        #[cfg(windows)]
        assert!(crate::win::is_owner_only(&root.token_file()).unwrap());
        let d = Discovery::load(&root).unwrap();
        assert_eq!(d.token, token);
        assert!(!Discovery::retract(&root, "other").unwrap());
        assert!(root.endpoint_file().exists());
        assert!(Discovery::retract(&root, "one").unwrap());
        assert!(!root.endpoint_file().exists());
        assert!(!root.token_file().exists());
    }

    #[test]
    fn pre_existing_root_is_hardened() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("runtime")).unwrap();
        let root = DataRoot::resolve(Some(&home)).unwrap();
        root.ensure_private_dirs().unwrap();
        #[cfg(windows)]
        {
            assert!(crate::win::is_owner_only(&home).unwrap());
            assert!(crate::win::is_owner_only(&home.join("runtime")).unwrap());
        }
    }

    #[test]
    fn token_comparison() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }
}
