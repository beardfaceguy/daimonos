# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="mcp-system-integration-review" -->

<!-- event id="request" artifact path="mcp-system-integration-review-mcp-system-integration/artifacts/round-1-review-request.diff" sha256="65c2696ef4288061cb77e961e5c40694e57d53bb6aead46bc72f5ec7cd83430d" -->
## Review Request — Round 1
**Task:** mcp-system-integration-review — Zed-primary MCP integration for Daimonos and Cursor ACP
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Keep the approved Daimonos harness-first/Zed fallback behavior, add `daimonos mcp-config sync` to atomically translate Zed JSONC context_servers into Cursor's standalone mcpServers format, and support `-- COMMAND` so Zed can launch Cursor ACP through a sync-then-exec wrapper. The live system is configured with this wrapper.

### Relevant Code / Diff
diff --git i/daimonos.default.toml w/daimonos.default.toml
index 8e94464..7df670b 100644
--- i/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -348,8 +348,9 @@ session_list_page_size = 50
 # Outbound MCP servers for daimonos's own agent frontends — interactive TUI,
 # one-shot `agent`, `chat` REPL, and the session daemon (#1289). The ACP path
 # is unaffected (there the client forwards the server list per session).
-# Reads a Claude-style `mcpServers` JSON file — the same format Claude
-# Desktop / Claude Code use — so one file can feed every harness:
+# Reads either Zed's JSONC `context_servers` settings or a Claude/Cursor-style
+# `mcpServers` JSON file. The default points directly at Zed, making it the
+# primary system configuration without copies or symlinks:
 #   { "mcpServers": {
 #       "linear":   { "command": "npx", "args": ["-y", "linear-mcp"] },
 #       "repowise": { "url": "http://127.0.0.1:8931/mcp" } } }
@@ -357,7 +358,7 @@ session_list_page_size = 50
 # Bridge tuning (timeouts, caps, allow_stdio/allow_http, self-connection
 # refusal) is shared with [acp.mcp] below.
 enabled = true
-servers_file = "~/.config/daimonos/mcp_servers.json"
+servers_file = "~/.config/zed/settings.json"
 
 [acp.mcp]
 # MCP-server bridge (ADR-003, #990). Zed forwards every configured context
@@ -399,12 +400,10 @@ max_tools_per_server = 128
 # Fallback for unpatched Zed: when Zed forwards an EMPTY MCP server list at
 # session start (a cold-start race where its context-server store isn't
 # populated yet, and it never re-forwards to a live session), read Zed's own
-# `context_servers` settings directly and bridge those. Off by default because
-# it reads an external app's config and spawns that config's stdio servers, so
-# it must not fire for non-Zed ACP clients. Enable it only when running
-# daimonos as Zed's ACP agent on an unpatched Zed. Only triggers on an empty
-# forwarded list — servers Zed did forward are never overridden.
-zed_config_fallback = false
+# `context_servers` settings directly and bridge those. This is considered
+# only when initialize identifies the ACP client as Zed; an empty list from any
+# other harness remains authoritative. Harness-forwarded servers always win.
+zed_config_fallback = true
 # Path to Zed's settings.json for the fallback above. Empty/unset derives it
 # from $XDG_CONFIG_HOME/$HOME (~/.config/zed/settings.json).
 # zed_settings_path = "/home/you/.config/zed/settings.json"
diff --git i/docs/zed-acp-setup.md w/docs/zed-acp-setup.md
index 2cbfa8f..067be82 100644
--- i/docs/zed-acp-setup.md
+++ w/docs/zed-acp-setup.md
@@ -13,6 +13,56 @@ used together.
   `daimonos acp` loads it the same way `daimonos agent`/`daimonos chat` do
 - Zed editor installed, with agent-panel support for custom `agent_servers`
 
