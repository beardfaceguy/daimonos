//! Per-workspace MCP primary election. No stdio forwarding lives here.

use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

pub(crate) enum Election {
    Connected(UnixStream),
    Primary(Primary),
    Pending,
}

pub(crate) struct Primary {
    pub listener: UnixListener,
    _guard: SocketGuard,
}

struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_socket() && meta.dev() == self.dev && meta.ino() == self.ino {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Attempt a connection before arbitration. Only the lock holder may retire a
/// stale socket, and it must check for a listener again after acquiring it.
pub(crate) fn elect(path: &Path) -> io::Result<Election> {
    if let Ok(stream) = UnixStream::connect(path) {
        return Ok(Election::Connected(stream));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let dir_meta = std::fs::symlink_metadata(parent)?;
    // The socket and lock are shared only by processes of this user. Never
    // arbitrate through a symlink or group/world-writable directory.
    if !dir_meta.is_dir()
        || dir_meta.uid() != unsafe { libc::geteuid() }
        || dir_meta.permissions().mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "MCP socket directory must be owner-controlled",
        ));
    }
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock_path)?;
    let lock_meta = lock.metadata()?;
    if !lock_meta.file_type().is_file()
        || lock_meta.uid() != unsafe { libc::geteuid() }
        || lock_meta.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "MCP lock must be owner-only regular file",
        ));
    }
    match lock.try_lock_exclusive() {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(Election::Pending),
        Err(err) => return Err(err),
    }
    if let Ok(stream) = UnixStream::connect(path) {
        return Ok(Election::Connected(stream));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() } => {
            std::fs::remove_file(path)?
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refusing to replace non-owned or non-socket MCP path",
            ))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    let guard = SocketGuard {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        _lock: lock,
    };
    Ok(Election::Primary(Primary {
        listener,
        _guard: guard,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_socket_is_replaced_only_under_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let primary = match elect(&path).unwrap() {
            Election::Primary(primary) => primary,
            _ => panic!("stale socket should be replaced"),
        };
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        drop(primary);
        assert!(!path.exists());
    }

    #[test]
    fn symlink_lock_is_rejected_without_replacing_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let target = dir.path().join("target");
        std::fs::write(&target, "keep me").unwrap();
        std::os::unix::fs::symlink(&target, path.with_extension("lock")).unwrap();
        assert!(elect(&path).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
        assert!(!path.exists());
    }

    #[test]
    fn non_socket_path_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        std::fs::write(&path, "do not delete").unwrap();
        assert!(elect(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "do not delete");
    }

    #[test]
    fn contended_lock_does_not_remove_primary_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let primary = match elect(&path).unwrap() {
            Election::Primary(primary) => primary,
            _ => panic!("expected primary"),
        };
        // Force the connect check to fail while the lock remains held.
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(elect(&path).unwrap(), Election::Pending));
        drop(primary);
    }

    #[test]
    fn single_contender_becomes_primary_and_second_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp.sock");
        let first = elect(&socket).unwrap();
        assert!(matches!(first, Election::Primary(_)));
        assert!(matches!(elect(&socket).unwrap(), Election::Connected(_)));
    }
}
