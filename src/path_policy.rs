//! Shared, descriptor-based filesystem authority for root-confined reads and walks.
//!
//! Context: docs/adr/022-shared-filesystem-authority.md (WP03); governing
//! contracts in docs/contexts/reviewer-runtime/INTERFACES.md ("State and
//! filesystem contracts", WP03 C) and INVENTORIES.md.
//!
//! A [`FilesystemAuthority`] pins the canonical run root as an owned directory
//! descriptor and mediates every actual open and walk against it. Confinement
//! holds at the opened object, not at a pre-checked path string, so a hostile
//! ancestor/root/link replacement after the check cannot redirect an open
//! outside the root. This module resolves relative inputs from the fixed root.
//! Frontends resolve their logical working directory before calling it.
//! Explicit out-of-root arguments are refused before outside data is exposed.
//! In-root symlinks (including ones
//! with absolute targets that stay inside the root) resolve normally.
//!
//! No-follow (ADR-022 / WP14) is an independent future surface: [`WalkOptions`]
//! carries the opt-in seam, but the full no-follow tool and the
//! `--no-follow-symlinks` option are not implemented here.

use std::path::{Component, Path, PathBuf};

/// Reviewed tool-refusal code: an explicit argument resolves outside the root.
pub const CODE_PATH_OUTSIDE_ROOT: &str = "path_outside_root";
/// Reviewed tool-refusal code: a symlink (or magic link) could not be honored
/// without leaving the root or following a forbidden link.
pub const CODE_SYMLINK_REFUSED: &str = "symlink_refused";
/// Reviewed startup/admission code: the confined profile has no enforceable
/// resolver on this platform/kernel. Returned before services start.
pub const CODE_FILESYSTEM_POLICY_UNSUPPORTED: &str = "filesystem_policy_unsupported";
/// Reviewed tool-refusal code: the resolved final object is a special file
/// (device, FIFO, or socket) whose blocking reopen is refused up front.
pub const CODE_UNSUPPORTED_FILE_TYPE: &str = "unsupported_file_type";
/// A configured traversal budget was exhausted.
pub const CODE_LIMIT_EXCEEDED: &str = "limit_exceeded";
/// The authorized path does not exist.
pub const CODE_PATH_NOT_FOUND: &str = "path_not_found";

/// Tools with adapters that preserve confined-mode guarantees (ADR-022).
pub fn tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "ls"
            | "search"
            | "diff_files"
            | "skill"
            | "set_cwd"
            | "batch"
            | "execute_script"
            | "get_tool_schema"
            | "list_tool_signatures"
            | "list_all_tools"
            | "session_stats"
            | "set_external_session_id"
            | "set_verbosity"
            | "workspace_info"
    )
}

/// A structured refusal or operation failure surfaced to the caller.
///
/// Field set matches the reviewed `ToolFailure` contract (code, message, tool,
/// argument, optional limit/configured/consumed/retryable). Defined here so WP03
/// compiles and tests independently of WP01/WP04; the integrator unifies it with
/// the shared run-policy error type during integration (see integration notes).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolFailure {
    pub code: String,
    pub message: String,
    pub tool: Option<String>,
    pub argument: Option<String>,
    #[serde(flatten)]
    pub budget: Option<Box<FailureBudget>>,
    pub retryable: bool,
}

/// Optional budget details, kept off the common refusal path.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailureBudget {
    pub limit: u64,
    pub configured: u64,
    pub consumed: u64,
}

impl ToolFailure {
    fn refusal(code: &str, message: impl Into<String>, argument: Option<&Path>) -> Self {
        ToolFailure {
            code: code.to_string(),
            message: message.into(),
            tool: None,
            argument: argument.map(|p| p.to_string_lossy().into_owned()),
            budget: None,
            retryable: false,
        }
    }

    /// Retain the caller's original argument rather than an internal path.
    pub fn with_argument(mut self, argument: &str) -> Self {
        self.argument = Some(argument.to_string());
        self
    }

    /// Encode the failure for both native and MCP callers.
    pub fn into_response(mut self, tool: &str) -> crate::protocol::Response {
        self.tool = Some(tool.to_string());
        let mut response =
            crate::protocol::Response::err(4, &format!("{}: {}", self.code, self.message));
        let mut data = serde_json::Map::new();
        data.insert("code".into(), serde_json::json!(self.code));
        data.insert("message".into(), serde_json::json!(self.message));
        data.insert("retryable".into(), serde_json::json!(self.retryable));
        if let Some(value) = self.tool {
            data.insert("tool".into(), serde_json::json!(value));
        }
        if let Some(value) = self.argument {
            data.insert("argument".into(), serde_json::json!(value));
        }
        if let Some(budget) = self.budget {
            data.insert("limit".into(), serde_json::json!(budget.limit));
            data.insert("configured".into(), serde_json::json!(budget.configured));
            data.insert("consumed".into(), serde_json::json!(budget.consumed));
        }
        response.d = Some(serde_json::Value::Object(data));
        response
    }

    pub fn path_outside_root(argument: &Path) -> Self {
        // Echo only the caller's argument, never the resolved outside location.
        Self::refusal(
            CODE_PATH_OUTSIDE_ROOT,
            "path resolves outside the confined root",
            Some(argument),
        )
    }

    pub fn symlink_refused(argument: &Path, _target_text: Option<&str>) -> Self {
        // Do not expose a target that may name an outside resource.
        Self::refusal(
            CODE_SYMLINK_REFUSED,
            "symlink could not be followed within the confined root",
            Some(argument),
        )
    }

    pub fn walk_limit(argument: &Path, configured: usize, consumed: usize) -> Self {
        Self::budget_limit(argument, "max_walk_entries", configured, consumed)
    }

    fn budget_limit(argument: &Path, name: &str, configured: usize, consumed: usize) -> Self {
        let mut failure = Self::refusal(
            CODE_LIMIT_EXCEEDED,
            format!("{name} exceeded: configured={configured}, consumed={consumed}"),
            Some(argument),
        );
        failure.budget = Some(Box::new(FailureBudget {
            limit: configured as u64,
            configured: configured as u64,
            consumed: consumed as u64,
        }));
        failure
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::refusal(CODE_FILESYSTEM_POLICY_UNSUPPORTED, message, None)
    }
}

/// Configurable bounds for the authority. Caller/config supplied — no hidden
/// constants. The integrator populates these from `reviewer.tool_limits` (see
/// integration notes); defaults belong in `daimonos.default.toml`.
#[derive(Debug, Clone, Copy)]
pub struct FsLimits {
    /// Maximum directory recursion depth for a walk (`None` = unbounded by the
    /// authority; the caller still passes `WalkOptions::max_depth`).
    pub max_walk_depth: Option<usize>,
    /// Upper bound on entries a single walk may enumerate before refusing.
    pub max_walk_entries: usize,
    /// Maximum number of symlinks followed while resolving one path. Bounds
    /// traversal explicitly instead of relying on a hidden kernel constant.
    pub max_symlinks: u32,
    /// Maximum bytes in one symlink target.
    pub max_symlink_target_bytes: usize,
}

