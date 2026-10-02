//! Opt-in structural request trace. Never receives credentials or message bodies.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// Fixed privacy/retention ceiling, not a throughput tuning parameter.
const LIMIT: u64 = 16 * 1024 * 1024;

fn record(body: &Value) -> Value {
    json!({
        "version": 1,
        "request_id": uuid::Uuid::new_v4().to_string(),
        "unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
        "provider": "openrouter",
        "model": body.get("model"),
        "stream": body.get("stream"),
        "message_count": body.get("messages").and_then(Value::as_array).map_or(0, Vec::len),
        "tools": body.get("tools"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_preserves_optional_schema_without_messages_or_credentials() {
        let body = json!({"model":"mock", "stream":true,
        "messages":[{"content":"PRIVATE PROMPT"}], "api_key":"SECRET",
        "tools":[{"type":"function", "function":{"name":"lookup", "parameters":{
            "type":"object", "additionalProperties":false,
            "properties":{"customView":{"type":"string","minLength":1}}
        }}}]});
        let trace = record(&body);
        assert_eq!(trace["tools"], body["tools"]);
        assert!(trace["tools"][0]["function"]["parameters"]
            .get("required")
            .is_none());
        assert!(!trace.to_string().contains("PRIVATE PROMPT"));
        assert!(!trace.to_string().contains("SECRET"));
        assert_eq!(trace["stream"], true);
    }

    #[test]
    fn writing_rotates_and_uses_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        write_with_limit(dir.path(), &json!({"tools":[]}), 128).unwrap();
        for _ in 0..12 {
            write_with_limit(dir.path(), &json!({"tools":[]}), 128).unwrap();
        }
        assert!(dir.path().join("requests.jsonl.1").exists());
        assert!(
            std::fs::metadata(dir.path().join("requests.jsonl"))
                .unwrap()
                .len()
                <= 128
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("requests.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_and_public_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let target = dir.path().join("target");
        std::fs::write(&target, "untouched").unwrap();
        symlink(&target, dir.path().join("requests.jsonl")).unwrap();
        assert!(write_with_limit(dir.path(), &json!({}), 128).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        std::fs::remove_file(dir.path().join("requests.jsonl")).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(write_with_limit(dir.path(), &json!({}), 128).is_err());
    }

    #[test]
    fn competing_writer_fails_fast_without_touching_trace() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let lock = private_file(&dir.path().join("trace.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        assert!(write_with_limit(dir.path(), &json!({}), 128).is_err());
        assert!(!dir.path().join("requests.jsonl").exists());
    }

    #[test]
    fn missing_directory_is_created_privately() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("trace");
        write_with_limit(&dir, &json!({}), 128).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_handle_stays_pinned_after_path_replacement() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("trace");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pinned = TraceDirectory::open(&dir).unwrap();
        let moved = parent.path().join("moved");
        std::fs::rename(&dir, &moved).unwrap();
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), &dir).unwrap();
        pinned.file("requests.jsonl", true).unwrap();
        pinned.rotate().unwrap();
        assert!(moved.join("requests.jsonl.1").exists());
        assert!(!other.path().join("requests.jsonl").exists());
        assert!(!other.path().join("requests.jsonl.1").exists());
        assert!(TraceDirectory::open(&dir).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unsafe_lock_and_backup_and_releases_lock_on_error() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "untouched").unwrap();
        symlink(&target, dir.path().join("trace.lock")).unwrap();
        assert!(write_with_limit(dir.path(), &json!({}), 8).is_err());
        assert!(!dir.path().join("requests.jsonl").exists());
        std::fs::remove_file(dir.path().join("trace.lock")).unwrap();
        write_with_limit(dir.path(), &json!({}), 8).unwrap();
        symlink(&target, dir.path().join("requests.jsonl.1")).unwrap();
        assert!(write_with_limit(dir.path(), &json!({}), 4).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        std::fs::remove_file(dir.path().join("requests.jsonl.1")).unwrap();
        write_with_limit(dir.path(), &json!({}), 4).unwrap();
        // Another writer can lock and rotate after both success and error.
        write_with_limit(dir.path(), &json!({}), 4).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 4);
        assert!(
            std::fs::metadata(dir.path().join("requests.jsonl.1"))
                .unwrap()
                .len()
                <= 4
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_hardlinked_trace_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "untouched").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&target, dir.path().join("requests.jsonl")).unwrap();
        assert!(write_with_limit(dir.path(), &json!({}), 128).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn oversized_record_does_not_write_or_rotate() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(write_with_limit(dir.path(), &json!({"tools":"x".repeat(200)}), 128).is_err());
        assert!(!dir.path().join("requests.jsonl").exists());
    }
}

/// Process environment only; intentionally not loaded from agent.env.
/// Missing/empty setting means zero filesystem activity.
#[derive(Default)]
pub(super) struct RequestTrace {
    pub(super) directory: Option<PathBuf>,
}

impl RequestTrace {
    pub(super) fn from_env() -> Self {
        Self {
            directory: std::env::var_os("DAIMONOS_OPENROUTER_TRACE_DIR")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
        }
    }

    pub(super) async fn capture(&self, body: &Value) {
        let Some(path) = self.directory.clone() else {
            return;
        };
        let projection = record(body);
        let result = tokio::task::spawn_blocking(move || {
            write_with_limit(Path::new(&path), &projection, LIMIT)
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            // Do not log paths, schemas, credentials, or potentially sensitive IO errors.
            tracing::warn!(
                event = "openrouter_request_trace_failed",
                "structural request trace unavailable"
            );
        }
    }
}

fn write_with_limit(dir: &Path, projection: &Value, limit: u64) -> std::io::Result<()> {
    use std::io::Write;
    let mut bytes = serde_json::to_vec(projection)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("trace record exceeds limit"));
    }
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let dir = TraceDirectory::open(dir)?;
    let lock = dir.file("trace.lock", true)?;
    // Do not stall a model request behind another process's tracing IO.
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let mut file = dir.file("requests.jsonl", true)?;
    if file.metadata()?.len() + bytes.len() as u64 > limit {
        match dir.file("requests.jsonl.1", false) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        dir.rotate()?;
        file = dir.file("requests.jsonl", true)?;
    }
    let original_len = file.metadata()?.len();
    if let Err(error) = file.write_all(&bytes) {
        // Best effort: do not leave a partial JSON line for the next writer.
        let _ = file.set_len(original_len);
        return Err(error);
    }
    Ok(()) // dropping the lock releases it, including on error
}

// Pin the validated directory inode for every Unix open/rename, so replacing
// the directory path cannot redirect subsequent writes. Parent symlinks resolve
// once during this open; the final component must not be a symlink.
struct TraceDirectory {
    #[cfg(unix)]
    handle: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl TraceDirectory {
    fn open(path: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let handle = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            check_private(&handle.metadata()?, true)?;
            Ok(Self { handle })
        }
        #[cfg(not(unix))]
        {
            check_private(&std::fs::symlink_metadata(path)?, true)?;
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    fn file(&self, name: &str, create: bool) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            let name = std::ffi::CString::new(name)?;
            let flags = libc::O_WRONLY
                | libc::O_APPEND
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if create { libc::O_CREAT } else { 0 };
            // SAFETY: directory FD is live, name is NUL-terminated, mode is
            // supplied for O_CREAT. On success we take sole ownership of fd.
            let fd = unsafe { libc::openat(self.handle.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: openat returned a fresh, valid file descriptor.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            check_private(&file.metadata()?, false)?;
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            if !create && !self.path.join(name).exists() {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            private_file(&self.path.join(name))
        }
    }

    fn rotate(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = self.handle.as_raw_fd();
            // SAFETY: both names are static NUL-terminated strings and fd
            // remains live for the call. renameat replaces only the dir entry.
            let result = unsafe {
                libc::renameat(
                    fd,
                    c"requests.jsonl".as_ptr(),
                    fd,
                    c"requests.jsonl.1".as_ptr(),
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        std::fs::rename(
            self.path.join("requests.jsonl"),
            self.path.join("requests.jsonl.1"),
        )
    }
}

#[cfg(any(test, not(unix)))]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    check_private(&file.metadata()?, false)?;
    Ok(file)
}

fn check_private(meta: &std::fs::Metadata, directory: bool) -> std::io::Result<()> {
    if (directory && !meta.is_dir()) || (!directory && !meta.is_file()) {
        return Err(std::io::Error::other("trace path has wrong type"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments and no memory access.
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
            || (!directory && meta.nlink() != 1)
        {
            return Err(std::io::Error::other(
                "trace path must be private and owned",
            ));
        }
    }
    Ok(())
}
