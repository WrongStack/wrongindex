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
    pub fn endpoint_name(&self) -> io::Result<String> {
        let user = user_identity()?;
        let mut key = self.root.to_string_lossy().into_owned();
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