impl Default for FsLimits {
    fn default() -> Self {
        let index = crate::config::IndexConfig::default();
        let tools = crate::config::ToolLimitsConfig::default();
        FsLimits {
            max_walk_depth: Some(index.max_depth),
            max_walk_entries: index.max_walk_entries,
            max_symlinks: tools.max_symlinks,
            max_symlink_target_bytes: tools.max_symlink_target_bytes,
        }
    }
}

/// Classification of a walk entry, derived from a `fstatat(AT_SYMLINK_NOFOLLOW)`
/// relative to a pinned directory descriptor (never by following the entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One enumerated, root-confined walk entry.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// Path relative to the confined root (the fixed root, not session cwd).
    pub relative_path: PathBuf,
    pub kind: EntryKind,
    /// Metadata from the independently authorized target handle.
    pub metadata: std::fs::Metadata,
    /// For a symlink: its literal target text, read without following it.
    pub link_target: Option<String>,
}

/// A walk entry that was intentionally skipped (e.g. an escaping or unreadable
/// link). Reported so callers can surface it without returning outside content.
#[derive(Debug, Clone)]
pub struct SkippedEntry {
    pub relative_path: PathBuf,
    pub reason: String,
    pub link_target: Option<String>,
}

impl Walk {
    /// Count skipped entries and link refusals without exposing target text.
    pub fn skipped_summary(&self) -> (usize, usize) {
        let links = self
            .skipped
            .iter()
            .filter(|entry| {
                let root_relative = !entry.relative_path.is_absolute()
                    && !entry
                        .relative_path
                        .components()
                        .any(|part| part == Component::ParentDir);
                root_relative
                    && (entry.link_target.is_some()
                        || entry.reason.contains(CODE_SYMLINK_REFUSED)
                        || entry.reason.contains(CODE_PATH_OUTSIDE_ROOT))
            })
            .count();
        (self.skipped.len(), links)
    }
}

/// Options controlling a confined walk.
#[derive(Debug, Clone, Copy)]
pub struct WalkOptions {
    /// Maximum recursion depth (0 = only the start directory's own entries).
    pub max_depth: Option<usize>,
    /// Follow authorized in-root directory links when true. The public
    /// no-follow tool surface remains deferred to WP14.
    pub follow_symlinks: bool,
    /// Include names beginning with a dot.
    pub include_hidden: bool,
    /// Directory names that must not be emitted or traversed.
    pub skip_dirs: &'static [&'static str],
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            max_depth: None,
            follow_symlinks: true,
            include_hidden: true,
            skip_dirs: &[],
        }
    }
}

/// Result of a confined walk: enumerated entries plus intentionally skipped ones.
#[derive(Debug, Clone, Default)]
pub struct Walk {
    pub entries: Vec<WalkEntry>,
    pub skipped: Vec<SkippedEntry>,
    ancestors: Vec<(u64, u64)>,
    exhausted: bool,
    depth_exhausted: Option<usize>,
    failure: Option<ToolFailure>,
}

/// An owned, already-authorized read handle to an in-root object.
///
/// Holding this is the proof of authorization: the handle refers to the exact
/// inode the kernel resolved under the pinned root, so a later ancestor/link
/// replacement cannot redirect subsequent reads elsewhere.
#[derive(Debug)]
pub struct AuthorizedRead {
    file: std::fs::File,
    relative_path: PathBuf,
}

impl AuthorizedRead {
    pub fn as_file(&self) -> &std::fs::File {
        &self.file
    }

    pub fn into_file(self) -> std::fs::File {
        self.file
    }

    /// Path relative to the confined root that was authorized.
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
}

/// Strip the fixed root prefix without erasing symlink-sensitive components.
/// Parent components are resolved only after preceding links are authorized.
fn relativize(root: &Path, path: &Path) -> Result<PathBuf, ToolFailure> {
    let root_norm = normalize_absolute(root);
    let relative = if path.is_absolute() {
        path.strip_prefix(&root_norm)
            .map_err(|_| ToolFailure::path_outside_root(path))?
    } else {
        path
    };
    let mut result = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => result.push(name),
            Component::ParentDir => result.push(".."),
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolFailure::path_outside_root(path));
            }
        }
    }
    Ok(result)
}

/// Collapse `.` and `..` lexically in an absolute path (no symlink resolution).
fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::Normal(c) => out.push(c),
            Component::ParentDir => {
                out.pop();
            }
        }
    }
    out
}

/// Canonical run-scoped filesystem authority.
///
/// Pins the canonical root as an owned directory descriptor and mediates every
/// actual open/walk relative to it via `openat2`. The pinned descriptor is the
/// run's fixed root identity; it does not change with session cwd.
/// A `cfg(test)` per-instance hook fired immediately before the final-component
/// open, letting a test deterministically win the pre-final-open race (move an
/// ancestor outside the root and plant an outside-only file) without any global
/// mutable hook or production sleep.
#[cfg(all(test, target_os = "linux"))]
pub(crate) struct PreFinalOpenObserver(pub Box<dyn Fn() + Send + Sync>);

#[cfg(all(test, target_os = "linux"))]
impl std::fmt::Debug for PreFinalOpenObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreFinalOpenObserver(..)")
    }
}

#[derive(Debug)]
pub struct FilesystemAuthority {
    root: PathBuf,
    limits: FsLimits,
    #[cfg(target_os = "linux")]
    root_fd: std::os::fd::OwnedFd,
    #[cfg(all(test, target_os = "linux"))]
    pre_final_open: Option<PreFinalOpenObserver>,
    #[cfg(all(test, target_os = "linux"))]
    post_walk_open: Option<PreFinalOpenObserver>,
}

#[cfg(all(test, target_os = "linux"))]
impl FilesystemAuthority {
    /// Install the deterministic pre-final-open observer (test-only).
    pub(crate) fn set_pre_final_open_observer(&mut self, f: impl Fn() + Send + Sync + 'static) {
        self.pre_final_open = Some(PreFinalOpenObserver(Box::new(f)));
    }

    pub(crate) fn set_post_walk_open_observer(&mut self, f: impl Fn() + Send + Sync + 'static) {
        self.post_walk_open = Some(PreFinalOpenObserver(Box::new(f)));
    }
}

impl FilesystemAuthority {
    /// Apply validated limits without reopening the pinned root.
    pub fn set_limits(&mut self, limits: FsLimits) {
        self.limits = limits;
    }

