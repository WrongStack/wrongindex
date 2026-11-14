//! Unix transport (WI-ADR-024): a stream socket in an owner-only directory,
//! a lock file beside it as the singleton guard, and the peer's effective
//! uid checked on both ends of every connection.
//!
//! A socket file cannot be the guard the way a first pipe instance is on
//! Windows: it outlives a service that was killed, and a second `bind` after
//! an `unlink` succeeds while the first service still runs. An advisory lock
//! (`flock`) ends with its holder, so it is the guard, and a socket found
//! while the lock is ours is a leftover that may be replaced.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

use crate::endpoint::effective_uid;

/// Why a socket could not be bound.
#[derive(Debug)]
pub enum BindError {
    /// Another service of this user holds the endpoint's lock.
    Held,
    Io(io::Error),
}

impl From<io::Error> for BindError {
    fn from(e: io::Error) -> Self {
        BindError::Io(e)
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::Held => f.write_str("the endpoint is held by another service"),
            BindError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for BindError {}

/// A bound socket and the lock that makes it this process's. Dropping it
/// removes the socket (when it is still the one bound here) and then
/// releases the lock.
pub struct SocketListener {
    listener: UnixListener,
    path: PathBuf,
    id: (u64, u64),
    _lock: File,
}

/// The lock file of the socket at `socket`.
pub fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

impl SocketListener {
    /// Take the endpoint at `socket`: make its directory owner-only, take the
    /// lock, replace a leftover socket, bind, and restrict the socket to the
    /// owner. Must run inside a Tokio runtime.
    pub fn bind(socket: &Path) -> Result<Self, BindError> {
        let dir = socket
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| io::Error::other(format!("{} has no directory", socket.display())))?;
        private_socket_dir(dir)?;
        // std opens with O_CLOEXEC: no child of the service inherits the lock.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path(socket))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(BindError::Held),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        // The lock is ours, so whatever is at `socket` was left by a service
        // that ended without removing it. Only a socket is replaced.
        match std::fs::symlink_metadata(socket) {
            Ok(m) if m.file_type().is_socket() => std::fs::remove_file(socket)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a socket; refusing to replace it",
                        socket.display()
                    ),
                )
                .into());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(socket)?;
        // The directory already admits only the owner; the socket says so too.
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        let meta = std::fs::symlink_metadata(socket)?;
        Ok(Self {
            listener,
            path: socket.to_path_buf(),
            id: (meta.dev(), meta.ino()),
            _lock: lock,
        })
    }

    /// The next client. A peer running as another user is refused with
    /// `PermissionDenied` (the caller logs it and accepts again).
    pub async fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        let peer = stream.peer_cred()?.uid();
        let me = effective_uid();
        if peer != me {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refused a client running as uid {peer} (service uid {me})"),
            ));
        }
        Ok(stream)
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        // Removed while the lock is still held (fields drop after this), and
        // only when it is the socket bound here.
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.id
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create `dir` owner-only (its missing parents with the default mode), or
/// check an existing one: a real directory, not a link, owned by this user;
/// its mode is then reset to 0700.
fn private_socket_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        other => other?,
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory", dir.display()),
        ));
    }
    if meta.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not owned by the current user", dir.display()),
        ));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Connect to the socket at `path` and check that its server runs as this
/// user before returning the stream.
pub async fn connect(path: &str, deadline: Duration) -> io::Result<UnixStream> {
    let stream = tokio::time::timeout(deadline, UnixStream::connect(path))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
    let peer = stream.peer_cred()?.uid();
    let me = effective_uid();
    if peer != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path} is served by uid {peer}, not by this user (uid {me})"),
        ));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn one_holder_per_endpoint_and_a_leftover_socket_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("runtime").join("wrongindex-test.sock");
        let first = SocketListener::bind(&socket).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(socket.parent().unwrap()), 0o700);
        assert_eq!(mode(&socket), 0o600);
        assert!(matches!(
            SocketListener::bind(&socket),
            Err(BindError::Held)
        ));
        // The second starter did not remove the holder's socket.
        assert!(socket.exists());

        // A client of this user is admitted and talks to the holder.
        let path = socket.to_string_lossy().into_owned();
        let (client, server) = tokio::join!(connect(&path, Duration::from_secs(5)), first.accept());
        let (mut client, mut server) = (client.unwrap(), server.unwrap());
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // Dropping the holder removes its socket and frees the endpoint.
        drop((client, server, first));
        assert!(!socket.exists());

        // A socket left behind by a service that was killed (its lock went
        // with it, the file did not) is replaced by the next service.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        let _again = SocketListener::bind(&socket).unwrap();
        assert!(matches!(
            SocketListener::bind(&socket),
            Err(BindError::Held)
        ));
    }

    #[tokio::test]
    async fn only_a_socket_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("wrongindex-test.sock");
        std::fs::write(&socket, b"not a socket").unwrap();
        let err = SocketListener::bind(&socket).err().unwrap();
        assert!(matches!(err, BindError::Io(ref e) if e.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read(&socket).unwrap(), b"not a socket");
    }

    #[tokio::test]
    async fn a_linked_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = SocketListener::bind(&link.join("wrongindex-test.sock"))
            .err()
            .unwrap();
        assert!(matches!(err, BindError::Io(ref e) if e.kind() == io::ErrorKind::PermissionDenied));
    }
}
