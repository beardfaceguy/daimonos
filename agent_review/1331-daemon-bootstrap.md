# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1331-daemon-bootstrap-20260824" -->

<!-- event id="request" artifact path="1331-daemon-bootstrap/artifacts/round-1-review-request.diff" sha256="904ee24f7a14f6c8db97d35ee8792c6d62734c2baf1a5bcd9492b6df8c58d4aa" -->
## Review Request — Round 1
**Task:** 1331 — Automatic session-daemon bootstrap for interactive TUI
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
On interactive startup, connect to the configured owner-only Unix socket first. Only for absent/refused sockets, spawn the current daimonos executable as a fully stdio-detached session-daemon in its own process group, forwarding workspace, explicit config, provider, model, and agent-env. Retry connection on configurable bounds through concurrent flock races. Publish atomic mode-0600 PID/version/socket/fingerprint metadata beside the socket and remove it with the socket guard. A SHA-256 fingerprint lets concurrent launchers with the same daemon-owned selections join the winner without silently accepting different provider/env settings. Never kill version-skewed or pre-existing daemons.

### Relevant Code / Diff
```rust
// src/session_bootstrap.rs
pub const BOOTSTRAP_FINGERPRINT_ENV: &str = "DAIMONOS_SESSION_BOOTSTRAP_FINGERPRINT";

pub struct BootstrapOptions<'a> {
    pub workspace: &'a Path,
    pub config_path: Option<&'a Path>,
    pub socket_path: &'a Path,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub agent_env: Option<&'a Path>,
    pub max_frame_bytes: usize,
    pub timeout: Duration,
    pub retry_interval: Duration,
}

pub struct BootstrapConnection {
    pub transport: UnixFrontendTransport,
    pub accepted_launch_options: bool,
}

pub async fn connect_or_spawn(options: &BootstrapOptions<'_>) -> anyhow::Result<BootstrapConnection> {
    match UnixStream::connect(options.socket_path).await {
        Ok(stream) => return Ok(BootstrapConnection {
            transport: frontend_transport(stream, options)?,
            accepted_launch_options: instance_fingerprint(options.socket_path).ok().flatten().as_deref()
                == Some(bootstrap_fingerprint(options).as_str()),
        }),
        Err(error) if should_bootstrap(&error) => {}
        Err(error) => bail!("cannot connect to session daemon at {}: {error}", options.socket_path.display()),
    }

    let executable = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(&executable);
    command.args(daemon_args(options))
        .env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options))
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .kill_on_drop(false);
    command.as_std_mut().process_group(0);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + options.timeout;
    let mut child_status = None;

    loop {
        match UnixStream::connect(options.socket_path).await {
            Ok(stream) => {
                if child.try_wait()?.is_none() {
                    tokio::spawn(async move { let _ = child.wait().await; });
                }
                return Ok(BootstrapConnection {
                    transport: frontend_transport(stream, options)?,
                    accepted_launch_options: instance_fingerprint(options.socket_path).ok().flatten().as_deref()
                        == Some(bootstrap_fingerprint(options).as_str()),
                });
            }
            Err(error) if should_bootstrap(&error) => {}
            Err(error) => bail!("cannot connect to bootstrapped session daemon at {}: {error}", options.socket_path.display()),
        }
        if child_status.is_none() { child_status = child.try_wait()?; }
        let now = Instant::now();
        if now >= deadline { break; }
        sleep(options.retry_interval.min(deadline - now)).await;
    }
    if child_status.is_none() && child.try_wait()?.is_none() {
        tokio::spawn(async move { let _ = child.wait().await; });
    }
    let status = child_status.map(|s| format!("; spawned daemon exited with {s}")).unwrap_or_default();
    bail!("timed out waiting for session daemon at {}{status}; run `daimonos --workspace {} session-daemon --socket {}` directly for startup diagnostics", options.socket_path.display(), options.workspace.display(), options.socket_path.display())
}

fn should_bootstrap(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused)
}

fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
    // --workspace, optional --config, session-daemon --socket,
    // optional --provider/--model/--agent-env, preserving OsString paths
}

fn bootstrap_fingerprint(options: &BootstrapOptions<'_>) -> String {
    // SHA-256 over nul-delimited workspace/config/socket/provider/model/agent-env bytes
}
```