    /// The fixed canonical root this authority confines to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limits(&self) -> &FsLimits {
        &self.limits
    }

    /// Convert an argument to a root-relative path without filesystem access.
    pub fn relative_argument(&self, path: &Path) -> Result<PathBuf, ToolFailure> {
        relativize(&self.root, path)
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use super::ToolFailure;
    use std::ffi::{CString, OsStr};
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// Kernel `struct open_how` ABI (see `openat2(2)`). Declared locally because
    /// `libc::open_how` is `#[non_exhaustive]` and cannot be struct-literal'd.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    /// Open relative to `dirfd` with the caller's kernel resolver flags.
    pub(super) fn openat2(
        dirfd: RawFd,
        path: &CString,
        flags: libc::c_int,
        resolve: u64,
    ) -> std::io::Result<OwnedFd> {
        let how = OpenHow {
            flags: (flags as u64) | (libc::O_CLOEXEC as u64),
            mode: 0,
            resolve,
        };
        // SAFETY: `path` and `how` remain valid for this syscall.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                path.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful open returns a new, owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) })
    }

    pub(super) fn cstring(path: &Path) -> Result<CString, ToolFailure> {
        let bytes = path.as_os_str().as_bytes();
        let bytes: &[u8] = if bytes.is_empty() { b"." } else { bytes };
        CString::new(bytes).map_err(|_| {
            ToolFailure::refusal(
                super::CODE_PATH_OUTSIDE_ROOT,
                "path contains an interior NUL byte",
                Some(path),
            )
        })
    }

    pub(super) fn cname(name: &OsStr) -> Option<CString> {
        CString::new(name.as_bytes()).ok()
    }

    pub(super) fn fstat(fd: RawFd) -> std::io::Result<libc::stat> {
        // SAFETY: `stat` is a plain C data structure, and `fstat` writes to it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut st as *mut libc::stat) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(st)
    }

    pub(super) fn is_procfs(fd: RawFd) -> std::io::Result<bool> {
        // SAFETY: `statfs` is plain C data, and `fstatfs` writes to it.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(fd, &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(stat.f_type as u64 == 0x9fa0)
    }

    /// Read the target of the symlink an `O_PATH|O_NOFOLLOW` fd refers to, via
    /// empty-path `readlinkat` on that exact pinned inode (never re-walking the
    /// pathname, which could point at a different inode after the O_PATH open).
    /// Returns raw OS bytes so the target is resolved verbatim rather than
    /// through a lossy reinterpretation; grows the buffer and refuses
    /// (`ENAMETOOLONG`) a target too long to read whole, so a truncated target
    /// is never silently accepted.
    pub(super) fn readlink_fd(fd: RawFd, max_bytes: usize) -> std::io::Result<Vec<u8>> {
        let empty = cname(std::ffi::OsStr::new("")).expect("empty name has no NUL");
        let ceiling = max_bytes.saturating_add(1).max(1);
        let mut cap = 256usize.min(ceiling);
        loop {
            let mut buf = vec![0u8; cap];
            // SAFETY: the C string and writable buffer remain valid for this call.
            let n = unsafe {
                libc::readlinkat(
                    fd,
                    empty.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    buf.len(),
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let n = n as usize;
            if n < buf.len() && n <= max_bytes {
                buf.truncate(n);
                return Ok(buf);
            }
            if cap >= ceiling {
                return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
            }
            cap = cap.saturating_mul(2).min(ceiling);
        }
    }

    /// Reopen the inode an `O_PATH` descriptor refers to with real access flags.
    /// Goes through `/proc/self/fd/N`, so it targets the exact already-resolved
    /// inode rather than re-walking the (racey) path.
    pub(super) fn reopen(path_fd: RawFd, flags: libc::c_int) -> std::io::Result<OwnedFd> {
        let proc_path = format!("/proc/self/fd/{}", path_fd);
        let c = CString::new(proc_path).unwrap();
        // SAFETY: the C string remains valid for the open call.
        let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful open returns a new, owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
impl FilesystemAuthority {
    /// Build an authority confined to `root`. Canonicalizes and pins the root,
    /// then verifies the `openat2` resolver is available on this kernel.
    pub fn confined(root: &Path, limits: FsLimits) -> Result<Self, ToolFailure> {
        Self::confined_inner(root, limits, None)
    }

    #[cfg(test)]
    fn confined_with_root_observer(
        root: &Path,
        limits: FsLimits,
        observer: &dyn Fn(),
    ) -> Result<Self, ToolFailure> {
        Self::confined_inner(root, limits, Some(observer))
    }

    fn confined_inner(
        root: &Path,
        limits: FsLimits,
        observer: Option<&dyn Fn()>,
    ) -> Result<Self, ToolFailure> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        Self::probe_support()?;
        // Pin first. Bootstrap children may receive a parent-owned proc-fd path.
        let c = sys::cstring(root)?;
        // SAFETY: the C string remains valid for the open call.
        let raw = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(ToolFailure::unsupported(
                "confined root is not an openable directory",
            ));
        }
        // SAFETY: a successful open returns a new, owned descriptor.
        let root_fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let opened = std::fs::read_link(format!("/proc/self/fd/{}", root_fd.as_raw_fd()))
            .map_err(|_| ToolFailure::unsupported("cannot verify the pinned confined root"))?;
        if let Some(observer) = observer {
            observer();
        }
        let canonical = std::fs::canonicalize(root)
            .map_err(|_| ToolFailure::unsupported("confined root changed during initialization"))?;
        if opened != canonical {
            return Err(ToolFailure::unsupported(
                "confined root changed during initialization",
            ));
        }

        // Probe the resolver against the pinned root itself. ENOSYS means the
        // kernel has no openat2; refuse the confined profile explicitly.
        let dot = sys::cstring(Path::new("."))?;
        match sys::openat2(
            root_fd.as_raw_fd(),
            &dot,
            libc::O_PATH | libc::O_DIRECTORY,
            libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS,
        ) {
            Ok(_probe) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                return Err(ToolFailure::unsupported(
                    "kernel does not provide the openat2 resolver for confinement",
                ));
            }
            Err(e) => {
                return Err(ToolFailure::unsupported(format!(
                    "confinement resolver probe failed: {}",
                    e.kind()
                )));
            }
        }

        Ok(FilesystemAuthority {
            root: canonical,
            limits,
            root_fd,
            #[cfg(test)]
            pre_final_open: None,
            #[cfg(test)]
            post_walk_open: None,
        })
    }

    /// Startup gate: verify the confined resolver exists before services start.
    /// Returns `filesystem_policy_unsupported` when the kernel lacks `openat2`.
    pub fn probe_support() -> Result<(), ToolFailure> {
        let dot = sys::cstring(Path::new("."))?;
        // A relative path with an invalid fd checks the syscall and flags
        // without accessing a directory outside the selected root.
        match sys::openat2(
            -1,
            &dot,
            libc::O_PATH | libc::O_DIRECTORY,
            libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
        ) {
            Err(error) if error.raw_os_error() == Some(libc::EBADF) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
                Err(ToolFailure::unsupported(
                    "kernel does not provide the openat2 resolver for confinement",
                ))
            }
            Err(error) => Err(ToolFailure::unsupported(format!(
                "confinement resolver probe failed: {}",
                error.kind()
            ))),
            Ok(_) => Err(ToolFailure::unsupported(
                "confinement resolver accepted an invalid fd",
            )),
        }
    }

    /// Open `path` for reading, confined to the fixed root.
    ///
    /// A relative `path` is taken beneath the root. Tool callers first apply
    /// the session working directory with `Session::resolve_path`. Explicit
    /// out-of-root arguments are refused with `path_outside_root` before any
    /// open. The returned handle is the authorization: it names the exact inode
    /// the kernel resolved beneath the pinned root.
    pub fn open_read(&self, path: &Path) -> Result<AuthorizedRead, ToolFailure> {
        let rel = relativize(&self.root, path)?;
        let fd = self.resolve(path, &rel, libc::O_RDONLY)?;
        Ok(AuthorizedRead {
            file: std::fs::File::from(fd),
            relative_path: rel,
        })
    }

    /// Name this process's pinned root for a directly spawned child.
    pub fn bootstrap_reference(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.root_fd.as_raw_fd()
        ))
    }

    /// Identify the pinned inode when checking daemon reuse.
    pub fn bootstrap_identity(&self) -> Result<String, ToolFailure> {
        use std::os::fd::AsRawFd;
        let stat = sys::fstat(self.root_fd.as_raw_fd())
            .map_err(|_| ToolFailure::unsupported("cannot identify the pinned confined root"))?;
        Ok(format!("{}:{}", stat.st_dev, stat.st_ino))
    }

    /// Return the in-root path of an authorized directory without reopening it.
    pub fn directory_path(
        &self,
        argument: &Path,
        entry: &AuthorizedRead,
    ) -> Result<PathBuf, ToolFailure> {
        use std::os::fd::AsRawFd;
        let metadata = entry
            .metadata()
            .map_err(|error| map_open_err(argument, error))?;
        if !metadata.is_dir() {
            return Err(ToolFailure::refusal(
                "path_read_failed",
                "not a directory",
                Some(argument),
            ));
        }
        let path = std::fs::read_link(format!("/proc/self/fd/{}", entry.as_file().as_raw_fd()))
            .map_err(|error| map_open_err(argument, error))?;
        if !path.starts_with(&self.root) {
            return Err(ToolFailure::path_outside_root(argument));
        }
        Ok(path)
    }

    /// Describe an authorized symlink without reopening an ambient path.
    pub fn symlink_info(
        &self,
        path: &Path,
    ) -> Result<Option<(std::fs::Metadata, String, bool)>, ToolFailure> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStringExt;
        let relative = relativize(&self.root, path)?;
        let name = sys::cstring(&relative)?;
        let node = match sys::openat2(
            self.root_fd.as_raw_fd(),
            &name,
            libc::O_PATH | libc::O_NOFOLLOW,
            libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
        ) {
            Ok(node) => {
                let checked = self.resolve_manual(path, &relative, libc::O_PATH, false, true)?;
                let expected =
                    sys::fstat(node.as_raw_fd()).map_err(|error| map_open_err(path, error))?;
                let actual =
                    sys::fstat(checked.as_raw_fd()).map_err(|error| map_open_err(path, error))?;
                if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
                    return Err(ToolFailure::refusal(
                        "path_read_failed",
                        "path changed during authorization",
                        Some(path),
                    ));
                }
                node
            }
            Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
                self.resolve_manual(path, &relative, libc::O_PATH, true, true)?
            }
            Err(error) => return Err(map_open_err(path, error)),
        };
        let stat = sys::fstat(node.as_raw_fd()).map_err(|error| map_open_err(path, error))?;
        if (stat.st_mode as u32) & (libc::S_IFMT) != libc::S_IFLNK {
            return Ok(None);
        }
        // Procfs can contain magic links. Do not expose their target text.
        if sys::is_procfs(node.as_raw_fd()).unwrap_or(true) {
            return Ok(None);
        }
        let target = sys::readlink_fd(node.as_raw_fd(), self.limits.max_symlink_target_bytes)
            .map_err(|error| map_link_error(path, self.limits.max_symlink_target_bytes, error))?;
        let target = PathBuf::from(std::ffi::OsString::from_vec(target));
        let target_path = if target.is_absolute() {
            target.clone()
        } else {
            relative.parent().unwrap_or(Path::new("")).join(&target)
        };
        // Check the full caller path for the total link budget, and check the
        // pinned link target separately so a concurrent swap cannot expose it.
        let mut broken = false;
        for requested in [path, target_path.as_path()] {
            match self.open_read(requested) {
                Ok(_) => {}
                Err(error) if error.code == CODE_PATH_NOT_FOUND => broken = true,
                Err(error) if error.code == CODE_PATH_OUTSIDE_ROOT => {
                    return Err(ToolFailure::symlink_refused(path, None));
                }
                Err(error) => return Err(error),
            }
        }
        let metadata = std::fs::File::from(node)
            .metadata()
            .map_err(|error| map_open_err(path, error))?;
        Ok(Some((
            metadata,
            target.to_string_lossy().into_owned(),
            broken,
        )))
    }

    /// Resolve atomically with the kernel's magic-link guard, then enforce the
    /// configured link budget against the same inode (ADR-022).
    fn resolve(
        &self,
        argument: &Path,
        rel: &Path,
        final_flags: libc::c_int,
    ) -> Result<std::os::fd::OwnedFd, ToolFailure> {
        use std::os::fd::AsRawFd;
        let requested = sys::cstring(rel)?;
        let node = match sys::openat2(
            self.root_fd.as_raw_fd(),
            &requested,
            libc::O_PATH,
            libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
        ) {
            Ok(node) => node,
            Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
                // Absolute in-root links require manual translation. Procfs links
                // are refused in this fallback because they may be magic links.
                return self.resolve_manual(argument, rel, final_flags, true, false);
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                // Distinguish a configured link-budget failure from a magic
                // link without following procfs links in this fallback.
                return self.resolve_manual(argument, rel, final_flags, true, false);
            }
            Err(error) => return Err(map_open_err(argument, error)),
        };
        let checked = self.resolve_manual(argument, rel, final_flags, false, false)?;
        let expected =
            sys::fstat(node.as_raw_fd()).map_err(|error| map_open_err(argument, error))?;
        let actual =
            sys::fstat(checked.as_raw_fd()).map_err(|error| map_open_err(argument, error))?;
        if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
            return Err(ToolFailure::refusal(
                "path_read_failed",
                "path changed during authorization",
                Some(argument),
            ));
        }
        let mode = (expected.st_mode as u32) & (libc::S_IFMT);
        finalize_node(argument, &node, mode, final_flags)
    }

    /// Resolve `rel` (already root-relative) against the pinned root, following
    /// symlinks in userspace so an in-root link with an absolute real target
    /// still resolves while any link that escapes the root is refused. Every
    /// component is opened from `root_fd` with the WHOLE accumulated relative
    /// path (`RESOLVE_BENEATH`), so containment is re-proven against the fixed
    /// root atomically at each step: a previously resolved ancestor that is
    /// moved or replaced can never become an independent authority outside the
    /// root. Returns a handle reopened on the exact resolved inode.
    fn resolve_manual(
        &self,
        argument: &Path,
        rel: &Path,
        final_flags: libc::c_int,
        reject_procfs_links: bool,
        leave_final_symlink: bool,
    ) -> Result<std::os::fd::OwnedFd, ToolFailure> {
        use std::collections::VecDeque;
        use std::ffi::OsString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStringExt;

        fn components(p: &Path, from_symlink: bool) -> VecDeque<(OsString, bool)> {
            p.components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some((s.to_os_string(), from_symlink)),
                    Component::ParentDir => Some((OsString::from(".."), from_symlink)),
                    _ => None,
                })
                .collect()
        }

        let mut pending = components(rel, false);
        // Real directory components resolved so far, relative to the root. Never
        // holds a symlink (those are expanded in place), so each `openat2` below
        // only NOFOLLOWs its own final component.
        let mut resolved = PathBuf::new();
        let mut symlink_budget = self.limits.max_symlinks;

        while let Some((comp, from_symlink)) = pending.pop_front() {
            if comp == ".." {
                if !resolved.pop() {
                    return Err(if from_symlink {
                        ToolFailure::symlink_refused(argument, None)
                    } else {
                        ToolFailure::path_outside_root(argument)
                    });
                }
                continue;
            }
            let is_last = pending.is_empty();
            let candidate = resolved.join(&comp);
            let cand_c = sys::cstring(&candidate)?;

            #[cfg(test)]
            if is_last {
                if let Some(obs) = self.pre_final_open.as_ref() {
                    (obs.0)();
                }
            }

            // Atomic open from the pinned root with the whole accumulated path.
            let node = sys::openat2(
                self.root_fd.as_raw_fd(),
                &cand_c,
                libc::O_PATH | libc::O_NOFOLLOW,
                libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
            )
            .map_err(|e| map_open_err(argument, e))?;

            let st = sys::fstat(node.as_raw_fd()).map_err(|e| map_open_err(argument, e))?;
            let mode = (st.st_mode as u32) & (libc::S_IFMT);

            if mode == libc::S_IFLNK {
                if reject_procfs_links && sys::is_procfs(node.as_raw_fd()).unwrap_or(true) {
                    return Err(ToolFailure::symlink_refused(argument, None));
                }
                if symlink_budget == 0 {
                    return Err(ToolFailure::budget_limit(
                        argument,
                        "max_symlinks",
                        self.limits.max_symlinks as usize,
                        (self.limits.max_symlinks as usize).saturating_add(1),
                    ));
                }
                symlink_budget -= 1;
                if is_last && leave_final_symlink {
                    return Ok(node);
                }
                // Read the exact pinned symlink inode (empty-path readlinkat on
                // the O_PATH|O_NOFOLLOW fd), preserving raw OS bytes.
                let target =
                    sys::readlink_fd(node.as_raw_fd(), self.limits.max_symlink_target_bytes)
                        .map_err(|error| {
                            map_link_error(argument, self.limits.max_symlink_target_bytes, error)
                        })?;
                let tpath = PathBuf::from(OsString::from_vec(target.clone()));
                let front = if tpath.is_absolute() {
                    match tpath.strip_prefix(&self.root) {
                        Ok(rest) => {
                            resolved.clear(); // absolute target: restart at root
                            components(rest, true)
                        }
                        Err(_) => {
                            let text = String::from_utf8_lossy(&target);
                            return Err(ToolFailure::symlink_refused(argument, Some(&text)));
                        }
                    }
                } else {
                    components(&tpath, true) // relative: resolve from the link's parent
                };
                for c in front.into_iter().rev() {
                    pending.push_front(c);
                }
                continue;
            }

            if is_last {
                return finalize_node(argument, &node, mode, final_flags);
            }
            if mode != libc::S_IFDIR {
                return Err(ToolFailure::refusal(
                    "path_read_failed",
                    "a path component is not a directory",
                    Some(argument),
                ));
            }
            resolved = candidate; // confirmed real directory; advance
        }

        // No trailing file component (e.g. "." or a directory target).
        let tail = if resolved.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            resolved.clone()
        };
        let tail_c = sys::cstring(&tail)?;
        let node = sys::openat2(
            self.root_fd.as_raw_fd(),
            &tail_c,
            libc::O_PATH | libc::O_DIRECTORY,
            libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
        )
        .map_err(|e| map_open_err(argument, e))?;
        let st = sys::fstat(node.as_raw_fd()).map_err(|e| map_open_err(argument, e))?;
        let mode = (st.st_mode as u32) & (libc::S_IFMT);
        finalize_node(argument, &node, mode, final_flags)
    }
}