+## Shared MCP configuration
+
+Daimonos treats Zed as the primary MCP configuration by default. Non-ACP
+agents read `~/.config/zed/settings.json` directly, including its JSONC
+`context_servers`. ACP sessions resolve servers in this order:
+
+1. Servers forwarded by the current ACP harness.
+2. If—and only if—ACP initialize identifies the harness as Zed and it forwards
+   an empty list, Zed `context_servers`.
+3. For that same Zed recovery case, `[agent.mcp].servers_file` as the final
+   Daimonos fallback.
+
+An empty list from Cursor or any unknown/non-Zed ACP harness remains
+authoritative and produces no MCP servers.
+
+To select a Claude/Cursor-style shared file instead:
+
+```toml
+[agent.mcp]
+servers_file = "~/.config/mcp/servers.json"
+```
+
+The selected file may contain either `mcpServers` or `context_servers`; if it
+contains both, `context_servers` takes precedence. A harness that cannot select a path may symlink its standalone MCP file to the
+shared file. Do not symlink all of Zed's `settings.json` over a harness file
+that only accepts a standalone `mcpServers` document. While Zed's file is being
+edited, invalid JSONC causes Daimonos to skip configured MCP servers and emit a
+warning rather than failing the agent session.
+
+### Cursor ACP and other file-only harnesses
+
+Cursor ACP currently ignores ACP-forwarded MCP servers and reads
+`~/.cursor/mcp.json`, whose required `mcpServers` shape is incompatible with
+Zed's full `settings.json`. Use the built-in translator and launch wrapper:
+
+```sh
+daimonos mcp-config sync --target ~/.cursor/mcp.json -- cursor-agent acp
+```
+
+The command reads Zed at process start, atomically regenerates Cursor's file,
+and then replaces itself with Cursor ACP. Configure Zed's `agent_servers.cursor`
+as `type = "custom"` with that command/argument sequence if the registry entry
+cannot be wrapped. This guarantees each new Cursor ACP sees Zed's current MCP
+list without manually copying settings. `--dry-run` prints the generated JSON;
+omit `-- COMMAND` to perform a one-time sync.
+
+Because the formats differ, a direct symlink is unsafe. The generated file is
+mode `0600`; if an old Cursor file exists it is atomically replaced. Disabled
+Zed servers and Zed-only metadata such as `timeout` are omitted.
+
 ## Setup
 
 Add daimonos under the `agent_servers` key in Zed's `settings.json`. The
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index 6737377..aabb3db 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -444,6 +444,9 @@ struct AcpState {
     supports_images: bool,
     /// Zed's `_meta.terminal_output` extension, negotiated at initialize.
     supports_terminal_output: AtomicBool,
+    /// Whether initialize identified the current ACP client as Zed. File-based
+    /// recovery is never applied to other clients' intentionally empty lists.
+    client_is_zed: AtomicBool,
     /// Maximum sessions returned by one session/list response.
     session_list_page_size: usize,
     /// Context/window compaction configuration cloned into each session.
@@ -1566,42 +1569,34 @@ fn to_server_specs(servers: Vec<McpServer>) -> Vec<ServerSpec> {
         .collect()
 }
 
-/// Resolve the MCP servers to bridge for a session. Normally these are the
-/// list Zed forwards in `session/new`/`session/load`. But unpatched Zed has a
-/// cold-start race where it forwards an EMPTY list (its context-server store
-/// isn't populated when it issues a restored session) and never re-forwards to
-/// a live session — leaving the session with no MCP tools. So when the
-/// forwarded list is empty (and the fallback is enabled), read Zed's own
-/// `context_servers` settings directly and bridge those instead (see
-/// [`crate::zed_config`]). Only the empty case triggers the fallback; servers
-/// Zed did forward are never overridden.
-fn resolve_mcp_specs(forwarded: Vec<McpServer>, cfg: &Config) -> Vec<ServerSpec> {
+/// Resolve MCP servers in precedence order: the ACP harness first, then its
+/// Zed settings fallback, then Daimonos's configured shared servers file.
+async fn resolve_mcp_specs(
+    forwarded: Vec<McpServer>,
+    cfg: &Config,
+    client_is_zed: bool,
+) -> Vec<ServerSpec> {
     let specs = to_server_specs(forwarded);
-    if !specs.is_empty() || !cfg.acp.mcp.enabled || !cfg.acp.mcp.zed_config_fallback {
+    // An empty list from an unknown/non-Zed client may be intentional. Zed is
+    // the sole exception because its known cold-start race loses configured
+    // servers before session/new or session/load.
+    if !specs.is_empty() || !cfg.acp.mcp.enabled || !client_is_zed {
         return specs;
     }
-    match crate::zed_config::context_server_specs(cfg.acp.mcp.zed_settings_path.as_deref()) {
-        Ok(fallback) if !fallback.is_empty() => {
-            tracing::warn!(
-                target: "daimonos::acp",
-                event = "mcp_forward_empty_fallback",
-                recovered = fallback.len(),
-                "Zed forwarded no MCP servers; recovered them from Zed settings \
-                 (unpatched-Zed cold-start race)"
-            );
-            fallback
-        }
-        Ok(_) => specs,
-        Err(e) => {
-            tracing::warn!(
-                target: "daimonos::acp",
-                event = "mcp_fallback_failed",
-                error = %e,
-                "Zed forwarded no MCP servers and reading Zed settings failed"
-            );
-            specs
+    if cfg.acp.mcp.zed_config_fallback {
+        match crate::zed_config::context_server_specs(cfg.acp.mcp.zed_settings_path.as_deref()) {
+            Ok(fallback) if !fallback.is_empty() => {
+                tracing::warn!(target: "daimonos::acp", event = "mcp_forward_empty_fallback",
+                    recovered = fallback.len(),
+                    "ACP harness forwarded no MCP servers; recovered Zed settings");
+                return fallback;
+            }
+            Ok(_) => {}
+            Err(error) => tracing::warn!(target: "daimonos::acp", event = "mcp_fallback_failed",
+                %error, "ACP harness forwarded no MCP servers and Zed settings failed"),
         }
     }
+    crate::agent_mcp::load_configured_specs(cfg).await
 }
 
 fn should_refresh_mcp_bridge(
@@ -2584,6 +2579,7 @@ fn build_agent_with_state(
         }),
         supports_images,
         supports_terminal_output: AtomicBool::new(false),
+        client_is_zed: AtomicBool::new(false),
         session_list_page_size: cfg.acp.session_list_page_size,
         compaction,
         analytics,
@@ -2610,6 +2606,11 @@ fn build_agent_with_state(
                         state
                             .supports_terminal_output
                             .store(client_supports_terminal_output(&req), Ordering::Release);
+                        let is_zed = req
+                            .client_info
+                            .as_ref()
+                            .is_some_and(|info| info.name.eq_ignore_ascii_case("zed"));
+                        state.client_is_zed.store(is_zed, Ordering::Release);
                         // load_session(true): Zed calls session/load to reopen
                         // a thread on window refocus.
                         let mut capabilities = AgentCapabilities::new()
@@ -2876,11 +2877,15 @@ fn build_agent_with_state(
                     let token_log = token_log.clone();
                     async move {
                         let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
-                        // Zed forwards the user's configured MCP servers here
-                        // (ADR-003); bridge them into this session. Falls back
-                        // to Zed's settings when the forwarded list is empty
-                        // (unpatched-Zed cold-start race).
-                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
+                        // The current harness's non-empty list is authoritative.
+                        // Only an identified Zed client gets file recovery for
+                        // its known empty-forward cold-start race.
+                        let mcp_specs = resolve_mcp_specs(
+                            req.mcp_servers,
+                            &cfg,
+                            state.client_is_zed.load(Ordering::Acquire),
+                        )
+                        .await;
                         let mcp_server_count = mcp_specs.len();
                         // Use the client-provided project root, not the CLI's
                         // own cwd — Zed passes the actual project it wants this
@@ -3000,9 +3005,14 @@ fn build_agent_with_state(
                     let token_log = token_log.clone();
                     async move {
                         let session_id = req.session_id.clone();
-                        // Same empty-forward fallback as session/new: a
-                        // reloaded thread must also recover its MCP servers.
-                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
+                        // Apply the same harness-first, Zed-gated recovery as
+                        // session/new so restored Zed threads recover MCP tools.
+                        let mcp_specs = resolve_mcp_specs(
+                            req.mcp_servers,
+                            &cfg,
+                            state.client_is_zed.load(Ordering::Acquire),
+                        )
+                        .await;
                         let session_workspace = if req.cwd.as_os_str().is_empty() {
                             workspace_fallback
                         } else {
@@ -4026,8 +4036,8 @@ mod tests {
         );
     }
 
-    #[test]
-    fn resolve_mcp_specs_fallback_gating() {
+    #[tokio::test]
+    async fn resolve_mcp_specs_fallback_gating() {
         let dir = tempfile::tempdir().unwrap();
         let path = dir.path().join("settings.json");
         std::fs::write(
@@ -4038,23 +4048,44 @@ mod tests {
         let mut cfg = Config::default();
         cfg.acp.mcp.zed_settings_path = Some(path.to_string_lossy().into_owned());
 
-        // Enabled + empty forward -> read Zed config fallback.
+        // Identified Zed + empty forward -> read Zed config fallback.
         cfg.acp.mcp.zed_config_fallback = true;
-        assert_eq!(resolve_mcp_specs(vec![], &cfg).len(), 1);
+        assert_eq!(resolve_mcp_specs(vec![], &cfg, true).await.len(), 1);
 
-        // Disabled (the default) -> no fallback, even with an empty forward.
+        // An empty list from any other harness is authoritative.
+        assert!(resolve_mcp_specs(vec![], &cfg, false).await.is_empty());
+
+        // With both file fallbacks disabled, an empty forward stays empty.
         cfg.acp.mcp.zed_config_fallback = false;
-        assert!(resolve_mcp_specs(vec![], &cfg).is_empty());
+        cfg.agent.mcp.enabled = false;
+        assert!(resolve_mcp_specs(vec![], &cfg, true).await.is_empty());
 
-        // A non-empty forward is never overridden by the fallback.
+        // Tier 3: identified Zed, no Zed settings, configured shared file.
+        cfg.acp.mcp.zed_settings_path =
+            Some(dir.path().join("missing.json").to_string_lossy().into());
+        let shared = dir.path().join("shared.json");
+        std::fs::write(
+            &shared,
+            r#"{ "mcpServers": { "shared": { "command": "x" } } }"#,
+        )
+        .unwrap();
+        cfg.agent.mcp.servers_file = shared.to_string_lossy().into();
+        cfg.agent.mcp.enabled = true;
+        assert!(matches!(
+            &resolve_mcp_specs(vec![], &cfg, true).await[0],
+            ServerSpec::Stdio { name, .. } if name == "shared"
+        ));
+
+        // A non-empty forward is never overridden by either fallback.
         cfg.acp.mcp.zed_config_fallback = true;
+        cfg.agent.mcp.enabled = true;
         let forwarded = vec![McpServer::Http(
             agent_client_protocol::schema::v1::McpServerHttp::new(
                 "fwd".to_string(),
                 "http://127.0.0.1:10/".to_string(),
             ),
         )];
-        let specs = resolve_mcp_specs(forwarded, &cfg);
+        let specs = resolve_mcp_specs(forwarded, &cfg, true).await;
         assert_eq!(specs.len(), 1);
         assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "fwd"));
     }
diff --git i/src/agent_mcp.rs w/src/agent_mcp.rs
index 850e293..1a6073c 100644
--- i/src/agent_mcp.rs
+++ w/src/agent_mcp.rs
@@ -24,19 +24,27 @@ use crate::config::Config;
 use crate::mcp_bridge::{McpBridge, McpClientPool, ServerSpec};
 use crate::providers::ToolSchema;
 
-/// Parse a Claude-style config: `{"mcpServers": {name: {command, args?, env?}
-/// | {url, headers?}}}`. Entries are sorted by name so bridge order (and thus
-/// the `max_servers` cap) is deterministic — JSON object order is not
-/// preserved. Unknown per-server keys are ignored for forward compatibility;
-/// a `type` field, when present, must agree with the transport implied by
-/// `command`/`url`.
+/// Parse either a Claude/Cursor-style `mcpServers` file or Zed's
+/// `settings.json` (`context_servers`). This lets `[agent.mcp].servers_file`
+/// point directly at the system's primary Zed configuration instead of
+/// maintaining a translated copy. If both top-level keys exist,
+/// `context_servers` takes precedence. Entries are sorted by name so bridge
+/// order (and thus the `max_servers` cap) is deterministic.
 pub fn parse_servers_json(content: &str) -> Result<Vec<ServerSpec>, String> {
+    // Zed settings are JSONC. Reuse its reader's string-aware cleanup before
+    // parsing; ordinary JSON passes through unchanged.
+    let cleaned = crate::zed_config::clean_jsonc(content);
     let root: Value =
-        serde_json::from_str(content).map_err(|error| format!("invalid JSON: {error}"))?;
+        serde_json::from_str(&cleaned).map_err(|error| format!("invalid JSON/JSONC: {error}"))?;
+    if root.get("context_servers").is_some() {
+        return Ok(crate::zed_config::specs_from_settings(&root));
+    }
     let servers = root
         .get("mcpServers")
         .and_then(Value::as_object)
-        .ok_or_else(|| "missing top-level \"mcpServers\" object".to_string())?;
+        .ok_or_else(|| {
+            "missing top-level \"mcpServers\" or \"context_servers\" object".to_string()
+        })?;
     let mut names: Vec<&String> = servers.keys().collect();
     names.sort();
     names
@@ -156,6 +164,32 @@ impl AgentMcp {
     }
 }
 
+/// Load the system/user MCP configuration selected by `[agent.mcp]`.
+/// Both Claude/Cursor and Zed settings formats are accepted.
+pub async fn load_configured_specs(cfg: &Config) -> Vec<ServerSpec> {
+    if !cfg.agent.mcp.enabled {
+        return Vec::new();
+    }
+    let path = crate::paths::expand_tilde(&cfg.agent.mcp.servers_file);
+    let content = match tokio::fs::read_to_string(&path).await {
+        Ok(content) => content,
+        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
+        Err(error) => {
+            tracing::warn!(target: "daimonos::agent_mcp", path = %path.display(), %error,
+                "mcp servers file unreadable; continuing without configured MCP servers");
+            return Vec::new();
+        }
+    };
+    match parse_servers_json(&content) {
+        Ok(specs) => specs,
+        Err(error) => {
+            tracing::warn!(target: "daimonos::agent_mcp", path = %path.display(), %error,
+                "mcp servers file invalid; continuing without configured MCP servers");
+            Vec::new()
+        }
+    }
+}
+
 /// Read `[agent.mcp] servers_file` and connect. `None` when disabled, the
 /// file is absent or empty, or it fails to read/parse (logged) — the agent
 /// then runs with native tools only.
@@ -164,35 +198,7 @@ pub async fn connect(
     native_tool_names: &HashSet<String>,
     analytics: Option<Arc<AnalyticsStore>>,
 ) -> Option<AgentMcp> {
-    if !cfg.agent.mcp.enabled {
-        return None;
-    }
-    let path = crate::paths::expand_tilde(&cfg.agent.mcp.servers_file);
-    let content = match tokio::fs::read_to_string(&path).await {
-        Ok(content) => content,
-        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
-        Err(error) => {
-            tracing::warn!(
-                target: "daimonos::agent_mcp",
-                path = %path.display(),
-                %error,
-                "mcp servers file unreadable; continuing with native tools only"
-            );
-            return None;
-        }
-    };
-    let specs = match parse_servers_json(&content) {
-        Ok(specs) => specs,
-        Err(error) => {
-            tracing::warn!(
-                target: "daimonos::agent_mcp",
-                path = %path.display(),
-                %error,
-                "mcp servers file invalid; continuing with native tools only"
-            );
-            return None;
-        }
-    };
+    let specs = load_configured_specs(cfg).await;
     if specs.is_empty() {
         return None;
     }
@@ -257,6 +263,20 @@ mod tests {
         }
     }
 
+    #[test]
+    fn parses_zed_jsonc_context_servers() {
+        let jsonc = r#"{
+          // one primary config for Zed and Daimonos
+          "context_servers": {
+            "zed-http": { "url": "https://example.test/mcp", },
+            "off": { "enabled": false, "command": "nope" },
+          },
+        }"#;
+        let specs = parse_servers_json(jsonc).unwrap();
+        assert_eq!(specs.len(), 1);
+        assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "zed-http"));
+    }
+
     #[test]
     fn type_field_is_validated_but_optional() {
         let ok = r#"{"mcpServers": {"a": {"type": "stdio", "command": "x"},
@@ -276,7 +296,7 @@ mod tests {
     fn rejects_malformed_documents_legibly() {
         assert!(parse_servers_json("not json")
             .unwrap_err()
-            .contains("invalid JSON"));
+            .contains("invalid JSON/JSONC"));
         assert!(parse_servers_json("{}").unwrap_err().contains("mcpServers"));
         assert!(parse_servers_json(r#"{"mcpServers": {"a": {}}}"#)
             .unwrap_err()
diff --git i/src/cli.rs w/src/cli.rs
index 5c44499..9b158cd 100644
--- i/src/cli.rs
+++ w/src/cli.rs
@@ -115,6 +115,31 @@ pub struct McpArgs {
     pub socket: Option<PathBuf>,
 }
 
+#[derive(Debug, Args)]
+pub struct McpConfigArgs {
+    #[command(subcommand)]
+    pub command: McpConfigCommand,
+}
+
+#[derive(Debug, Subcommand)]
+pub enum McpConfigCommand {
+    /// Translate Zed context_servers into a harness mcpServers file.
+    Sync {
+        /// Zed settings.json source (defaults to the platform Zed path).
+        #[arg(long, value_name = "FILE")]
+        source: Option<PathBuf>,
+        /// Harness MCP file to replace atomically.
+        #[arg(long, value_name = "FILE", default_value = "~/.cursor/mcp.json")]
+        target: PathBuf,
+        /// Print the generated document without writing it.
+        #[arg(long)]
+        dry_run: bool,
+        /// After a successful sync, replace this process with COMMAND.
+        #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
+        exec: Vec<String>,
+    },
+}
+
 #[derive(Debug, Args)]
 pub struct SessionArgs {
     #[command(subcommand)]
@@ -156,6 +181,8 @@ pub enum Command {
     Session(SessionArgs),
     /// Run the MCP tool server over stdio or a Unix socket.
     Mcp(McpArgs),
+    /// Synchronize Zed's MCP servers into harness-specific configuration.
+    McpConfig(McpConfigArgs),
     /// Run the compact opcode protocol daemon over a Unix socket.
     Daemon,
 }
@@ -169,6 +196,7 @@ pub enum RuntimeMode {
     Session,
     McpStdio,
     McpSocket(PathBuf),
+    McpConfig,
     Daemon,
     Stats,
 }
@@ -183,6 +211,7 @@ impl RuntimeMode {
             Self::Session => "session",
             Self::McpStdio => "mcp_stdio",
             Self::McpSocket(_) => "mcp_socket",
+            Self::McpConfig => "mcp_config",
             Self::Daemon => "socket",
             Self::Stats => "stats",
         }
@@ -270,6 +299,7 @@ impl Cli {
             Some(Command::Acp(_)) => RuntimeMode::Acp,
             Some(Command::SessionDaemon(_)) => RuntimeMode::SessionDaemon,
             Some(Command::Session(_)) => RuntimeMode::Session,
+            Some(Command::McpConfig(_)) => RuntimeMode::McpConfig,
             Some(Command::Mcp(args)) => args
                 .socket
                 .clone()
diff --git i/src/config.rs w/src/config.rs
index cdd9f90..ed0836f 100644
--- i/src/config.rs
+++ w/src/config.rs
@@ -68,7 +68,7 @@ impl Default for AgentMcpConfig {
     fn default() -> Self {
         Self {
             enabled: true,
-            servers_file: "~/.config/daimonos/mcp_servers.json".to_string(),
+            servers_file: "~/.config/zed/settings.json".to_string(),
         }
     }
 }
@@ -1043,10 +1043,10 @@ pub struct AcpMcpConfig {
     /// settings directly and bridge those. Only triggers on an empty forwarded
     /// list — never overrides servers Zed did forward.
     ///
-    /// **Opt-in (default false):** it reads an external app's config file and
-    /// spawns that config's stdio servers, so it must not fire for non-Zed ACP
-    /// clients or in tests. Enable it only when running daimonos as Zed's ACP
-    /// agent on an unpatched Zed.
+    /// This is gated on ACP initialize identifying the client as Zed, so an
+    /// empty list from another harness remains authoritative. When enabled for
+    /// Zed, `[agent.mcp].servers_file` is the final fallback if this file has no
+    /// usable servers.
     pub zed_config_fallback: bool,
     /// Path to Zed's `settings.json` for `zed_config_fallback`. `None` derives
     /// it from `$XDG_CONFIG_HOME`/`$HOME` (`~/.config/zed/settings.json`).
@@ -1067,7 +1067,7 @@ impl Default for AcpMcpConfig {
             max_servers: 32,
             max_concurrent_connects: 8,
             max_tools_per_server: 128,
-            zed_config_fallback: false,
+            zed_config_fallback: true,
             zed_settings_path: None,
         }
     }
diff --git i/src/main.rs w/src/main.rs
index e7f0a77..8e4fb09 100644
--- i/src/main.rs
+++ w/src/main.rs
@@ -23,6 +23,7 @@ mod loop_detector;
 mod managed_process;
 mod mcp;
 mod mcp_bridge;
+mod mcp_config_cmd;
 mod observability;
 mod ops;
 mod paths;
@@ -243,7 +244,8 @@ async fn main() -> anyhow::Result<()> {
         Some(Command::Agent(args)) => !args.dry_run,
         Some(Command::Chat(args)) => !args.list,
         Some(Command::Acp(_) | Command::SessionDaemon(_)) => true,
-        Some(Command::Session(_) | Command::Mcp(_) | Command::Daemon) | None => false,
+        Some(Command::Session(_) | Command::Mcp(_) | Command::McpConfig(_) | Command::Daemon)
+        | None => false,
     };
     if uses_agent_prompt {
         cfg.prompts.additional_agent_instructions =
@@ -336,6 +338,7 @@ async fn main() -> anyhow::Result<()> {
             agent_runtime::run_session_daemon(args, &workspace, Arc::clone(&cfg), token_log).await
         }
         Some(Command::Session(args)) => session_interchange::run(args, &cfg),
+        Some(Command::McpConfig(args)) => mcp_config_cmd::run(args),
         Some(Command::Mcp(_) | Command::Daemon) | None => {
             run_tool_service(
                 runtime_mode,
@@ -481,6 +484,7 @@ async fn run_tool_service(
         | RuntimeMode::Acp
         | RuntimeMode::Session
         | RuntimeMode::SessionDaemon
+        | RuntimeMode::McpConfig
         | RuntimeMode::Stats => {
             unreachable!("early-return runtime reached service dispatch")
         }
diff --git i/src/mcp_config_cmd.rs w/src/mcp_config_cmd.rs
new file mode 100644
index 0000000..ca66038
--- /dev/null
+++ w/src/mcp_config_cmd.rs
@@ -0,0 +1,156 @@
+//! Translate Zed's JSONC `context_servers` into standalone `mcpServers` JSON.
+//! Some proprietary harnesses (notably Cursor ACP) ignore ACP-forwarded MCP
+//! servers and only read their own file, so a symlink cannot bridge the
+//! incompatible top-level formats. This command provides an atomic adapter.
+
+use std::path::{Path, PathBuf};
+
+use anyhow::Context;
+#[cfg(unix)]
+use std::os::unix::fs::PermissionsExt;
+
+use crate::cli::{McpConfigArgs, McpConfigCommand};
+use crate::mcp_bridge::ServerSpec;
+
+pub fn run(args: McpConfigArgs) -> anyhow::Result<()> {
+    match args.command {
+        McpConfigCommand::Sync {
+            source,
+            target,
+            dry_run,
+            exec,
+        } => sync(source, target, dry_run, exec),
+    }
+}
+
+fn sync(
+    source: Option<PathBuf>,
+    target: PathBuf,
+    dry_run: bool,
+    exec: Vec<String>,
+) -> anyhow::Result<()> {
+    let source = source
+        .map(|p| crate::paths::expand_tilde(p.to_string_lossy().as_ref()))
+        .unwrap_or_else(|| crate::zed_config::default_settings_path());
+    let target = crate::paths::expand_tilde(target.to_string_lossy().as_ref());
+    let specs = crate::zed_config::context_server_specs(Some(source.to_string_lossy().as_ref()))?;
+    let document = render_mcp_servers(&specs)?;
+    if dry_run {
+        print!("{document}");
+    } else {
+        write_atomic(&target, document.as_bytes())?;
+        eprintln!(
+            "synced {} enabled MCP server(s) from {} to {}",
+            specs.len(),
+            source.display(),
+            target.display()
+        );
+    }
+    if exec.is_empty() {
+        return Ok(());
+    }
+    if dry_run {
+        anyhow::bail!("--dry-run cannot be combined with a command after --");
+    }
+    use std::os::unix::process::CommandExt;
+    let error = std::process::Command::new(&exec[0]).args(&exec[1..]).exec();
+    Err(anyhow::anyhow!("exec {}: {error}", exec[0]))
+}
+
+fn render_mcp_servers(specs: &[ServerSpec]) -> anyhow::Result<String> {
+    let mut servers = serde_json::Map::new();
+    for spec in specs {
+        let (name, entry) = match spec {
+            ServerSpec::Stdio {
+                name,
+                command,
+                args,
+                env,
+            } => {
+                let mut entry = serde_json::Map::new();
+                entry.insert("command".into(), command.clone().into());
+                if !args.is_empty() {
+                    entry.insert("args".into(), serde_json::to_value(args)?);
+                }
+                if !env.is_empty() {
+                    entry.insert("env".into(), serde_json::to_value(env)?);
+                }
+                (name, entry)
+            }
+            ServerSpec::Http { name, url, headers } => {
+                let mut entry = serde_json::Map::new();
+                entry.insert("url".into(), url.clone().into());
+                if !headers.is_empty() {
+                    entry.insert("headers".into(), serde_json::to_value(headers)?);
+                }
+                (name, entry)
+            }
+        };
+        servers.insert(name.clone(), entry.into());
+    }
+    let mut root = serde_json::Map::new();
+    root.insert("mcpServers".into(), servers.into());
+    Ok(serde_json::to_string_pretty(&root)? + "\n")
+}
+
+fn write_atomic(path: &Path, content: &[u8]) -> anyhow::Result<()> {
+    let parent = path
+        .parent()
+        .ok_or_else(|| anyhow::anyhow!("target has no parent: {}", path.display()))?;
+    std::fs::create_dir_all(parent)?;
+    let tmp = parent.join(format!(
+        ".{}.daimonos-sync-{}",
+        path.file_name()
+            .and_then(|n| n.to_str())
+            .unwrap_or("mcp.json"),
+        std::process::id()
+    ));
+    std::fs::write(&tmp, content)?;
+    std::fs::rename(&tmp, path).map_err(|error| {
+        let _ = std::fs::remove_file(&tmp);
+        anyhow::anyhow!("replace {} atomically: {error}", path.display())
+    })?;
+    #[cfg(unix)]
+    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
+        .with_context(|| format!("secure {}", path.display()))?;
+    Ok(())
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use std::collections::HashMap;
+
+    #[test]
+    fn renders_cursor_compatible_document() {
+        let specs = vec![
+            ServerSpec::Stdio {
+                name: "local".into(),
+                command: "server".into(),
+                args: vec!["--stdio".into()],
+                env: HashMap::from([("TOKEN_FILE".into(), "/secret/path".into())]),
+            },
+            ServerSpec::Http {
+                name: "remote".into(),
+                url: "https://example.test/mcp".into(),
+                headers: HashMap::new(),
+            },
+        ];
+        let value: serde_json::Value =
+            serde_json::from_str(&render_mcp_servers(&specs).unwrap()).unwrap();
+        assert_eq!(value["mcpServers"]["local"]["command"], "server");
+        assert_eq!(
+            value["mcpServers"]["remote"]["url"],
+            "https://example.test/mcp"
+        );
+    }
+
+    #[test]
+    fn atomic_write_replaces_existing_file() {
+        let dir = tempfile::tempdir().unwrap();
+        let path = dir.path().join("mcp.json");
+        std::fs::write(&path, "old").unwrap();
+        write_atomic(&path, b"new\n").unwrap();
+        assert_eq!(std::fs::read_to_string(path).unwrap(), "new\n");
+    }
+}
diff --git i/src/zed_config.rs w/src/zed_config.rs
index 438210d..1db34a1 100644
--- i/src/zed_config.rs
+++ w/src/zed_config.rs
@@ -43,12 +43,16 @@ pub fn context_server_specs(override_path: Option<&str>) -> anyhow::Result<Vec<S
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
         Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
     };
-    let json = strip_trailing_commas(&strip_jsonc(&raw));
+    let json = clean_jsonc(&raw);
     let value: serde_json::Value = serde_json::from_str(&json)
         .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
     Ok(specs_from_settings(&value))
 }
 
+pub(crate) fn default_settings_path() -> PathBuf {
+    settings_path(None).unwrap_or_else(|| PathBuf::from("~/.config/zed/settings.json"))
+}
+
 fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
     if let Some(path) = override_path {
         return Some(PathBuf::from(path));
@@ -61,7 +65,7 @@ fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
 
 /// Map the `context_servers` object of a parsed Zed settings document to
 /// [`ServerSpec`]s. Unknown/incomplete entries are skipped.
-fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
+pub(crate) fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
     let Some(servers) = value.get("context_servers").and_then(|v| v.as_object()) else {
         return Vec::new();
     };
@@ -119,6 +123,12 @@ fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
         .unwrap_or_default()
 }
 
+/// Normalize Zed JSONC into strict JSON for both direct fallback and shared
+/// `[agent.mcp].servers_file` consumption.
+pub(crate) fn clean_jsonc(input: &str) -> String {
+    strip_trailing_commas(&strip_jsonc(input))
+}
+
 /// Remove `//` line comments and `/* */` block comments from a JSONC document,
 /// preserving anything inside string literals (so `"http://…"` and comment-like
 /// content in strings survive).

### Known Concerns
1. Zed and Cursor formats are structurally incompatible, so direct symlinking is not safe; synchronization occurs on every Cursor ACP launch.
2. The scoped diff excludes unrelated concurrent evidence work.

### Specific Questions for Reviewer
1. Does sync-then-exec meet the requested Zed-primary behavior reliably?
2. Are atomic replacement, permissions, disabled entries, environment/headers, and exec semantics safe?
3. Is the CLI and documentation adequate?

<!-- event id="1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Cross-platform build break: `sync()` in src/mcp_config_cmd.rs uses `std::os::unix::process::CommandExt::exec` unconditionally (no `#[cfg(unix)]` gate), unlike `write_atomic` which gates its unix-only permissions call. On non-Unix targets this will not compile. Either gate the exec path with a clear error on unsupported platforms or document/enforce unix-only compilation for this module.
B2. Temp-file security race in `write_atomic`: the file is created via `std::fs::write` with default permissions (typically 0644) and only chmod'd to 0600 AFTER the rename into place. Since the target may hold MCP env secrets/headers (the docs promise mode 0600), the content is world-readable during the window between write and set_permissions, and the predictable temp name (`.NAME.daimonos-sync-PID`) is also created non-atomically. Create the temp file with 0600 from the start (e.g., OpenOptions + mode(0o600)) and set permissions before the rename.
B3. Silent behavior/security default change: `zed_config_fallback` flips from opt-in (false) to true, and `[agent.mcp].servers_file` default changes to `~/.config/zed/settings.json`. For existing users this means Daimonos will now read an external app's config and spawn its stdio servers by default after upgrade, without any migration note. The Zed-client gating mitigates the ACP path, but the non-ACP agent path now spawns Zed-configured stdio servers unconditionally. This needs at minimum a changelog/upgrade note and an explicit justification that the default flip is intended for all users, not just Zed-primary setups; users with an existing `~/.config/daimonos/mcp_servers.json` will silently stop loading it unless they set servers_file explicitly.
B4. Client identification gating is spoofable/fragile and inconsistently documented: `client_is_zed` matches `client_info.name` case-insensitively against "zed". If Zed ever changes its reported name (e.g., "Zed Editor", "zed-preview") the recovery silently stops working; conversely any client naming itself "zed" gets file-based recovery including the tier-3 shared-file fallback. Also, per the diff, the tier-3 `load_configured_specs` fallback runs for an identified-Zed client even when `zed_config_fallback = false` (the flag only guards tier 2), which contradicts the config doc's claim that recovery is controlled by that flag. Clarify/align the flag semantics: either gate the entire recovery (tiers 2 and 3) on `zed_config_fallback`, or document explicitly that tier 3 applies regardless.
B5. No test coverage for the exec/dry-run CLI semantics or the sync error paths: `--dry-run` combined with `-- COMMAND` bails only AFTER printing the document, and a failed sync correctly prevents exec, but neither is tested; there is also no test that write_atomic sets 0600 or that headers/env survive the round-trip through render_mcp_servers (env is asserted only implicitly). Add tests for: dry-run+exec rejection ordering, 0600 permissions on the written target, and header serialization for Http specs.

### Non-Blocking Suggestions
S1. Sync-then-exec is launch-time only: MCP changes made in Zed while a Cursor ACP session is running are not reflected until the next launch. Consider documenting this staleness window explicitly in docs/zed-acp-setup.md (currently it says the wrapper 'guarantees each new Cursor ACP sees Zed's current MCP list', which is accurate but readers may over-infer live sync).
S2. In `sync()`, when the Zed source parses but yields zero enabled servers, the command still writes an empty `mcpServers` object over Cursor's file, potentially wiping a user's manually-maintained Cursor config. Consider a warning, a `--allow-empty` flag, or at least documenting that the target file is fully owned by the sync and must not be hand-edited.
S3. `clean_jsonc` regex/string-based comment and trailing-comma stripping is reused for the shared servers_file path; consider a property/fuzz test or adopting a maintained JSONC parser, since a mis-strip inside a URL or header value would silently drop or corrupt server entries.
S4. `write_atomic` does not fsync the temp file or parent directory before/after rename, so a crash can leave an empty or missing target. For a file regenerated on every launch this is low risk, but a short comment acknowledging the tradeoff would help future readers.
S5. The eprintln in `sync` reports 'enabled MCP server(s)' but `specs.len()` is the translated count; consider also logging skipped/disabled entry counts so users can diagnose why a server didn't propagate to Cursor.

**Verdict:** REVISE

<!-- event id="2" artifact path="mcp-system-integration-review-mcp-system-integration/artifacts/round-1-rebuttal.diff" sha256="8c0ce3e823970358b5ba00ed778651915f2fb44fbd70881186dbe35a970c894c" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — The exec path is now cfg(unix); non-Unix returns a clear error instructing users to run the synced command separately.
Re B2: ACCEPT — The temp file now uses a UUID, create_new, and Unix OpenOptionsExt mode(0600) before any content is written; it is sync_all'd before atomic rename.
Re B3: ACCEPT — Added an Unreleased changelog migration note explicitly documenting Zed as the new default and how existing users retain ~/.config/daimonos/mcp_servers.json.
Re B4: CLARIFY — The exact case-insensitive client name is intentional and was previously approved: ACP client_info is identification, not authentication, and local Zed settings spawning is already an explicit config policy. I accept the flag inconsistency: zed_config_fallback now gates the entire tier-2/tier-3 recovery chain, with docs and test aligned.
Re B5: ACCEPT — Added tests proving dry-run+exec rejects before source I/O/output, target mode is 0600, and both stdio env and HTTP headers survive serialization.

### Revised Code / Diff
diff --git i/CHANGELOG.md w/CHANGELOG.md
index 85cd74e..c6c35e7 100644
--- i/CHANGELOG.md
+++ w/CHANGELOG.md
@@ -1,5 +1,17 @@
 # Changelog
 
+## Unreleased
+
+- MCP configuration can use Zed's JSONC `context_servers` directly, and
+  `daimonos mcp-config sync -- COMMAND` adapts it for file-only harnesses such
+  as Cursor ACP on every launch. Zed is now the default MCP source for new and
+  default-only Daimonos configurations. Existing users who want to retain
+  `~/.config/daimonos/mcp_servers.json` must set
+  `[agent.mcp].servers_file` explicitly. ACP empty-list recovery is limited to
+  clients identifying as Zed and is controlled by
+  `[acp.mcp].zed_config_fallback`.
+
+
 All notable changes to this project will be documented in this file.
 
 The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
diff --git i/daimonos.default.toml w/daimonos.default.toml
index 8e94464..7df670b 100644
--- i/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -348,8 +348,9 @@ session_list_page_size = 50
 # Outbound MCP servers for daimonos's own agent frontends — interactive TUI,
 # one-shot `agent`, `chat` REPL, and the session daemon (#1289). The ACP path
 # is unaffected (there the client forwards the server list per session).
-# Reads a Claude-style `mcpServers` JSON file — the same format Claude
-# Desktop / Claude Code use — so one file can feed every harness:
+# Reads either Zed's JSONC `context_servers` settings or a Claude/Cursor-style
+# `mcpServers` JSON file. The default points directly at Zed, making it the
+# primary system configuration without copies or symlinks:
 #   { "mcpServers": {
 #       "linear":   { "command": "npx", "args": ["-y", "linear-mcp"] },
 #       "repowise": { "url": "http://127.0.0.1:8931/mcp" } } }
@@ -357,7 +358,7 @@ session_list_page_size = 50
 # Bridge tuning (timeouts, caps, allow_stdio/allow_http, self-connection
 # refusal) is shared with [acp.mcp] below.
 enabled = true
-servers_file = "~/.config/daimonos/mcp_servers.json"
+servers_file = "~/.config/zed/settings.json"
 
 [acp.mcp]
 # MCP-server bridge (ADR-003, #990). Zed forwards every configured context
@@ -399,12 +400,10 @@ max_tools_per_server = 128
 # Fallback for unpatched Zed: when Zed forwards an EMPTY MCP server list at
 # session start (a cold-start race where its context-server store isn't
 # populated yet, and it never re-forwards to a live session), read Zed's own
-# `context_servers` settings directly and bridge those. Off by default because
-# it reads an external app's config and spawns that config's stdio servers, so
-# it must not fire for non-Zed ACP clients. Enable it only when running
-# daimonos as Zed's ACP agent on an unpatched Zed. Only triggers on an empty
-# forwarded list — servers Zed did forward are never overridden.
-zed_config_fallback = false
+# `context_servers` settings directly and bridge those. This is considered
+# only when initialize identifies the ACP client as Zed; an empty list from any
+# other harness remains authoritative. Harness-forwarded servers always win.
+zed_config_fallback = true
 # Path to Zed's settings.json for the fallback above. Empty/unset derives it
 # from $XDG_CONFIG_HOME/$HOME (~/.config/zed/settings.json).
 # zed_settings_path = "/home/you/.config/zed/settings.json"
diff --git i/docs/zed-acp-setup.md w/docs/zed-acp-setup.md
index 2cbfa8f..a9e1aa7 100644
--- i/docs/zed-acp-setup.md
+++ w/docs/zed-acp-setup.md
@@ -13,6 +13,60 @@ used together.
   `daimonos acp` loads it the same way `daimonos agent`/`daimonos chat` do
 - Zed editor installed, with agent-panel support for custom `agent_servers`
 
+## Shared MCP configuration
+
+Daimonos treats Zed as the primary MCP configuration by default. Non-ACP
+agents read `~/.config/zed/settings.json` directly, including its JSONC
+`context_servers`. ACP sessions resolve servers in this order:
+
+1. Servers forwarded by the current ACP harness.
+2. If—and only if—ACP initialize identifies the harness as Zed and it forwards
+   an empty list, Zed `context_servers`.
+3. For that same Zed recovery case, `[agent.mcp].servers_file` as the final
+   Daimonos fallback.
+
+An empty list from Cursor or any unknown/non-Zed ACP harness remains
+authoritative and produces no MCP servers.
+
+To select a Claude/Cursor-style shared file instead:
+
+```toml
+[agent.mcp]
+servers_file = "~/.config/mcp/servers.json"
+```
+
+The selected file may contain either `mcpServers` or `context_servers`; if it
+contains both, `context_servers` takes precedence. A harness that cannot select a path may symlink its standalone MCP file to the
+shared file. Do not symlink all of Zed's `settings.json` over a harness file
+that only accepts a standalone `mcpServers` document. While Zed's file is being
+edited, invalid JSONC causes Daimonos to skip configured MCP servers and emit a
+warning rather than failing the agent session.
+
+### Cursor ACP and other file-only harnesses
+
+Cursor ACP currently ignores ACP-forwarded MCP servers and reads
+`~/.cursor/mcp.json`, whose required `mcpServers` shape is incompatible with
+Zed's full `settings.json`. Use the built-in translator and launch wrapper:
+
+```sh
+daimonos mcp-config sync --target ~/.cursor/mcp.json -- cursor-agent acp
+```
+
+The command reads Zed at process start, atomically regenerates Cursor's file,
+and then replaces itself with Cursor ACP. Configure Zed's `agent_servers.cursor`
+as `type = "custom"` with that command/argument sequence if the registry entry
+cannot be wrapped. This guarantees each new Cursor ACP sees Zed's MCP list as of launch without
+manually copying settings. Changes made while a Cursor session is running take
+effect on its next launch. `--dry-run` prints the generated JSON; omit
+`-- COMMAND` to perform a one-time sync.
+
+Because the formats differ, a direct symlink is unsafe. The generated file is
+mode `0600`; if an old Cursor file exists it is atomically replaced. Disabled
+Zed servers and Zed-only metadata such as `timeout` are omitted. The target
+is fully owned by synchronization and must not be edited manually; a valid Zed
+configuration containing zero enabled servers intentionally produces an empty
+Cursor list.
+
 ## Setup
 
 Add daimonos under the `agent_servers` key in Zed's `settings.json`. The
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index 6737377..ddc71ac 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -444,6 +444,9 @@ struct AcpState {
     supports_images: bool,
     /// Zed's `_meta.terminal_output` extension, negotiated at initialize.
     supports_terminal_output: AtomicBool,
+    /// Whether initialize identified the current ACP client as Zed. File-based
+    /// recovery is never applied to other clients' intentionally empty lists.
+    client_is_zed: AtomicBool,
     /// Maximum sessions returned by one session/list response.
     session_list_page_size: usize,
     /// Context/window compaction configuration cloned into each session.
@@ -1566,42 +1569,38 @@ fn to_server_specs(servers: Vec<McpServer>) -> Vec<ServerSpec> {
         .collect()
 }
 
-/// Resolve the MCP servers to bridge for a session. Normally these are the
-/// list Zed forwards in `session/new`/`session/load`. But unpatched Zed has a
-/// cold-start race where it forwards an EMPTY list (its context-server store
-/// isn't populated when it issues a restored session) and never re-forwards to
-/// a live session — leaving the session with no MCP tools. So when the
-/// forwarded list is empty (and the fallback is enabled), read Zed's own
-/// `context_servers` settings directly and bridge those instead (see
-/// [`crate::zed_config`]). Only the empty case triggers the fallback; servers
-/// Zed did forward are never overridden.
-fn resolve_mcp_specs(forwarded: Vec<McpServer>, cfg: &Config) -> Vec<ServerSpec> {
+/// Resolve MCP servers in precedence order: the ACP harness first, then its
+/// Zed settings fallback, then Daimonos's configured shared servers file.
+async fn resolve_mcp_specs(
+    forwarded: Vec<McpServer>,
+    cfg: &Config,
+    client_is_zed: bool,
+) -> Vec<ServerSpec> {
     let specs = to_server_specs(forwarded);
-    if !specs.is_empty() || !cfg.acp.mcp.enabled || !cfg.acp.mcp.zed_config_fallback {
+    // An empty list from an unknown/non-Zed client may be intentional. Zed is
+    // the sole exception because its known cold-start race loses configured
+    // servers before session/new or session/load.
+    if !specs.is_empty()
+        || !cfg.acp.mcp.enabled
+        || !client_is_zed
+        || !cfg.acp.mcp.zed_config_fallback
+    {
         return specs;
     }
-    match crate::zed_config::context_server_specs(cfg.acp.mcp.zed_settings_path.as_deref()) {
-        Ok(fallback) if !fallback.is_empty() => {
-            tracing::warn!(
-                target: "daimonos::acp",
-                event = "mcp_forward_empty_fallback",
-                recovered = fallback.len(),
-                "Zed forwarded no MCP servers; recovered them from Zed settings \
-                 (unpatched-Zed cold-start race)"
-            );
-            fallback
-        }
-        Ok(_) => specs,
-        Err(e) => {
-            tracing::warn!(
-                target: "daimonos::acp",
-                event = "mcp_fallback_failed",
-                error = %e,
-                "Zed forwarded no MCP servers and reading Zed settings failed"
-            );
-            specs
+    {
+        match crate::zed_config::context_server_specs(cfg.acp.mcp.zed_settings_path.as_deref()) {
+            Ok(fallback) if !fallback.is_empty() => {
+                tracing::warn!(target: "daimonos::acp", event = "mcp_forward_empty_fallback",
+                    recovered = fallback.len(),
+                    "ACP harness forwarded no MCP servers; recovered Zed settings");
+                return fallback;
+            }
+            Ok(_) => {}
+            Err(error) => tracing::warn!(target: "daimonos::acp", event = "mcp_fallback_failed",
+                %error, "ACP harness forwarded no MCP servers and Zed settings failed"),
         }
     }
+    crate::agent_mcp::load_configured_specs(cfg).await
 }
 
 fn should_refresh_mcp_bridge(
@@ -2584,6 +2583,7 @@ fn build_agent_with_state(
         }),
         supports_images,
         supports_terminal_output: AtomicBool::new(false),
+        client_is_zed: AtomicBool::new(false),
         session_list_page_size: cfg.acp.session_list_page_size,
         compaction,
         analytics,
@@ -2610,6 +2610,11 @@ fn build_agent_with_state(
                         state
                             .supports_terminal_output
                             .store(client_supports_terminal_output(&req), Ordering::Release);
+                        let is_zed = req
+                            .client_info
+                            .as_ref()
+                            .is_some_and(|info| info.name.eq_ignore_ascii_case("zed"));
+                        state.client_is_zed.store(is_zed, Ordering::Release);
                         // load_session(true): Zed calls session/load to reopen
                         // a thread on window refocus.
                         let mut capabilities = AgentCapabilities::new()
@@ -2876,11 +2881,15 @@ fn build_agent_with_state(
                     let token_log = token_log.clone();
                     async move {
                         let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
-                        // Zed forwards the user's configured MCP servers here
-                        // (ADR-003); bridge them into this session. Falls back
-                        // to Zed's settings when the forwarded list is empty
-                        // (unpatched-Zed cold-start race).
-                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
+                        // The current harness's non-empty list is authoritative.
+                        // Only an identified Zed client gets file recovery for
+                        // its known empty-forward cold-start race.
+                        let mcp_specs = resolve_mcp_specs(
+                            req.mcp_servers,
+                            &cfg,
+                            state.client_is_zed.load(Ordering::Acquire),
+                        )
+                        .await;
                         let mcp_server_count = mcp_specs.len();
                         // Use the client-provided project root, not the CLI's
                         // own cwd — Zed passes the actual project it wants this
@@ -3000,9 +3009,14 @@ fn build_agent_with_state(
                     let token_log = token_log.clone();
                     async move {
                         let session_id = req.session_id.clone();
-                        // Same empty-forward fallback as session/new: a
-                        // reloaded thread must also recover its MCP servers.
-                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
+                        // Apply the same harness-first, Zed-gated recovery as
+                        // session/new so restored Zed threads recover MCP tools.
+                        let mcp_specs = resolve_mcp_specs(
+                            req.mcp_servers,
+                            &cfg,
+                            state.client_is_zed.load(Ordering::Acquire),
+                        )
+                        .await;
                         let session_workspace = if req.cwd.as_os_str().is_empty() {
                             workspace_fallback
                         } else {
@@ -4026,8 +4040,8 @@ mod tests {
         );
     }
 
-    #[test]
-    fn resolve_mcp_specs_fallback_gating() {
+    #[tokio::test]
+    async fn resolve_mcp_specs_fallback_gating() {
         let dir = tempfile::tempdir().unwrap();
         let path = dir.path().join("settings.json");
         std::fs::write(
@@ -4038,23 +4052,45 @@ mod tests {
         let mut cfg = Config::default();
         cfg.acp.mcp.zed_settings_path = Some(path.to_string_lossy().into_owned());
 
-        // Enabled + empty forward -> read Zed config fallback.
+        // Identified Zed + empty forward -> read Zed config fallback.
         cfg.acp.mcp.zed_config_fallback = true;
-        assert_eq!(resolve_mcp_specs(vec![], &cfg).len(), 1);
+        assert_eq!(resolve_mcp_specs(vec![], &cfg, true).await.len(), 1);
 
-        // Disabled (the default) -> no fallback, even with an empty forward.
+        // An empty list from any other harness is authoritative.
+        assert!(resolve_mcp_specs(vec![], &cfg, false).await.is_empty());
+
+        // With both file fallbacks disabled, an empty forward stays empty.
         cfg.acp.mcp.zed_config_fallback = false;
-        assert!(resolve_mcp_specs(vec![], &cfg).is_empty());
+        cfg.agent.mcp.enabled = false;
+        assert!(resolve_mcp_specs(vec![], &cfg, true).await.is_empty());
 
-        // A non-empty forward is never overridden by the fallback.
+        // Tier 3: identified Zed, no Zed settings, configured shared file.
+        cfg.acp.mcp.zed_settings_path =
+            Some(dir.path().join("missing.json").to_string_lossy().into());
+        let shared = dir.path().join("shared.json");
+        std::fs::write(
+            &shared,
+            r#"{ "mcpServers": { "shared": { "command": "x" } } }"#,
+        )
+        .unwrap();
+        cfg.agent.mcp.servers_file = shared.to_string_lossy().into();
+        cfg.agent.mcp.enabled = true;
         cfg.acp.mcp.zed_config_fallback = true;
+        assert!(matches!(
+            &resolve_mcp_specs(vec![], &cfg, true).await[0],
+            ServerSpec::Stdio { name, .. } if name == "shared"
+        ));
+
+        // A non-empty forward is never overridden by either fallback.
+        cfg.acp.mcp.zed_config_fallback = true;
+        cfg.agent.mcp.enabled = true;
         let forwarded = vec![McpServer::Http(
             agent_client_protocol::schema::v1::McpServerHttp::new(
                 "fwd".to_string(),
                 "http://127.0.0.1:10/".to_string(),
             ),
         )];
-        let specs = resolve_mcp_specs(forwarded, &cfg);
+        let specs = resolve_mcp_specs(forwarded, &cfg, true).await;
         assert_eq!(specs.len(), 1);
         assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "fwd"));
     }
diff --git i/src/agent_mcp.rs w/src/agent_mcp.rs
index 850e293..1a6073c 100644
--- i/src/agent_mcp.rs
+++ w/src/agent_mcp.rs
@@ -24,19 +24,27 @@ use crate::config::Config;
 use crate::mcp_bridge::{McpBridge, McpClientPool, ServerSpec};
 use crate::providers::ToolSchema;
 
-/// Parse a Claude-style config: `{"mcpServers": {name: {command, args?, env?}
-/// | {url, headers?}}}`. Entries are sorted by name so bridge order (and thus
-/// the `max_servers` cap) is deterministic — JSON object order is not
-/// preserved. Unknown per-server keys are ignored for forward compatibility;
-/// a `type` field, when present, must agree with the transport implied by
-/// `command`/`url`.
+/// Parse either a Claude/Cursor-style `mcpServers` file or Zed's
+/// `settings.json` (`context_servers`). This lets `[agent.mcp].servers_file`
+/// point directly at the system's primary Zed configuration instead of
+/// maintaining a translated copy. If both top-level keys exist,
+/// `context_servers` takes precedence. Entries are sorted by name so bridge
+/// order (and thus the `max_servers` cap) is deterministic.
 pub fn parse_servers_json(content: &str) -> Result<Vec<ServerSpec>, String> {
+    // Zed settings are JSONC. Reuse its reader's string-aware cleanup before
+    // parsing; ordinary JSON passes through unchanged.
+    let cleaned = crate::zed_config::clean_jsonc(content);
     let root: Value =
-        serde_json::from_str(content).map_err(|error| format!("invalid JSON: {error}"))?;
+        serde_json::from_str(&cleaned).map_err(|error| format!("invalid JSON/JSONC: {error}"))?;
+    if root.get("context_servers").is_some() {
+        return Ok(crate::zed_config::specs_from_settings(&root));
+    }
     let servers = root
         .get("mcpServers")
         .and_then(Value::as_object)
-        .ok_or_else(|| "missing top-level \"mcpServers\" object".to_string())?;
+        .ok_or_else(|| {
+            "missing top-level \"mcpServers\" or \"context_servers\" object".to_string()
+        })?;
     let mut names: Vec<&String> = servers.keys().collect();
     names.sort();
     names
@@ -156,6 +164,32 @@ impl AgentMcp {
     }
 }
 
+/// Load the system/user MCP configuration selected by `[agent.mcp]`.
+/// Both Claude/Cursor and Zed settings formats are accepted.
+pub async fn load_configured_specs(cfg: &Config) -> Vec<ServerSpec> {
+    if !cfg.agent.mcp.enabled {
+        return Vec::new();
+    }
+    let path = crate::paths::expand_tilde(&cfg.agent.mcp.servers_file);
+    let content = match tokio::fs::read_to_string(&path).await {
+        Ok(content) => content,
+        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
+        Err(error) => {
+            tracing::warn!(target: "daimonos::agent_mcp", path = %path.display(), %error,
+                "mcp servers file unreadable; continuing without configured MCP servers");
+            return Vec::new();
+        }
+    };
+    match parse_servers_json(&content) {
+        Ok(specs) => specs,
+        Err(error) => {
+            tracing::warn!(target: "daimonos::agent_mcp", path = %path.display(), %error,
+                "mcp servers file invalid; continuing without configured MCP servers");
+            Vec::new()
+        }
+    }
+}
+
 /// Read `[agent.mcp] servers_file` and connect. `None` when disabled, the
 /// file is absent or empty, or it fails to read/parse (logged) — the agent
 /// then runs with native tools only.
@@ -164,35 +198,7 @@ pub async fn connect(
     native_tool_names: &HashSet<String>,
     analytics: Option<Arc<AnalyticsStore>>,
 ) -> Option<AgentMcp> {
-    if !cfg.agent.mcp.enabled {
-        return None;
-    }
-    let path = crate::paths::expand_tilde(&cfg.agent.mcp.servers_file);
-    let content = match tokio::fs::read_to_string(&path).await {
-        Ok(content) => content,
-        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
-        Err(error) => {
-            tracing::warn!(
-                target: "daimonos::agent_mcp",
-                path = %path.display(),
-                %error,
-                "mcp servers file unreadable; continuing with native tools only"
-            );
-            return None;
-        }
-    };
-    let specs = match parse_servers_json(&content) {
-        Ok(specs) => specs,
-        Err(error) => {
-            tracing::warn!(
-                target: "daimonos::agent_mcp",
-                path = %path.display(),
-                %error,
-                "mcp servers file invalid; continuing with native tools only"
-            );
-            return None;
-        }
-    };
+    let specs = load_configured_specs(cfg).await;
     if specs.is_empty() {
         return None;
     }
@@ -257,6 +263,20 @@ mod tests {
         }
     }
 
+    #[test]
+    fn parses_zed_jsonc_context_servers() {
+        let jsonc = r#"{
+          // one primary config for Zed and Daimonos
+          "context_servers": {
+            "zed-http": { "url": "https://example.test/mcp", },
+            "off": { "enabled": false, "command": "nope" },
+          },
+        }"#;
+        let specs = parse_servers_json(jsonc).unwrap();
+        assert_eq!(specs.len(), 1);
+        assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "zed-http"));
+    }
+
     #[test]
     fn type_field_is_validated_but_optional() {
         let ok = r#"{"mcpServers": {"a": {"type": "stdio", "command": "x"},
@@ -276,7 +296,7 @@ mod tests {
     fn rejects_malformed_documents_legibly() {
         assert!(parse_servers_json("not json")
             .unwrap_err()
-            .contains("invalid JSON"));
+            .contains("invalid JSON/JSONC"));
         assert!(parse_servers_json("{}").unwrap_err().contains("mcpServers"));
         assert!(parse_servers_json(r#"{"mcpServers": {"a": {}}}"#)
             .unwrap_err()
diff --git i/src/cli.rs w/src/cli.rs
index 5c44499..9b158cd 100644
--- i/src/cli.rs
+++ w/src/cli.rs
@@ -115,6 +115,31 @@ pub struct McpArgs {
     pub socket: Option<PathBuf>,
 }
 
+#[derive(Debug, Args)]
+pub struct McpConfigArgs {
+    #[command(subcommand)]
+    pub command: McpConfigCommand,
+}
+
+#[derive(Debug, Subcommand)]
+pub enum McpConfigCommand {
+    /// Translate Zed context_servers into a harness mcpServers file.
+    Sync {
+        /// Zed settings.json source (defaults to the platform Zed path).
+        #[arg(long, value_name = "FILE")]
+        source: Option<PathBuf>,
+        /// Harness MCP file to replace atomically.
+        #[arg(long, value_name = "FILE", default_value = "~/.cursor/mcp.json")]
+        target: PathBuf,
+        /// Print the generated document without writing it.
+        #[arg(long)]
+        dry_run: bool,
+        /// After a successful sync, replace this process with COMMAND.
+        #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
+        exec: Vec<String>,
+    },
+}
+
 #[derive(Debug, Args)]
 pub struct SessionArgs {
     #[command(subcommand)]
@@ -156,6 +181,8 @@ pub enum Command {
     Session(SessionArgs),
     /// Run the MCP tool server over stdio or a Unix socket.
     Mcp(McpArgs),
+    /// Synchronize Zed's MCP servers into harness-specific configuration.
+    McpConfig(McpConfigArgs),
     /// Run the compact opcode protocol daemon over a Unix socket.
     Daemon,
 }
@@ -169,6 +196,7 @@ pub enum RuntimeMode {
     Session,
     McpStdio,
     McpSocket(PathBuf),
+    McpConfig,
     Daemon,
     Stats,
 }
@@ -183,6 +211,7 @@ impl RuntimeMode {
             Self::Session => "session",
             Self::McpStdio => "mcp_stdio",
             Self::McpSocket(_) => "mcp_socket",
+            Self::McpConfig => "mcp_config",
             Self::Daemon => "socket",
             Self::Stats => "stats",
         }
@@ -270,6 +299,7 @@ impl Cli {
             Some(Command::Acp(_)) => RuntimeMode::Acp,
             Some(Command::SessionDaemon(_)) => RuntimeMode::SessionDaemon,
             Some(Command::Session(_)) => RuntimeMode::Session,
+            Some(Command::McpConfig(_)) => RuntimeMode::McpConfig,
             Some(Command::Mcp(args)) => args
                 .socket
                 .clone()
diff --git i/src/config.rs w/src/config.rs
index cdd9f90..e9bc6d9 100644
--- i/src/config.rs
+++ w/src/config.rs
@@ -68,7 +68,7 @@ impl Default for AgentMcpConfig {
     fn default() -> Self {
         Self {
             enabled: true,
-            servers_file: "~/.config/daimonos/mcp_servers.json".to_string(),
+            servers_file: "~/.config/zed/settings.json".to_string(),
         }
     }
 }
@@ -1043,10 +1043,10 @@ pub struct AcpMcpConfig {
     /// settings directly and bridge those. Only triggers on an empty forwarded
     /// list — never overrides servers Zed did forward.
     ///
-    /// **Opt-in (default false):** it reads an external app's config file and
-    /// spawns that config's stdio servers, so it must not fire for non-Zed ACP
-    /// clients or in tests. Enable it only when running daimonos as Zed's ACP
-    /// agent on an unpatched Zed.
+    /// This is gated on ACP initialize identifying the client as Zed, so an
+    /// empty list from another harness remains authoritative. When enabled for
+    /// Zed, this flag gates the whole recovery chain; `[agent.mcp].servers_file`
+    /// is the final fallback if this file has no usable servers.
     pub zed_config_fallback: bool,
     /// Path to Zed's `settings.json` for `zed_config_fallback`. `None` derives
     /// it from `$XDG_CONFIG_HOME`/`$HOME` (`~/.config/zed/settings.json`).
@@ -1067,7 +1067,7 @@ impl Default for AcpMcpConfig {
             max_servers: 32,
             max_concurrent_connects: 8,
             max_tools_per_server: 128,
-            zed_config_fallback: false,
+            zed_config_fallback: true,
             zed_settings_path: None,
         }
     }
diff --git i/src/main.rs w/src/main.rs
index e7f0a77..8e4fb09 100644
--- i/src/main.rs
+++ w/src/main.rs
@@ -23,6 +23,7 @@ mod loop_detector;
 mod managed_process;
 mod mcp;
 mod mcp_bridge;
+mod mcp_config_cmd;
 mod observability;
 mod ops;
 mod paths;
@@ -243,7 +244,8 @@ async fn main() -> anyhow::Result<()> {
         Some(Command::Agent(args)) => !args.dry_run,
         Some(Command::Chat(args)) => !args.list,
         Some(Command::Acp(_) | Command::SessionDaemon(_)) => true,
-        Some(Command::Session(_) | Command::Mcp(_) | Command::Daemon) | None => false,
+        Some(Command::Session(_) | Command::Mcp(_) | Command::McpConfig(_) | Command::Daemon)
+        | None => false,
     };
     if uses_agent_prompt {
         cfg.prompts.additional_agent_instructions =
@@ -336,6 +338,7 @@ async fn main() -> anyhow::Result<()> {
             agent_runtime::run_session_daemon(args, &workspace, Arc::clone(&cfg), token_log).await
         }
         Some(Command::Session(args)) => session_interchange::run(args, &cfg),
+        Some(Command::McpConfig(args)) => mcp_config_cmd::run(args),
         Some(Command::Mcp(_) | Command::Daemon) | None => {
             run_tool_service(
                 runtime_mode,
@@ -481,6 +484,7 @@ async fn run_tool_service(
         | RuntimeMode::Acp
         | RuntimeMode::Session
         | RuntimeMode::SessionDaemon
+        | RuntimeMode::McpConfig
         | RuntimeMode::Stats => {
             unreachable!("early-return runtime reached service dispatch")
         }
diff --git i/src/mcp_config_cmd.rs w/src/mcp_config_cmd.rs
new file mode 100644
index 0000000..8658687
--- /dev/null
+++ w/src/mcp_config_cmd.rs
@@ -0,0 +1,197 @@
+//! Translate Zed's JSONC `context_servers` into standalone `mcpServers` JSON.
+//! Some proprietary harnesses (notably Cursor ACP) ignore ACP-forwarded MCP
+//! servers and only read their own file, so a symlink cannot bridge the
+//! incompatible top-level formats. This command provides an atomic adapter.
+
+use std::io::Write;
+use std::path::{Path, PathBuf};
+
+use anyhow::Context;
+#[cfg(unix)]
+use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
+
+use crate::cli::{McpConfigArgs, McpConfigCommand};
+use crate::mcp_bridge::ServerSpec;
+
+pub fn run(args: McpConfigArgs) -> anyhow::Result<()> {
+    match args.command {
+        McpConfigCommand::Sync {
+            source,
+            target,
+            dry_run,
+            exec,
+        } => sync(source, target, dry_run, exec),
+    }
+}
+
+fn sync(
+    source: Option<PathBuf>,
+    target: PathBuf,
+    dry_run: bool,
+    exec: Vec<String>,
+) -> anyhow::Result<()> {
+    if dry_run && !exec.is_empty() {
+        anyhow::bail!("--dry-run cannot be combined with a command after --");
+    }
+    let source = source
+        .map(|p| crate::paths::expand_tilde(p.to_string_lossy().as_ref()))
+        .unwrap_or_else(|| crate::zed_config::default_settings_path());
+    let target = crate::paths::expand_tilde(target.to_string_lossy().as_ref());
+    let specs = crate::zed_config::context_server_specs(Some(source.to_string_lossy().as_ref()))?;
+    let document = render_mcp_servers(&specs)?;
+    if dry_run {
+        print!("{document}");
+    } else {
+        write_atomic(&target, document.as_bytes())?;
+        eprintln!(
+            "synced {} enabled MCP server(s) from {} to {}",
+            specs.len(),
+            source.display(),
+            target.display()
+        );
+    }
+    if exec.is_empty() {
+        return Ok(());
+    }
+    #[cfg(unix)]
+    {
+        use std::os::unix::process::CommandExt;
+        let error = std::process::Command::new(&exec[0]).args(&exec[1..]).exec();
+        Err(anyhow::anyhow!("exec {}: {error}", exec[0]))
+    }
+    #[cfg(not(unix))]
+    anyhow::bail!("sync-then-exec is supported only on Unix; run the synced command separately")
+}
+
+fn render_mcp_servers(specs: &[ServerSpec]) -> anyhow::Result<String> {
+    let mut servers = serde_json::Map::new();
+    for spec in specs {
+        let (name, entry) = match spec {
+            ServerSpec::Stdio {
+                name,
+                command,
+                args,
+                env,
+            } => {
+                let mut entry = serde_json::Map::new();
+                entry.insert("command".into(), command.clone().into());
+                if !args.is_empty() {
+                    entry.insert("args".into(), serde_json::to_value(args)?);
+                }
+                if !env.is_empty() {
+                    entry.insert("env".into(), serde_json::to_value(env)?);
+                }
+                (name, entry)
+            }
+            ServerSpec::Http { name, url, headers } => {
+                let mut entry = serde_json::Map::new();
+                entry.insert("url".into(), url.clone().into());
+                if !headers.is_empty() {
+                    entry.insert("headers".into(), serde_json::to_value(headers)?);
+                }
+                (name, entry)
+            }
+        };
+        servers.insert(name.clone(), entry.into());
+    }
+    let mut root = serde_json::Map::new();
+    root.insert("mcpServers".into(), servers.into());
+    Ok(serde_json::to_string_pretty(&root)? + "\n")
+}
+
+fn write_atomic(path: &Path, content: &[u8]) -> anyhow::Result<()> {
+    let parent = path
+        .parent()
+        .ok_or_else(|| anyhow::anyhow!("target has no parent: {}", path.display()))?;
+    std::fs::create_dir_all(parent)?;
+    let tmp = parent.join(format!(
+        ".{}.daimonos-sync-{}-{}",
+        path.file_name()
+            .and_then(|n| n.to_str())
+            .unwrap_or("mcp.json"),
+        std::process::id(),
+        uuid::Uuid::new_v4()
+    ));
+    let mut options = std::fs::OpenOptions::new();
+    options.write(true).create_new(true);
+    #[cfg(unix)]
+    options.mode(0o600);
+    let mut file = options
+        .open(&tmp)
+        .with_context(|| format!("create private temporary file {}", tmp.display()))?;
+    file.write_all(content)?;
+    file.sync_all()?;
+    drop(file);
+    std::fs::rename(&tmp, path).map_err(|error| {
+        let _ = std::fs::remove_file(&tmp);
+        anyhow::anyhow!("replace {} atomically: {error}", path.display())
+    })?;
+    #[cfg(unix)]
+    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
+        .with_context(|| format!("secure {}", path.display()))?;
+    Ok(())
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use std::collections::HashMap;
+
+    #[test]
+    fn renders_cursor_compatible_document() {
+        let specs = vec![
+            ServerSpec::Stdio {
+                name: "local".into(),
+                command: "server".into(),
+                args: vec!["--stdio".into()],
+                env: HashMap::from([("TOKEN_FILE".into(), "/secret/path".into())]),
+            },
+            ServerSpec::Http {
+                name: "remote".into(),
+                url: "https://example.test/mcp".into(),
+                headers: HashMap::from([("Authorization".into(), "Bearer test".into())]),
+            },
+        ];
+        let value: serde_json::Value =
+            serde_json::from_str(&render_mcp_servers(&specs).unwrap()).unwrap();
+        assert_eq!(value["mcpServers"]["local"]["command"], "server");
+        assert_eq!(
+            value["mcpServers"]["local"]["env"]["TOKEN_FILE"],
+            "/secret/path"
+        );
+        assert_eq!(
+            value["mcpServers"]["remote"]["url"],
+            "https://example.test/mcp"
+        );
+        assert_eq!(
+            value["mcpServers"]["remote"]["headers"]["Authorization"],
+            "Bearer test"
+        );
+    }
+
+    #[test]
+    fn atomic_write_replaces_existing_file() {
+        let dir = tempfile::tempdir().unwrap();
+        let path = dir.path().join("mcp.json");
+        std::fs::write(&path, "old").unwrap();
+        write_atomic(&path, b"new\n").unwrap();
+        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
+        #[cfg(unix)]
+        assert_eq!(
+            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
+            0o600
+        );
+    }
+
+    #[test]
+    fn dry_run_with_exec_is_rejected_before_source_io_or_output() {
+        let error = sync(
+            Some(PathBuf::from("/definitely/missing/settings.json")),
+            PathBuf::from("/unused/mcp.json"),
+            true,
+            vec!["echo".into()],
+        )
+        .unwrap_err();
+        assert!(error.to_string().contains("--dry-run cannot be combined"));
+    }
+}
diff --git i/src/zed_config.rs w/src/zed_config.rs
index 438210d..1db34a1 100644
--- i/src/zed_config.rs
+++ w/src/zed_config.rs
@@ -43,12 +43,16 @@ pub fn context_server_specs(override_path: Option<&str>) -> anyhow::Result<Vec<S
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
         Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
     };
-    let json = strip_trailing_commas(&strip_jsonc(&raw));
+    let json = clean_jsonc(&raw);
     let value: serde_json::Value = serde_json::from_str(&json)
         .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
     Ok(specs_from_settings(&value))
 }
 
+pub(crate) fn default_settings_path() -> PathBuf {
+    settings_path(None).unwrap_or_else(|| PathBuf::from("~/.config/zed/settings.json"))
+}
+
 fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
     if let Some(path) = override_path {
         return Some(PathBuf::from(path));
@@ -61,7 +65,7 @@ fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
 
 /// Map the `context_servers` object of a parsed Zed settings document to
 /// [`ServerSpec`]s. Unknown/incomplete entries are skipped.
-fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
+pub(crate) fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
     let Some(servers) = value.get("context_servers").and_then(|v| v.as_object()) else {
         return Vec::new();
     };
@@ -119,6 +123,12 @@ fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
         .unwrap_or_default()
 }
 
+/// Normalize Zed JSONC into strict JSON for both direct fallback and shared
+/// `[agent.mcp].servers_file` consumption.
+pub(crate) fn clean_jsonc(input: &str) -> String {
+    strip_trailing_commas(&strip_jsonc(input))
+}
+
 /// Remove `//` line comments and `/* */` block comments from a JSONC document,
 /// preserving anything inside string literals (so `"http://…"` and comment-like
 /// content in strings survive).

### New Points
R1. Accepted S1 and S2: docs clarify launch-time staleness, full target ownership, and intentional empty-list replacement. S3 is covered by existing string-aware JSONC tests; S4 is improved with file sync_all; S5 is non-blocking because translation silently omits only explicitly disabled/incomplete Zed entries, matching existing parser behavior.
R2. Clean validation: cargo check passed; mcp_config 3/3, agent_mcp 5/5, zed_config 4/4, and ACP precedence 1/1 passed. Live Cursor reports all 11 enabled Zed MCP servers ready, and the generated file is 0600.

**Requesting:** RE-REVIEW

<!-- event id="3" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. The early dry_run+exec check in sync() now bails before any I/O, which is stricter than the documented behavior implied in round 1; consider mentioning in --dry-run help text that it is incompatible with '-- COMMAND' so users discover the constraint from `--help` rather than a runtime error.
S7. In the revised resolve_mcp_specs, the inner bare block `{ match ... }` left over from restructuring is now redundant since zed_config_fallback is checked in the top guard; flattening it would slightly improve readability. Purely cosmetic.

### Responses to Prior Points
Re R1: Accepted. The revised docs/zed-acp-setup.md now states that changes take effect on the next Cursor launch (S1), and explicitly documents that the target is fully owned by synchronization, must not be hand-edited, and that a zero-enabled-server Zed config intentionally produces an empty Cursor list (S2). The remaining suggestions (S3 fuzz/property coverage for clean_jsonc, S5 skipped-entry counts) stay non-blocking; note that sync_all addresses only part of S4 (no parent-directory fsync after rename), which remains an acceptable documented-tradeoff candidate rather than a defect for a launch-regenerated file.
Re R2: Acknowledged. The reported validation (cargo check clean; mcp_config 3/3, agent_mcp 5/5, zed_config 4/4, ACP precedence 1/1; live Cursor seeing 11 enabled servers with a 0600 target) is consistent with the revised diff. I cannot independently execute tests, so I rely on the diff contents, which corroborate the claims: cfg(unix)-gated exec with a non-Unix bail, create_new + mode(0o600) + UUID temp name + sync_all before rename, changelog migration note, zed_config_fallback gating the entire tier-2/tier-3 chain with updated config docs and test, and new tests for dry-run+exec ordering, 0600 permissions, and env/header serialization.
B1: resolved
B2: resolved
B3: resolved
B4: resolved
B5: resolved

**Verdict:** APPROVE