```rust
// src/agent_runtime.rs interactive branch
let bootstrap = session_bootstrap::connect_or_spawn(&BootstrapOptions {
    workspace,
    config_path: explicit_config_path.as_deref(),
    socket_path: &socket_path,
    provider: provider.as_deref(),
    model: model.as_deref(),
    agent_env: agent_env.as_deref(),
    max_frame_bytes: cfg.session.max_frame_bytes,
    timeout: Duration::from_secs(cfg.session.bootstrap_timeout_secs),
    retry_interval: Duration::from_millis(cfg.session.bootstrap_retry_interval_ms),
}).await?;
if !bootstrap.accepted_launch_options {
    if let Some(provider) = provider.as_deref() {
        bail!("--provider '{provider}' cannot override a running session daemon; restart the daemon with that provider");
    }
    if let Some(path) = agent_env.as_deref() {
        bail!("--agent-env {} cannot override a running session daemon; restart the daemon with that environment", path.display());
    }
}
let controller = SessionControllerHandle::spawn(bootstrap.transport, /* capabilities and bounded channels */);
```

```rust
// src/session_daemon.rs socket ownership metadata
struct SocketPathGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    _lock_file: File,
    instance_file: Option<(PathBuf, u64, u64)>,
}

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        // Remove instance file and socket only when current device/inode match.
    }
}

fn write_instance_metadata(socket_path: &Path) -> io::Result<(PathBuf, u64, u64)> {
    let path = instance_metadata_path(socket_path); // <socket>.pid
    let temporary = unique sibling path;
    let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&temporary)?;
    let body = json!({
        "pid": process::id(), "version": env!("CARGO_PKG_VERSION"),
        "socket": socket_path, "started_unix_ms": now,
        "bootstrap_fingerprint": env::var(BOOTSTRAP_FINGERPRINT_ENV).ok(),
    });
    file.write_all(&serde_json::to_vec(&body)?)?;
    file.sync_all()?;
    rename(temporary, path)?;
    let metadata = symlink_metadata(path)?;
    Ok((path, metadata.dev(), metadata.ino()))
}
```

Config additions (validated positive and documented):
- session.bootstrap_timeout_secs = 15
- session.bootstrap_retry_interval_ms = 50

Tests:
- daemon_args preserves exact explicit selections.
- fingerprint stable and changes with daemon-owned selection.
- only NotFound/ConnectionRefused trigger spawn.
- socket test verifies mode-0600 PID metadata contents and lifecycle cleanup.
- PTY integration launches two concurrent interactive agents with identical explicit config/provider/model/agent-env; both attach to one daemon, both quit cleanly, daemon remains, then SIGTERM removes socket and metadata.
- PTY missing-agent-env test verifies bounded failure names child exit and direct diagnostic command.

Verification: cargo fmt/clippy clean; full cargo test passed; full pytest passed (295).

### Known Concerns
1. Bootstrap stderr is redirected to null to guarantee terminal isolation; failure reports child status and a direct manual command rather than preserving daemon stderr.
2. The fingerprint is operational identity, not authentication; its metadata directory remains owner-only and the socket still enforces peer UID.
3. A spawned daemon still running at bootstrap timeout is intentionally left detached rather than killed, because it may be healthy but slow and task consensus forbids unsafe daemon termination.