/// Map a raw `openat2` failure into a bounded, leak-free refusal.
#[cfg(target_os = "linux")]
fn map_link_error(argument: &Path, configured: usize, error: std::io::Error) -> ToolFailure {
    if error.raw_os_error() == Some(libc::ENAMETOOLONG) {
        ToolFailure::budget_limit(
            argument,
            "max_symlink_target_bytes",
            configured,
            configured.saturating_add(1),
        )
    } else {
        ToolFailure::symlink_refused(argument, None)
    }
}

#[cfg(target_os = "linux")]
fn map_open_err(argument: &Path, e: std::io::Error) -> ToolFailure {
    match e.raw_os_error() {
        // A forbidden magic link or a symlink loop under RESOLVE_NO_MAGICLINKS.
        Some(libc::ELOOP) => ToolFailure::symlink_refused(argument, None),
        Some(libc::ENOENT) => {
            ToolFailure::refusal(CODE_PATH_NOT_FOUND, "path does not exist", Some(argument))
        }
        _ => ToolFailure::refusal(
            // Non-policy I/O outcome (e.g. not found, permission). Passed through
            // for the integrator's existing read error surfacing; not a reviewed
            // confinement refusal. See integration notes.
            "path_read_failed",
            format!("open failed: {}", e.kind()),
            Some(argument),
        ),
    }
}

/// Final step shared by file and directory resolution: refuse special files
/// (device/FIFO/socket) BEFORE a potentially blocking `O_RDONLY` reopen, then
/// reopen the already-resolved inode through its `O_PATH` fd (no path re-walk).
/// A FIFO/device/socket open can block indefinitely before any cooperative
/// cancellation, so it is refused up front rather than pretending a post-open
/// timeout could cancel it. Directories stay supported (safe walks).
#[cfg(target_os = "linux")]
fn finalize_node(
    argument: &Path,
    node: &std::os::fd::OwnedFd,
    mode: u32,
    final_flags: libc::c_int,
) -> Result<std::os::fd::OwnedFd, ToolFailure> {
    use std::os::fd::AsRawFd;
    let is_reg = mode == libc::S_IFREG;
    let is_dir = mode == libc::S_IFDIR;
    if !is_reg && !is_dir {
        return Err(ToolFailure::refusal(
            CODE_UNSUPPORTED_FILE_TYPE,
            "unsupported special file (device, FIFO, or socket) cannot be read",
            Some(argument),
        ));
    }
    sys::reopen(node.as_raw_fd(), final_flags).map_err(|e| map_open_err(argument, e))
}

#[cfg(not(target_os = "linux"))]
impl FilesystemAuthority {
    pub fn confined(_root: &Path, _limits: FsLimits) -> Result<Self, ToolFailure> {
        Err(ToolFailure::unsupported(
            "confined filesystem profile requires the Linux openat2 resolver; \
             no certified resolver on this platform",
        ))
    }

    pub fn probe_support() -> Result<(), ToolFailure> {
        Err(ToolFailure::unsupported(
            "confined filesystem profile requires the Linux openat2 resolver; \
             no certified resolver on this platform",
        ))
    }

    pub fn open_read(&self, path: &Path) -> Result<AuthorizedRead, ToolFailure> {
        Err(ToolFailure::unsupported(
            "confinement unsupported on this platform",
        ))
        .map_err(|mut f: ToolFailure| {
            f.argument = Some(path.to_string_lossy().into_owned());
            f
        })
    }

    pub fn bootstrap_reference(&self) -> PathBuf {
        self.root.clone()
    }