### Specific Questions for Reviewer
1. Can child/process lifecycle leak zombies or accidentally die when the TUI runtime exits?
2. Does the fingerprint correctly preserve CLI override semantics across both existing-daemon and concurrent-bootstrap paths?
3. Are metadata creation/replacement/removal safe against stale files and symlinks in the owner-controlled directory?
4. Is leaving a still-starting child alive on timeout acceptable, or should a child definitively owned by this launcher be terminated?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. CLI override semantics are enforced inconsistently: accepted_launch_options is a fingerprint over ALL of workspace/config/socket/provider/model/agent-env, but the interactive branch only bails when --provider or --agent-env were explicitly passed. Two failure modes follow. (1) If the user passes only --model or a different --config and the running daemon's fingerprint differs, accepted_launch_options is false but no bail! fires, so the TUI silently attaches to a daemon using a different model/config — exactly the 'silently accepting different provider/env settings' outcome the design claims to prevent. (2) Conversely, if the user passes no explicit selections at all and the existing daemon was started with explicit selections, the fingerprints differ (None vs Some values hashed), accepted_launch_options is false, yet attaching is presumably fine; the current bail conditions accidentally allow it only because no flags were set, which is correct behavior arrived at by coincidence rather than design. The mismatch check must cover every daemon-owned selection that participates in the fingerprint (model, config at minimum), or the fingerprint must be restricted to only the selections whose mismatch is actually rejected, with the policy stated explicitly.
B2. Fingerprint comparison is racy and fails open. instance_fingerprint(...) reads <socket>.pid metadata, but on the fast path (initial connect success) and on the spawn-race path, the metadata file may not exist yet: the daemon plausibly binds the socket before write_instance_metadata completes (order is not shown or asserted), and a launcher that loses the flock race can connect to the winner's socket before the winner has renamed the metadata file. instance_fingerprint(...).ok().flatten() maps both 'file missing' and 'read error' to None, which compares unequal to the local fingerprint, setting accepted_launch_options=false and (combined with B1) causing spurious bail!s for launchers that passed --provider/--agent-env with settings identical to the winner — breaking the advertised concurrent-launcher join guarantee, and doing so nondeterministically. Additionally, a pre-existing daemon started directly via `session-daemon` (no BOOTSTRAP_FINGERPRINT_ENV) writes bootstrap_fingerprint:null, so any explicit --provider attach to it always fails even when the selections match. Require: (a) daemon guarantees metadata is durably renamed before the socket accepts connections (or the fingerprint is exchanged in-band during the transport handshake, which also fixes the TOCTOU between connect and metadata read); (b) distinguish 'fingerprint unavailable' from 'fingerprint mismatch' with distinct user-facing outcomes; (c) define and test behavior against directly-launched daemons with no fingerprint.
B3. Spawned-child reaping is leaky on the error paths, answering the request's own zombie question in the negative. In the retry loop, once child_status becomes Some(...) the code stops calling try_wait, fine; but if the child is still running at timeout, the final block spawns a detached wait task on the tokio runtime — when connect_or_spawn returns Err, the caller bails, the runtime is torn down, the wait task is dropped, and (with kill_on_drop(false)) the child is intentionally left alive but will become a zombie of the TUI process if it exits before the TUI does and the wait task never ran to completion. Same issue on the success path: the tokio::spawn(child.wait()) reaper is dropped at runtime shutdown while the TUI keeps running its UI loop only if that loop is on the same runtime — if the daemon crashes mid-session, whether it is reaped depends on runtime lifetime, which is not established here. Since the child is in its own process group and detached by design, the correct fix is to fully disown it (e.g., double-fork/daemonize semantics or explicit reparenting guarantee) rather than rely on best-effort spawned wait tasks; alternatively document and test the reaping guarantee explicitly (e.g., assert no zombie after daemon crash while TUI persists). Also note child_status captures only the first observed exit; a child that exits after the last pre-timeout try_wait but before the bail produces the misleading 'timed out' message with no exit status — do a final try_wait before formatting the error.
B4. Metadata lifecycle has stale-file and symlink gaps despite the owner-only directory. (1) write_instance_metadata uses create_new on the temporary but rename() over the final path clobbers unconditionally: a still-running older daemon's metadata can be overwritten by a new daemon that won a flock on a different lock file or after a stale-socket takeover path not shown here — the guard's dev/ino check protects removal but nothing prevents two live daemons from fighting over <socket>.pid if the stale-socket takeover logic (should_bootstrap treats ConnectionRefused as spawn-worthy, so a crashed daemon's leftover socket triggers a new spawn) doesn't first prove exclusivity via the flock before rename. Specify and test the ordering: acquire flock -> verify/remove stale socket -> bind -> write metadata -> accept. (2) instance_metadata_path uses symlink_metadata after rename, good, but the read side (instance_fingerprint) must also open with O_NOFOLLOW/verify symlink_metadata is a regular file before reading, or a symlink planted between daemon restarts (still owner-created in some workflows, e.g., restored backups) causes reads of arbitrary files; the diff doesn't show the read-side hardening. (3) On ConnectionRefused with an existing socket file, the new spawn will fail to bind unless it unlinks the stale socket; the diff never shows stale-socket unlink protected by the flock — without it the retry loop spins to timeout on every crashed-daemon recovery, which contradicts the feature's core purpose. Show the daemon-side takeover sequence or add an explicit test: kill -9 the daemon, relaunch interactive TUI, assert successful bootstrap within timeout.

### Non-Blocking Suggestions
S1. Answering the stderr known-concern: instead of Stdio::null for the child's stderr, pipe it to a small bounded ring buffer (or a mode-0600 file beside the socket, e.g., <socket>.bootstrap.log truncated per attempt) and include the tail in the timeout/exit error. This preserves terminal isolation while making the most common failure (bad agent-env, port/socket perms, config parse error) diagnosable without asking the user to re-run a manual command.
S2. On the timeout question: leaving an unowned or possibly-shared daemon alive is right, but a child this launcher definitively spawned, that has never accepted a connection, and whose flock/metadata prove no other launcher joined it, could be safely SIGTERMed on timeout without violating the no-unsafe-termination consensus. If you keep the leave-alive policy, at least mention the live PID in the timeout message so the user can inspect or kill it.
S3. Include a protocol/handshake version alongside the fingerprint in the transport greeting so version-skewed daemons are detected at attach time with a clear 'restart the daemon (running vX, client vY)' error rather than surfacing as frame-decode failures; the metadata file already records version but per B2 the in-band path is the reliable one.
S4. bootstrap_fingerprint hashes path bytes verbatim; canonicalize workspace/config/agent-env (or hash dev/ino for existing paths) so equivalent invocations from different CWDs or via symlinked paths join the same daemon instead of hard-failing as mismatches.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1331-daemon-bootstrap/artifacts/round-1-rebuttal.diff" sha256="81632e80672646bc98ffdf07fd631846140b51b12f7b2e8817513e9182bc7a02" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — The fingerprint policy mixed configurable runtime model state with immutable launch selections and did not enforce explicit config mismatch. I replaced the boolean with LaunchIdentity {NotRequested, Matched, Mismatched, Unavailable}. The fingerprint now covers canonical workspace plus explicit config/provider/agent-env only. Model is deliberately excluded because TUI applies it through daemon-confirmed SetConfig. No explicit immutable selection yields NotRequested and may attach to any compatible daemon; explicit mismatch and unavailable identity produce distinct hard errors.
Re B2: ACCEPT — Metadata was written after bind, so connect could win before the atomic rename. bind_local_socket now holds the exact socket-derived flock, removes any stale owner socket, atomically writes/syncs metadata, and only then binds the socket; successful connect therefore implies complete metadata. Unavailable and mismatched identity are distinct. Direct daemons publish null identity: clients with no explicit immutable selections attach, while explicit config/provider/agent-env requires restart under the new bootstrap version. Read errors no longer collapse to None.
Re B3: ACCEPT — Best-effort Tokio wait tasks were not a durable reaping strategy. Bootstrap now uses the classic async-signal-safe double-fork in Command::pre_exec: setsid, fork, intermediate _exit; the launcher immediately waits/reaps the intermediate and the grandchild execs the daemon reparented outside the TUI. No child handle or runtime task remains. The concurrent PTY test reads /proc and proves daemon PPid is not either TUI process. Timeout reports metadata PID when available and leaves the independent daemon untouched.
Re B4: ACCEPT — The daemon-side ordering is now explicit and tested: acquire exact socket-derived flock, validate/remove stale owner socket, publish atomic metadata, bind, then accept. The same lock path prevents two live daemons from writing one metadata path. Read side now requires an owner-only regular file, uses O_NOFOLLOW, compares pre-open and opened dev/inode, and bounds metadata to 16 KiB. Added a Rust stale-socket takeover test and an external SIGKILL recovery test proving a new daemon replaces the stale socket/metadata and cleans both on shutdown.

### Revised Code / Diff
```diff
--- a/src/session_bootstrap.rs
+++ b/src/session_bootstrap.rs
@@
-pub struct BootstrapConnection { pub transport: UnixFrontendTransport, pub accepted_launch_options: bool }
+pub struct BootstrapConnection { pub transport: UnixFrontendTransport, pub launch_identity: LaunchIdentity }
+
+pub enum LaunchIdentity {
+    NotRequested,
+    Matched,
+    Mismatched,
+    Unavailable,
+}
@@
-command.as_std_mut().process_group(0);
-let mut child = command.spawn()?;
+unsafe {
+    command.as_std_mut().pre_exec(|| {
+        if libc::setsid() < 0 { return Err(io::Error::last_os_error()); }
+        let pid = libc::fork();
+        if pid < 0 { return Err(io::Error::last_os_error()); }
+        if pid > 0 { libc::_exit(0); }
+        Ok(())
+    });
+}
+let mut intermediate = command.spawn()?;
+let status = intermediate.wait().await?;
+if !status.success() { bail!("session daemon bootstrap process exited with {status}"); }
@@
-// tokio::spawn(child.wait()) on success/error
+// no persistent child handle: daemon grandchild is reparented outside TUI
@@
 fn bootstrap_fingerprint(options: &BootstrapOptions<'_>) -> String {
-    // workspace, config, socket, provider, model, agent-env
+    // workspace, explicit config, provider, agent-env
+    // model excluded: it is daemon-confirmed mutable runtime state
 }
+
+fn launch_identity(options: &BootstrapOptions<'_>) -> io::Result<LaunchIdentity> {
+    if options.config_path.is_none() && options.provider.is_none() && options.agent_env.is_none() {
+        return Ok(LaunchIdentity::NotRequested);
+    }
+    let Some(metadata) = instance_metadata(options.socket_path)? else {
+        return Ok(LaunchIdentity::Unavailable);
+    };
+    let Some(actual) = metadata["bootstrap_fingerprint"].as_str() else {
+        return Ok(LaunchIdentity::Unavailable);
+    };
+    Ok(if actual == bootstrap_fingerprint(options) {
+        LaunchIdentity::Matched
+    } else {
+        LaunchIdentity::Mismatched
+    })
+}
+
+fn instance_metadata(socket: &Path) -> io::Result<Option<Value>> {
+    let metadata = symlink_metadata(path)?;
+    if !metadata.file_type().is_file()
+        || metadata.uid() != geteuid()
+        || metadata.permissions().mode() & 0o077 != 0 {
+        return Err(io::Error::new(PermissionDenied, "metadata is not owner-only regular file"));
+    }
+    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
+    let opened = file.metadata()?;
+    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
+        return Err(io::Error::other("metadata changed while opening"));
+    }
+    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
+    if bytes.len() > 16 * 1024 { return Err(io::Error::new(InvalidData, "metadata too large")); }
+    Ok(Some(serde_json::from_slice(&bytes)?))
+}
```

```diff
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@
-if !bootstrap.accepted_launch_options { /* provider/agent-env-only checks */ }
+match bootstrap.launch_identity {
+    LaunchIdentity::NotRequested | LaunchIdentity::Matched => {}
+    LaunchIdentity::Mismatched => bail!(
+        "explicit config/provider/agent-env selection differs from the running session daemon; restart it"
+    ),
+    LaunchIdentity::Unavailable => bail!(
+        "running daemon publishes no comparable identity; restart with this version before explicit selection"
+    ),
+}
```

```diff
--- a/src/session_daemon.rs
+++ b/src/session_daemon.rs
@@
- let listener = UnixListener::bind(socket_path)?;
- // chmod and metadata
- guard.instance_file = Some(write_instance_metadata(socket_path)?);
+ // Exact socket-derived flock is already held and any stale owner socket removed.
+ let instance_file = write_instance_metadata(socket_path)?;
+ let listener = match UnixListener::bind(socket_path) {
+     Ok(listener) => listener,
+     Err(error) => { remove_matching_regular_file(&instance_file); return Err(error); }
+ };
+ // chmod/metadata failures remove both socket and matching instance file.
+ let guard = SocketPathGuard { instance_file: Some(instance_file), /* inode ownership */ };
```

Tests added/revised:
- LaunchIdentity match/mismatch/unavailable/not-requested policy.
- Model changes do not alter immutable fingerprint; provider changes do.
- Metadata symlink rejection with O_NOFOLLOW path.
- Stale owner socket replacement under flock.
- External SIGKILL stale-socket recovery.
- Concurrent PTY bootstrap asserts daemon PPid is neither TUI PID (double-fork reparenting).

Full paths and implementation are in src/session_bootstrap.rs, src/session_daemon.rs, src/agent_runtime.rs, and tests/test_cli_modes.py.

### New Points
R1. Publishing metadata before bind removes the connect/metadata race entirely: connect cannot succeed until after the atomic metadata rename and sync complete.
R2. Model is intentionally excluded from immutable launch identity because the existing task-1331 path validates and applies it through correlated SetConfig before any initial prompt; treating it as immutable would reject a supported operation.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. The Unavailable bail message ('running daemon publishes no comparable identity; restart with this version before explicit selection') should include the discovered daemon PID/version from the metadata file when the file exists but lacks a fingerprint (directly-launched daemon), versus the file being absent/unreadable. These are different operator situations — 'old daemon version' vs 'metadata corrupted or raced' — and the current single message conflates them; distinguishing them costs one extra field in the error.
S6. The double-fork pre_exec closure calls libc::fork() inside pre_exec, which runs post-fork in the child of a multithreaded tokio process; setsid/fork/_exit are async-signal-safe so this is correct, but add a code comment asserting only async-signal-safe calls are permitted there, and note that any future addition (e.g., logging, allocation via bail-style formatting) would be UB. A regression test alone won't catch a reintroduced allocation reliably.
S7. With the double-fork, the launcher can no longer observe the daemon grandchild's exit status, so the timeout diagnostic ('spawned daemon exited with {status}') from the original design is now limited to the intermediate's status. Combine with prior S1: have the grandchild write early-startup failures to <socket>.bootstrap.log (mode 0600, truncated per attempt) so the timeout error can include the actual daemon failure reason; otherwise the most common failure mode (daemon exits instantly on bad config) reports only a generic timeout.
S8. Prior suggestions S1 (stderr capture), S3 (in-band handshake version), and S4 (path canonicalization for the fingerprint) remain open and worthwhile. S4 in particular interacts with the new LaunchIdentity policy: two launchers passing the same --config via differing relative paths will now hard-fail as Mismatched rather than silently diverge, which is safe but avoidably unfriendly; canonicalizing before hashing (the rebuttal mentions 'canonical workspace' — extend that to config and agent-env) resolves it.

### Responses to Prior Points
Re R1: Accepted. Writing and syncing metadata before bind is the correct ordering: a successful connect now strictly implies the atomic metadata rename has completed, which closes the connect-before-metadata race identified in B2. The failure-path cleanup (removing the matching instance file if bind fails) also avoids leaving orphan metadata behind. The residual TOCTOU (daemon restarting between connect and metadata read) is bounded by the flock and dev/ino checks and is acceptable for operational identity rather than authentication.
Re R2: Accepted. Excluding model from the immutable launch identity is the right call given that the TUI applies model via daemon-confirmed correlated SetConfig: model mismatch is resolved by an explicit, acknowledged runtime operation rather than silent divergence, so it does not belong in the fingerprint. The added test asserting model changes do not alter the fingerprint while provider changes do locks in this policy. Please ensure the policy (model = mutable runtime state, config/provider/agent-env = immutable launch identity) is documented where the fingerprint is defined, as the revised diff comment already begins to do.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