    pub fn bootstrap_identity(&self) -> Result<String, ToolFailure> {
        Err(ToolFailure::unsupported(
            "confinement unsupported on this platform",
        ))
    }

    pub fn symlink_info(
        &self,
        path: &Path,
    ) -> Result<Option<(std::fs::Metadata, String, bool)>, ToolFailure> {
        let mut f = ToolFailure::unsupported("confinement unsupported on this platform");
        f.argument = Some(path.to_string_lossy().into_owned());
        Err(f)
    }

    pub fn directory_path(
        &self,
        path: &Path,
        _entry: &AuthorizedRead,
    ) -> Result<PathBuf, ToolFailure> {
        let mut f = ToolFailure::unsupported("confinement unsupported on this platform");
        f.argument = Some(path.to_string_lossy().into_owned());
        Err(f)
    }

    pub fn walk(&self, path: &Path, _opts: WalkOptions) -> Result<Walk, ToolFailure> {
        let mut f = ToolFailure::unsupported("confinement unsupported on this platform");
        f.argument = Some(path.to_string_lossy().into_owned());
        Err(f)
    }
}

#[cfg(target_os = "linux")]
impl FilesystemAuthority {
    /// Enumerate `path` through the fixed root. In-root links may be followed.
    /// Refused or unreadable entries are counted in [`Walk::skipped`].
    pub fn walk(&self, path: &Path, opts: WalkOptions) -> Result<Walk, ToolFailure> {
        let start_rel = relativize(&self.root, path)?;
        self.resolve(path, &start_rel, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let effective_depth = match (opts.max_depth, self.limits.max_walk_depth) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let mut walk = Walk::default();
        let mut budget = self.limits().max_walk_entries;
        self.walk_dir(
            &start_rel,
            0,
            effective_depth,
            &opts,
            &mut walk,
            &mut budget,
        );
        if let Some(failure) = walk.failure.take() {
            return Err(failure);
        }
        if walk.exhausted {
            return Err(ToolFailure::walk_limit(
                path,
                self.limits.max_walk_entries,
                self.limits.max_walk_entries,
            ));
        }
        if let (Some(configured), Some(consumed)) =
            (self.limits.max_walk_depth, walk.depth_exhausted)
        {
            return Err(ToolFailure::budget_limit(
                path,
                "max_walk_depth",
                configured,
                consumed,
            ));
        }
        Ok(walk)
    }

    fn walk_dir(
        &self,
        rel: &Path,
        depth: usize,
        max_depth: Option<usize>,
        opts: &WalkOptions,
        walk: &mut Walk,
        budget: &mut usize,
    ) {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let dir_fd = match self.resolve(rel, rel, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(fd) => fd,
            Err(f) => {
                walk.skipped.push(SkippedEntry {
                    relative_path: rel.to_path_buf(),
                    reason: format!("directory not readable ({})", f.code),
                    link_target: None,
                });
                return;
            }
        };

        #[cfg(test)]
        if let Some(observer) = &self.post_walk_open {
            (observer.0)();
        }
        let stat = match sys::fstat(std::os::fd::AsRawFd::as_raw_fd(&dir_fd)) {
            Ok(stat) => stat,
            Err(_) => {
                walk.skipped.push(SkippedEntry {
                    relative_path: rel.to_path_buf(),
                    reason: "directory metadata unavailable".to_string(),
                    link_target: None,
                });
                return;
            }
        };
        let identity = (stat.st_dev as u64, stat.st_ino as u64);
        if walk.ancestors.contains(&identity) {
            walk.skipped.push(SkippedEntry {
                relative_path: rel.to_path_buf(),
                reason: "directory cycle".to_string(),
                link_target: None,
            });
            return;
        }
        walk.ancestors.push(identity);
        let raw = dir_fd.into_raw_fd();
        // SAFETY: ownership of `raw` transfers to `fdopendir` on success.
        let dirp = unsafe { libc::fdopendir(raw) };
        if dirp.is_null() {
            // SAFETY: `fdopendir` failed, so this code still owns `raw`.
            unsafe { libc::close(raw) };
            walk.failure = Some(ToolFailure::refusal(
                "path_read_failed",
                "cannot enumerate authorized directory",
                Some(rel),
            ));
            walk.ancestors.pop();
            return;
        }

        // Collect children while the directory handle is open, then close it
        // before recursing to avoid holding many descriptors at once.
        let mut subdirs: Vec<PathBuf> = Vec::new();
        loop {
            // SAFETY: errno is thread-local and `dirp` is open on this thread.
            unsafe { *libc::__errno_location() = 0 };
            // SAFETY: `dirp` is open and is not used concurrently.
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                // SAFETY: errno is thread-local and remains valid on this thread.
                let errno = unsafe { *libc::__errno_location() };
                if errno != 0 {
                    walk.failure = Some(ToolFailure::refusal(
                        "path_read_failed",
                        "directory enumeration failed",
                        Some(rel),
                    ));
                }
                break;
            }
            // SAFETY: `readdir` returned a live entry with a terminated name.
            let name_ptr = unsafe { (*ent).d_name.as_ptr() };
            let name_c = unsafe { std::ffi::CStr::from_ptr(name_ptr) };
            let name_bytes = name_c.to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            if *budget == 0 {
                walk.exhausted = true;
                break;
            }
            let name = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name_bytes)
                .to_owned();
            let child_rel = rel.join(&name);

            *budget -= 1;
            if !opts.include_hidden && name_bytes.first() == Some(&b'.') {
                continue;
            }
            // Reauthorize each name against the pinned root. A directory handle
            // can be moved after readdir starts, so it cannot authorize children.
            let authorized = match self.open_read(&child_rel) {
                Ok(entry) => entry,
                Err(error) => {
                    walk.skipped.push(SkippedEntry {
                        relative_path: rel.to_path_buf(),
                        reason: format!("entry skipped ({})", error.code),
                        link_target: None,
                    });
                    continue;
                }
            };
            let metadata = match authorized.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    walk.skipped.push(SkippedEntry {
                        relative_path: rel.to_path_buf(),
                        reason: "entry metadata unavailable".to_string(),
                        link_target: None,
                    });
                    continue;
                }
            };
            let (kind, link_target) = classify_at(
                self.root_fd.as_raw_fd(),
                &child_rel,
                self.limits.max_symlink_target_bytes,
            )
            .unwrap_or_else(|| {
                (
                    if metadata.is_dir() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    },
                    None,
                )
            });
            if metadata.is_dir() && opts.skip_dirs.iter().any(|skip| name == *skip) {
                continue;
            }
            let directory =
                metadata.is_dir() && (kind != EntryKind::Symlink || opts.follow_symlinks);
            if directory
                && self.limits.max_walk_depth.is_some_and(|configured| {
                    depth >= configured
                        && opts
                            .max_depth
                            .is_none_or(|requested| requested > configured)
                })
            {
                walk.depth_exhausted = Some(depth.saturating_add(1));
                break;
            }
            let descend = directory && !opts_blocks_descent(opts, depth, max_depth);
            walk.entries.push(WalkEntry {
                relative_path: child_rel.clone(),
                kind,
                metadata,
                link_target,
            });
            if descend {
                subdirs.push(child_rel);
            }
        }
        // SAFETY: `dirp` is open and is closed exactly once here.
        unsafe { libc::closedir(dirp) };

        for sub in subdirs {
            if walk.exhausted || walk.depth_exhausted.is_some() || walk.failure.is_some() {
                break;
            }
            self.walk_dir(&sub, depth + 1, max_depth, opts, walk, budget);
        }
        walk.ancestors.pop();
    }
}

#[cfg(target_os = "linux")]
fn opts_blocks_descent(_opts: &WalkOptions, depth: usize, max_depth: Option<usize>) -> bool {
    match max_depth {
        Some(d) => depth >= d,
        None => false,
    }
}

/// Classify a name by reopening it beneath the fixed root without following its leaf.
#[cfg(target_os = "linux")]
fn classify_at(
    rootfd: libc::c_int,
    relative: &Path,
    max_target_bytes: usize,
) -> Option<(EntryKind, Option<String>)> {
    use std::os::fd::AsRawFd;
    let path = sys::cstring(relative).ok()?;
    let node = sys::openat2(
        rootfd,
        &path,
        libc::O_PATH | libc::O_NOFOLLOW,
        libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
    )
    .ok()?;
    let stat = sys::fstat(node.as_raw_fd()).ok()?;
    let mode = (stat.st_mode as u32) & (libc::S_IFMT);
    if mode == libc::S_IFLNK {
        Some((
            EntryKind::Symlink,
            read_link_at(node.as_raw_fd(), max_target_bytes),
        ))
    } else if mode == libc::S_IFDIR {
        Some((EntryKind::Dir, None))
    } else if mode == libc::S_IFREG {
        Some((EntryKind::File, None))
    } else {
        Some((EntryKind::Other, None))
    }
}

#[cfg(target_os = "linux")]
fn read_link_at(fd: libc::c_int, max_bytes: usize) -> Option<String> {
    sys::readlink_fd(fd, max_bytes)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    fn authority(root: &Path) -> FilesystemAuthority {
        FilesystemAuthority::confined(root, FsLimits::default()).expect("confined authority")
    }

    fn read_to_string(auth: &FilesystemAuthority, p: &str) -> Result<String, ToolFailure> {
        let mut f = auth.open_read(Path::new(p))?.into_file();
        let mut s = String::new();
        f.read_to_string(&mut s)
            .map_err(|e| ToolFailure::refusal("path_read_failed", format!("{}", e.kind()), None))?;
        Ok(s)
    }

    #[test]
    fn probe_support_ok_on_this_kernel() {
        FilesystemAuthority::probe_support().expect("openat2 available");
    }

    #[test]
    fn rejects_magic_link_even_when_target_is_inside_root() {
        use std::os::fd::AsRawFd;
        let auth = authority(Path::new("/proc"));
        let path = format!("self/fd/{}/version", auth.root_fd.as_raw_fd());
        let error = auth.open_read(Path::new(&path)).unwrap_err();
        assert_eq!(error.code, CODE_SYMLINK_REFUSED);
    }

    #[test]
    fn permits_ordinary_in_root_proc_symlink() {
        let auth = authority(Path::new("/proc"));
        let content = read_to_string(&auth, "self/status").unwrap();
        assert!(content.contains("Name:"));
    }

    #[test]
    fn root_ancestor_swap_during_initialization_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("parent");
        let replacement = base.path().join("replacement");
        std::fs::create_dir_all(parent.join("root")).unwrap();
        std::fs::create_dir_all(replacement.join("root")).unwrap();
        let old = base.path().join("old-parent");
        let hook = || {
            std::fs::rename(&parent, &old).unwrap();
            symlink(&replacement, &parent).unwrap();
        };
        let error = FilesystemAuthority::confined_with_root_observer(
            &parent.join("root"),
            FsLimits::default(),
            &hook,
        )
        .unwrap_err();
        assert_eq!(error.code, CODE_FILESYSTEM_POLICY_UNSUPPORTED);
    }

    #[test]
    fn supports_configured_symlink_budget_above_kernel_single_lookup_limit() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("target"), "inside").unwrap();
        for index in (0..45).rev() {
            let target = if index == 44 {
                "target".to_string()
            } else {
                format!("link{}", index + 1)
            };
            symlink(target, directory.path().join(format!("link{index}"))).unwrap();
        }
        let limits = FsLimits {
            max_symlinks: 50,
            ..FsLimits::default()
        };
        let auth = FilesystemAuthority::confined(directory.path(), limits).unwrap();
        assert_eq!(read_to_string(&auth, "link0").unwrap(), "inside");
    }

    #[test]
    fn reads_in_root_file_by_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let auth = authority(dir.path());
        assert_eq!(read_to_string(&auth, "a.txt").unwrap(), "hello");
    }

    #[test]
    fn relative_resolves_against_fixed_root_not_cwd() {
        // The authority never consults process/session cwd; a bare relative name
        // always resolves beneath the pinned root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "nested").unwrap();
        let auth = authority(dir.path());
        assert_eq!(read_to_string(&auth, "sub/b.txt").unwrap(), "nested");
    }

    #[test]
    fn absolute_in_root_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.txt"), "abs").unwrap();
        let auth = authority(dir.path());
        let abs = auth.root().join("c.txt");
        let got = read_to_string(&auth, abs.to_str().unwrap()).unwrap();
        assert_eq!(got, "abs");
    }

    #[test]
    fn absolute_out_of_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let auth = authority(dir.path());
        let err = auth.open_read(Path::new("/etc/passwd")).unwrap_err();
        assert_eq!(err.code, CODE_PATH_OUTSIDE_ROOT);
        // No resolved outside location leaked; only the caller's argument.
        assert_eq!(err.argument.as_deref(), Some("/etc/passwd"));
    }

    #[test]
    fn relative_parent_escape_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let auth = authority(dir.path());
        let err = auth.open_read(Path::new("../../etc/passwd")).unwrap_err();
        assert_eq!(err.code, CODE_PATH_OUTSIDE_ROOT);
    }

    #[test]
    fn in_root_symlink_relative_target_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "linked").unwrap();
        symlink("real.txt", dir.path().join("rel_link.txt")).unwrap();
        let auth = authority(dir.path());
        assert_eq!(read_to_string(&auth, "rel_link.txt").unwrap(), "linked");
    }

    #[test]
    fn in_root_symlink_absolute_target_is_followed() {
        // The RESOLVE_BENEATH pitfall: an in-root link whose target is an
        // absolute path inside the root must still resolve. RESOLVE_IN_ROOT does.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "abs-linked").unwrap();
        let auth = authority(dir.path());
        let abs_target = auth.root().join("real.txt");
        symlink(&abs_target, dir.path().join("abs_link.txt")).unwrap();
        assert_eq!(read_to_string(&auth, "abs_link.txt").unwrap(), "abs-linked");
    }

    #[test]
    fn escaping_symlink_does_not_leak_outside_content() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "TOP-SECRET").unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Absolute symlink pointing outside the root.
        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("escape.txt"),
        )
        .unwrap();
        let auth = authority(dir.path());
        // Under RESOLVE_IN_ROOT the absolute target is reinterpreted beneath the
        // root, so it never reads the outside secret.
        match read_to_string(&auth, "escape.txt") {
            Ok(s) => assert_ne!(s, "TOP-SECRET", "must not read outside content"),
            Err(f) => assert_ne!(f.code, "ok"),
        }
    }

    #[test]
    fn hostile_ancestor_replacement_stays_confined() {
        // Pin the authority, then swap an in-root subdirectory for a symlink to
        // an outside tree. Because opens are relative to the pinned root fd and
        // use RESOLVE_IN_ROOT, the replaced component cannot redirect outside.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("loot.txt"), "LOOT").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner.txt"), "inner").unwrap();
        let auth = authority(dir.path());

        std::fs::remove_dir_all(dir.path().join("sub")).unwrap();
        symlink(outside.path(), dir.path().join("sub")).unwrap();

        if let Ok(s) = read_to_string(&auth, "sub/loot.txt") {
            assert_ne!(s, "LOOT", "must not follow swapped ancestor outside");
        }
    }

    #[test]
    fn walk_reports_entries_and_symlink_targets_without_descending() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f1.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("d1")).unwrap();
        std::fs::write(dir.path().join("d1/f2.txt"), "y").unwrap();
        symlink("f1.txt", dir.path().join("link1")).unwrap();
        let auth = authority(dir.path());

        let walk = auth.walk(Path::new("."), WalkOptions::default()).unwrap();
        let names: Vec<String> = walk
            .entries
            .iter()
            .map(|e| e.relative_path.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == "f1.txt"),
            "entries={names:?}; skipped={:?}",
            walk.skipped
        );
        assert!(names.iter().any(|n| n == "d1"));
        assert!(names.iter().any(|n| n == "d1/f2.txt"));

        let link = walk
            .entries
            .iter()
            .find(|e| e.relative_path.to_string_lossy() == "link1")
            .expect("link entry present");
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(link.link_target.as_deref(), Some("f1.txt"));
    }

    #[test]
    fn walk_does_not_descend_symlinked_directory_outside_root() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("hidden.txt"), "nope").unwrap();
        let dir = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("portal")).unwrap();
        let auth = authority(dir.path());

        let walk = auth.walk(Path::new("."), WalkOptions::default()).unwrap();
        // The escaping symlink is skipped, and no outside entry appears.
        assert!(!walk
            .entries
            .iter()
            .any(|e| e.relative_path.to_string_lossy() == "portal"));
        assert!(!walk.skipped.is_empty());
        assert!(!walk
            .entries
            .iter()
            .any(|e| e.relative_path.to_string_lossy().contains("hidden.txt")));
    }

    #[test]
    fn moved_walk_directory_cannot_expose_new_outside_names() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let mut auth = authority(root.path());
        let source = root.path().join("sub");
        let moved = outside.path().join("moved");
        let once = std::sync::atomic::AtomicBool::new(false);
        auth.set_post_walk_open_observer(move || {
            if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::fs::rename(&source, &moved).unwrap();
                std::fs::create_dir(&source).unwrap();
                std::fs::write(moved.join("OUTSIDE_SECRET_NAME"), "secret").unwrap();
            }
        });
        let walk = auth.walk(Path::new("sub"), WalkOptions::default()).unwrap();
        assert!(!walk.entries.iter().any(|entry| entry
            .relative_path
            .to_string_lossy()
            .contains("OUTSIDE_SECRET_NAME")));
        assert!(!walk.skipped.iter().any(|entry| entry
            .relative_path
            .to_string_lossy()
            .contains("OUTSIDE_SECRET_NAME")));
    }

    #[test]
    fn walk_preserves_non_utf8_entry_names() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let directory = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"name\xff.txt".to_vec());
        std::fs::write(directory.path().join(&name), "inside").unwrap();
        let auth = authority(directory.path());
        let walk = auth.walk(Path::new("."), WalkOptions::default()).unwrap();
        assert!(walk
            .entries
            .iter()
            .any(|entry| entry.relative_path.as_os_str().as_bytes() == name.as_bytes()));
    }

    #[test]
    fn walk_respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/deep.txt"), "z").unwrap();
        let auth = authority(dir.path());

        let shallow = auth
            .walk(
                Path::new("."),
                WalkOptions {
                    max_depth: Some(0),
                    follow_symlinks: false,
                    ..WalkOptions::default()
                },
            )
            .unwrap();
        assert!(shallow
            .entries
            .iter()
            .any(|e| e.relative_path.to_string_lossy() == "a"));
        assert!(!shallow
            .entries
            .iter()
            .any(|e| e.relative_path.to_string_lossy().contains("deep.txt")));
    }

    // --- Security-review regressions (controller follow-up) ---

    // Regression 1: a resolved ancestor must not become an independent authority.
    // Win the pre-final-open race deterministically: after `a` is resolved, move
    // it outside the root and plant an outside-only secret, then let the final
    // open proceed. Authority must refuse, never return the secret.
    #[test]
    fn moved_ancestor_cannot_become_independent_authority() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let mut auth = authority(dir.path());
        let root = dir.path().to_path_buf();
        let out = outside.path().to_path_buf();
        auth.set_pre_final_open_observer(move || {
            let moved = out.join("moved_a");
            let _ = std::fs::rename(root.join("a"), &moved);
            let _ = std::fs::write(moved.join("secret.txt"), "OUTSIDE-ONLY-SECRET");
        });
        if let Ok(s) = read_to_string(&auth, "a/secret.txt") {
            assert_ne!(
                s, "OUTSIDE-ONLY-SECRET",
                "must not read moved-outside secret"
            );
        }
    }

    // Regression 2: a relative symlink target that climbs above the root must be
    // refused, not clamped to a same-named in-root file (shadow name).
    #[test]
    fn relative_parent_symlink_escape_refused_despite_shadow_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("outside.txt"), "INSIDE-SHADOW").unwrap();
        symlink("../outside.txt", dir.path().join("escape")).unwrap();
        let auth = authority(dir.path());
        let err = auth.open_read(Path::new("escape")).unwrap_err();
        assert_eq!(err.code, CODE_SYMLINK_REFUSED);
        // Never reinterpreted as the shadow file's content.
        assert!(read_to_string(&auth, "escape").is_err());
    }

    // Regression 3: the symlink target is read from the pinned O_PATH inode and
    // preserved as raw OS bytes. A non-UTF8 in-root target still resolves; a
    // lossy String reinterpretation would corrupt the name and fail.
    #[test]
    fn symlink_target_raw_bytes_preserved_through_node_fd() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let real_name = std::ffi::OsStr::from_bytes(b"real_\xff.txt");
        let real_path = dir.path().join(real_name);
        std::fs::write(&real_path, "raw-bytes-ok").unwrap();
        symlink(&real_path, dir.path().join("nlink")).unwrap();
        let auth = authority(dir.path());
        assert_eq!(read_to_string(&auth, "nlink").unwrap(), "raw-bytes-ok");
    }

    // Regression 4: a FIFO (special file) is refused up front, not reopened
    // O_RDONLY where it could block before cooperative cancellation.
    #[test]
    fn fifo_is_refused_before_blocking_open() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the C string remains valid for the `mkfifo` call.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed");
        let auth = authority(dir.path());
        let err = auth.open_read(Path::new("pipe")).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED_FILE_TYPE);
    }
}
