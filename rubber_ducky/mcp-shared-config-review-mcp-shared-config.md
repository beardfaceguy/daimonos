# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="mcp-shared-config-review" -->

<!-- event id="request" artifact path="mcp-shared-config-review-mcp-shared-config/artifacts/round-1-review-request.diff" sha256="57181c5b1c16285b2f6d8c46cae75776e051b0c9220b8feb2460d735357e026b" -->
## Review Request — Round 1
**Task:** mcp-shared-config-review — Shared Zed/ACP MCP configuration and harness-first precedence
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Use Zed settings as Daimonos's default shared configuration, parse both Zed context_servers JSONC and Claude/Cursor mcpServers JSON, and resolve ACP servers from harness first, then Zed, then Daimonos agent configuration.

### Relevant Code / Diff
diff --git i/daimonos.default.toml w/daimonos.default.toml
index 8e94464..10ad18c 100644
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
@@ -400,11 +401,10 @@ max_tools_per_server = 128
 # session start (a cold-start race where its context-server store isn't
 # populated yet, and it never re-forwards to a live session), read Zed's own
 # `context_servers` settings directly and bridge those. Off by default because
-# it reads an external app's config and spawns that config's stdio servers, so
-# it must not fire for non-Zed ACP clients. Enable it only when running
-# daimonos as Zed's ACP agent on an unpatched Zed. Only triggers on an empty
-# forwarded list — servers Zed did forward are never overridden.
-zed_config_fallback = false
+# it reads an external app's config and spawns that config's stdio servers.
+# Harness-forwarded servers always win; this is consulted only for an empty
+# forwarded list, before `[agent.mcp].servers_file`.
+zed_config_fallback = true
 # Path to Zed's settings.json for the fallback above. Empty/unset derives it
 # from $XDG_CONFIG_HOME/$HOME (~/.config/zed/settings.json).
 # zed_settings_path = "/home/you/.config/zed/settings.json"
diff --git i/docs/zed-acp-setup.md w/docs/zed-acp-setup.md
index 2cbfa8f..9a5d0c4 100644
--- i/docs/zed-acp-setup.md
+++ w/docs/zed-acp-setup.md
@@ -13,6 +13,28 @@ used together.
   `daimonos acp` loads it the same way `daimonos agent`/`daimonos chat` do
 - Zed editor installed, with agent-panel support for custom `agent_servers`
 
+## Shared MCP configuration
+
+Daimonos treats Zed as the primary MCP configuration by default. Non-ACP
+agents read `~/.config/zed/settings.json` directly, including its JSONC
+`context_servers`. ACP sessions resolve servers in this order:
+
+1. Servers forwarded by the current ACP harness.
+2. Zed `context_servers` when the harness forwards an empty list.
+3. `[agent.mcp].servers_file` as the Daimonos fallback.
+
+To select a Claude/Cursor-style shared file instead:
+
+```toml
+[agent.mcp]
+servers_file = "~/.config/mcp/servers.json"
+```
+
+The selected file may contain either `mcpServers` or `context_servers`. A
+harness that cannot select a path may symlink its standalone MCP file to the
+shared file. Do not symlink all of Zed's `settings.json` over a harness file
+that only accepts a standalone `mcpServers` document.
+
 ## Setup
 
 Add daimonos under the `agent_servers` key in Zed's `settings.json`. The
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index 6737377..a779d54 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -1566,42 +1566,27 @@ fn to_server_specs(servers: Vec<McpServer>) -> Vec<ServerSpec> {
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
+async fn resolve_mcp_specs(forwarded: Vec<McpServer>, cfg: &Config) -> Vec<ServerSpec> {
     let specs = to_server_specs(forwarded);
-    if !specs.is_empty() || !cfg.acp.mcp.enabled || !cfg.acp.mcp.zed_config_fallback {
+    if !specs.is_empty() || !cfg.acp.mcp.enabled {
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
@@ -2880,7 +2865,7 @@ fn build_agent_with_state(
                         // (ADR-003); bridge them into this session. Falls back
                         // to Zed's settings when the forwarded list is empty
                         // (unpatched-Zed cold-start race).
-                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
+                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg).await;
                         let mcp_server_count = mcp_specs.len();
                         // Use the client-provided project root, not the CLI's
                         // own cwd — Zed passes the actual project it wants this
@@ -3002,7 +2987,7 @@ fn build_agent_with_state(
                         let session_id = req.session_id.clone();
                         // Same empty-forward fallback as session/new: a
                         // reloaded thread must also recover its MCP servers.
-                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
+                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg).await;
                         let session_workspace = if req.cwd.as_os_str().is_empty() {
                             workspace_fallback
                         } else {
@@ -4026,8 +4011,8 @@ mod tests {
         );
     }
 
-    #[test]
-    fn resolve_mcp_specs_fallback_gating() {
+    #[tokio::test]
+    async fn resolve_mcp_specs_fallback_gating() {
         let dir = tempfile::tempdir().unwrap();
         let path = dir.path().join("settings.json");
         std::fs::write(
@@ -4040,21 +4025,23 @@ mod tests {
 
         // Enabled + empty forward -> read Zed config fallback.
         cfg.acp.mcp.zed_config_fallback = true;
-        assert_eq!(resolve_mcp_specs(vec![], &cfg).len(), 1);
+        assert_eq!(resolve_mcp_specs(vec![], &cfg).await.len(), 1);
 
-        // Disabled (the default) -> no fallback, even with an empty forward.
+        // With both file fallbacks disabled, an empty forward stays empty.
         cfg.acp.mcp.zed_config_fallback = false;
-        assert!(resolve_mcp_specs(vec![], &cfg).is_empty());
+        cfg.agent.mcp.enabled = false;
+        assert!(resolve_mcp_specs(vec![], &cfg).await.is_empty());
 
-        // A non-empty forward is never overridden by the fallback.
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
+        let specs = resolve_mcp_specs(forwarded, &cfg).await;
         assert_eq!(specs.len(), 1);
         assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "fwd"));
     }
@@ -7720,6 +7707,7 @@ mod tests {
             stop_reason,
             error_message: None,
             context_overflow: false,
+            evidence: Default::default(),
         };
         assert_eq!(
             canonical_assistant_outcome(&turn(crate::providers::StopReason::EndTurn)),
diff --git i/src/agent_mcp.rs w/src/agent_mcp.rs
index 850e293..a6a9163 100644
--- i/src/agent_mcp.rs
+++ w/src/agent_mcp.rs
@@ -24,24 +24,32 @@ use crate::config::Config;
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
+/// maintaining a translated copy. Entries are sorted by name so bridge order
+/// (and thus the `max_servers` cap) is deterministic.
 pub fn parse_servers_json(content: &str) -> Result<Vec<ServerSpec>, String> {
+    // Zed settings are JSONC. Reuse its reader's string-aware cleanup before
+    // parsing; ordinary JSON passes through unchanged.
+    let cleaned = crate::zed_config::clean_jsonc(content);
     let root: Value =
-        serde_json::from_str(content).map_err(|error| format!("invalid JSON: {error}"))?;
+        serde_json::from_str(&cleaned).map_err(|error| format!("invalid JSON/JSONC: {error}"))?;
     let servers = root
         .get("mcpServers")
+        .or_else(|| root.get("context_servers"))
         .and_then(Value::as_object)
-        .ok_or_else(|| "missing top-level \"mcpServers\" object".to_string())?;
+        .ok_or_else(|| {
+            "missing top-level \"mcpServers\" or \"context_servers\" object".to_string()
+        })?;
     let mut names: Vec<&String> = servers.keys().collect();
     names.sort();
     names
         .into_iter()
-        .map(|name| parse_entry(name, &servers[name]))
+        .filter_map(|name| match parse_entry(name, &servers[name]) {
+            Err(error) if error.ends_with(": disabled") => None,
+            result => Some(result),
+        })
         .collect()
 }
 
@@ -49,6 +57,10 @@ fn parse_entry(name: &str, entry: &Value) -> Result<ServerSpec, String> {
     let obj = entry
         .as_object()
         .ok_or_else(|| format!("server {name:?}: entry is not an object"))?;
+    // Zed defaults enabled to true and uses an explicit false to disable.
+    if obj.get("enabled").and_then(Value::as_bool) == Some(false) {
+        return Err(format!("server {name:?}: disabled"));
+    }
     let declared = obj.get("type").and_then(Value::as_str);
     if let Some(command) = obj.get("command").and_then(Value::as_str) {
         if let Some(kind) = declared {
@@ -156,6 +168,32 @@ impl AgentMcp {
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
@@ -164,35 +202,7 @@ pub async fn connect(
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
@@ -257,6 +267,20 @@ mod tests {
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
@@ -276,7 +300,7 @@ mod tests {
     fn rejects_malformed_documents_legibly() {
         assert!(parse_servers_json("not json")
             .unwrap_err()
-            .contains("invalid JSON"));
+            .contains("invalid JSON/JSONC"));
         assert!(parse_servers_json("{}").unwrap_err().contains("mcpServers"));
         assert!(parse_servers_json(r#"{"mcpServers": {"a": {}}}"#)
             .unwrap_err()
diff --git i/src/config.rs w/src/config.rs
index cdd9f90..9561539 100644
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
@@ -1067,7 +1067,7 @@ impl Default for AcpMcpConfig {
             max_servers: 32,
             max_concurrent_connects: 8,
             max_tools_per_server: 128,
-            zed_config_fallback: false,
+            zed_config_fallback: true,
             zed_settings_path: None,
         }
     }
diff --git i/src/zed_config.rs w/src/zed_config.rs
index 438210d..698a3fc 100644
--- i/src/zed_config.rs
+++ w/src/zed_config.rs
@@ -43,7 +43,7 @@ pub fn context_server_specs(override_path: Option<&str>) -> anyhow::Result<Vec<S
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
         Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
     };
-    let json = strip_trailing_commas(&strip_jsonc(&raw));
+    let json = clean_jsonc(&raw);
     let value: serde_json::Value = serde_json::from_str(&json)
         .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
     Ok(specs_from_settings(&value))
@@ -119,6 +119,12 @@ fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
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
1. The worktree contains unrelated in-progress evidence changes; review only the supplied diff.

### Specific Questions for Reviewer
1. Does the precedence exactly satisfy harness-first behavior?
2. Are default changes safe for non-Zed ACP clients?
3. Are JSONC and disabled-entry semantics robust?
4. Is symlink guidance accurate?

<!-- event id="1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Changing the default `zed_config_fallback` from false to true is unsafe for non-Zed ACP clients. The old comment explicitly stated it 'must not fire for non-Zed ACP clients' because it reads an external app's config and spawns that config's stdio servers. With the new default, any non-Zed ACP harness that legitimately forwards an empty MCP server list (e.g. the user configured no servers in that client) will silently have Zed-configured stdio processes spawned into its sessions. This directly answers the request's question 'Are default changes safe for non-Zed ACP clients?' — no. Either keep the default false, or gate the fallback on evidence that the connected client is Zed (e.g. clientInfo from initialize).
B2. Empty forwarded list is overloaded as 'no configuration'. An ACP harness forwarding an empty list may be an intentional statement that the session should have zero MCP servers, but resolve_mcp_specs now treats it as a signal to inject servers from Zed settings and then from `[agent.mcp].servers_file` (also enabled and pointed at Zed settings by default). Combined, the default configuration means every empty-forward ACP session on a machine with Zed installed gets Zed's context servers bridged twice-removed from user intent. This is not 'harness-first' — harness-first would require the harness's empty list to win unless the user opts in. At minimum, the `[agent.mcp].servers_file` fallback for the ACP path should be a separate opt-in flag rather than reusing `agent.mcp.enabled`, which controls a different feature (non-ACP frontends).
B3. Disabled-entry semantics use a fragile error-string sentinel: parse_entry returns Err("server {name:?}: disabled") and parse_servers_json filters entries whose error 'ends_with(": disabled")'. Any future error message ending in ': disabled' (or a server name interacting with Debug-quoting/escapes in unexpected ways) would be silently dropped instead of surfaced. Use a typed result (e.g. Result<Option<ServerSpec>, String> or a small enum) so 'disabled' is a first-class outcome, not a string match on an error.
B4. Semantics divergence between the two Zed-settings code paths: the `zed_config_fallback` path uses zed_config::specs_from_settings, while the `[agent.mcp].servers_file` path pointed at the same settings.json goes through parse_servers_json/parse_entry. These parsers differ (e.g. `type` validation, disabled handling, and Zed-specific entry shapes such as extension-provided `settings`-only entries or `source` fields). The same file can yield different server sets depending on which fallback tier consumed it, which is confusing and hard to debug. Unify on one Zed-settings parser or add tests proving equivalent output for representative Zed configs.
B5. The diff includes an unrelated hunk in src/acp_cmd.rs tests adding `evidence: Default::default()` to a turn-outcome struct. The known_concerns note says unrelated in-progress evidence changes exist in the worktree; this hunk appears to be part of that unrelated work leaking into the reviewed diff (and implies a struct change not shown here, so the diff may not compile standalone). Remove it from this change or include the corresponding struct change explicitly.

### Non-Blocking Suggestions
S1. Update the stale inline comment at the resolve_mcp_specs call site in build_agent_with_state ('Falls back to Zed's settings when the forwarded list is empty (unpatched-Zed cold-start race)') to describe the new three-tier precedence, matching the new function doc.
S2. docs/zed-acp-setup.md symlink guidance is directionally correct but could be tightened: also warn that pointing `[agent.mcp].servers_file` at Zed's full settings.json means unrelated Zed settings edits (or transient JSONC syntax errors while Zed is mid-edit) will invalidate MCP loading for Daimonos; the warn-and-continue behavior in load_configured_specs makes that failure silent apart from a log line.
S3. load_configured_specs logs and returns an empty Vec on unreadable/invalid files. On the ACP path this silently yields a session with no MCP tools; consider surfacing a session-visible notice or at least a distinct event name from the non-ACP path so the two consumers are distinguishable in telemetry.
S4. Add a test covering precedence tier 3: empty forward + zed_config_fallback yielding empty/failed Zed settings + a populated `[agent.mcp].servers_file` — the current updated test only exercises tiers 1 and 2 and the all-disabled case.
S5. Consider honoring disabled semantics for Claude/Cursor-style files consistently and documenting it: the new `enabled: false` check in parse_entry now also applies to `mcpServers` documents, which is a behavior change for existing users of that format; note it in the config comment/docs.

**Verdict:** REVISE

<!-- event id="2" artifact path="mcp-shared-config-review-mcp-shared-config/artifacts/round-1-rebuttal.diff" sha256="026cf2c124acc99a442fb8788dde540d49bb9901c9300be975ee0e09932ddbe0" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — The reviewer is correct that a non-Zed client can intentionally send an empty list. The revision records client_info during ACP initialize and permits file recovery only when the client name/title identifies Zed.
Re B2: ACCEPT — The empty-list ambiguity is real for non-Zed clients. The revision makes their empty list authoritative. I do not add a separate ACP fallback flag because the existing zed_config_fallback remains the explicit ACP opt-in/gate and tier 3 is reachable only inside that identified-Zed recovery flow; agent.mcp.enabled then controls whether the configured shared file participates.
Re B3: ACCEPT — String matching was fragile. The revision removes it entirely: Zed context_servers are routed through zed_config::specs_from_settings, where disabled entries are represented directly as Option::None; Claude/Cursor mcpServers retain strict Result parsing.
Re B4: ACCEPT — The same Zed settings file must have identical semantics at both tiers. parse_servers_json now delegates context_servers to zed_config::specs_from_settings, while mcpServers continues through the strict Claude/Cursor parser.
Re B5: CLARIFY — The evidence field is pre-existing unrelated worktree state, not a change made for this task. Rubber-ducky reviewed raw git diff against HEAD, which necessarily captured it because src/acp_cmd.rs is shared with concurrent work. The submitted revised artifact still contains the complete actual working-tree diff as required, but this task neither owns nor reverts that hunk; isolated validation patches the separately incomplete evidence work only outside the worktree.

### Revised Code / Diff
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
index 2cbfa8f..f221412 100644
--- i/docs/zed-acp-setup.md
+++ w/docs/zed-acp-setup.md
@@ -13,6 +13,35 @@ used together.
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
+The selected file may contain either `mcpServers` or `context_servers`. A
+harness that cannot select a path may symlink its standalone MCP file to the
+shared file. Do not symlink all of Zed's `settings.json` over a harness file
+that only accepts a standalone `mcpServers` document. While Zed's file is being
+edited, invalid JSONC causes Daimonos to skip configured MCP servers and emit a
+warning rather than failing the agent session.
+
 ## Setup
 
 Add daimonos under the `agent_servers` key in Zed's `settings.json`. The
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index 6737377..e058280 100644
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
@@ -2610,6 +2606,13 @@ fn build_agent_with_state(
                         state
                             .supports_terminal_output
                             .store(client_supports_terminal_output(&req), Ordering::Release);
+                        let is_zed = req.client_info.as_ref().is_some_and(|info| {
+                            info.name.eq_ignore_ascii_case("zed")
+                                || info.title.as_deref().is_some_and(|title| {
+                                    title.to_ascii_lowercase().contains("zed")
+                                })
+                        });
+                        state.client_is_zed.store(is_zed, Ordering::Release);
                         // load_session(true): Zed calls session/load to reopen
                         // a thread on window refocus.
                         let mut capabilities = AgentCapabilities::new()
@@ -2876,11 +2879,15 @@ fn build_agent_with_state(
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
@@ -3000,9 +3007,14 @@ fn build_agent_with_state(
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
@@ -4026,8 +4038,8 @@ mod tests {
         );
     }
 
-    #[test]
-    fn resolve_mcp_specs_fallback_gating() {
+    #[tokio::test]
+    async fn resolve_mcp_specs_fallback_gating() {
         let dir = tempfile::tempdir().unwrap();
         let path = dir.path().join("settings.json");
         std::fs::write(
@@ -4038,23 +4050,44 @@ mod tests {
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
@@ -7720,6 +7753,7 @@ mod tests {
             stop_reason,
             error_message: None,
             context_overflow: false,
+            evidence: Default::default(),
         };
         assert_eq!(
             canonical_assistant_outcome(&turn(crate::providers::StopReason::EndTurn)),
diff --git i/src/agent_mcp.rs w/src/agent_mcp.rs
index 850e293..b60f121 100644
--- i/src/agent_mcp.rs
+++ w/src/agent_mcp.rs
@@ -24,19 +24,26 @@ use crate::config::Config;
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
+/// maintaining a translated copy. Entries are sorted by name so bridge order
+/// (and thus the `max_servers` cap) is deterministic.
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
@@ -156,6 +163,32 @@ impl AgentMcp {
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
@@ -164,35 +197,7 @@ pub async fn connect(
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
@@ -257,6 +262,20 @@ mod tests {
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
@@ -276,7 +295,7 @@ mod tests {
     fn rejects_malformed_documents_legibly() {
         assert!(parse_servers_json("not json")
             .unwrap_err()
-            .contains("invalid JSON"));
+            .contains("invalid JSON/JSONC"));
         assert!(parse_servers_json("{}").unwrap_err().contains("mcpServers"));
         assert!(parse_servers_json(r#"{"mcpServers": {"a": {}}}"#)
             .unwrap_err()
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
diff --git i/src/zed_config.rs w/src/zed_config.rs
index 438210d..967903e 100644
--- i/src/zed_config.rs
+++ w/src/zed_config.rs
@@ -43,7 +43,7 @@ pub fn context_server_specs(override_path: Option<&str>) -> anyhow::Result<Vec<S
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
         Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
     };
-    let json = strip_trailing_commas(&strip_jsonc(&raw));
+    let json = clean_jsonc(&raw);
     let value: serde_json::Value = serde_json::from_str(&json)
         .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
     Ok(specs_from_settings(&value))
@@ -61,7 +61,7 @@ fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
 
 /// Map the `context_servers` object of a parsed Zed settings document to
 /// [`ServerSpec`]s. Unknown/incomplete entries are skipped.
-fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
+pub(crate) fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
     let Some(servers) = value.get("context_servers").and_then(|v| v.as_object()) else {
         return Vec::new();
     };
@@ -119,6 +119,12 @@ fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
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
R1. Validation: isolated cargo check passes; agent_mcp tests 5/5, zed_config tests 4/4, and ACP precedence test 1/1 pass. The precedence test now covers identified-Zed fallback, non-Zed empty authority, both fallbacks disabled, tier 3 shared-file recovery, and non-empty forwarded authority.
R2. The revision also accepts suggestions S1, S2, and S4 by updating call-site comments, documenting transient invalid-JSONC behavior, and testing tier 3. S3 is left non-blocking because existing fail-open logging is deliberate bridge behavior; S5 no longer applies because enabled handling is confined to Zed parsing.

**Requesting:** RE-REVIEW

<!-- event id="3" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** PARTIAL

### Blocking Concerns
B6. B5 remains unresolved. The revised diff still contains the unrelated `evidence: Default::default()` hunk in src/acp_cmd.rs tests, and the CLARIFY disposition confirms it belongs to separate in-progress evidence work whose corresponding struct change is not in this diff. As submitted, the reviewed artifact is not self-consistent: it references a struct field introduced by changes outside this diff, so the diff does not stand alone and the change under review is entangled with concurrent work. The worker states isolated validation 'patches the separately incomplete evidence work only outside the worktree', which means the validation evidence in R1 was gathered against a tree that differs from the submitted diff. Please either (a) rebase/stage this change onto a base without the evidence work so the submitted diff excludes the foreign hunk, or (b) explicitly include the minimal struct change so the diff compiles standalone, or (c) have the coordinator formally scope that hunk out of this review with a base-commit reference. I cannot approve a diff whose compilability depends on unstated external state.

### Non-Blocking Suggestions
S6. The Zed-client identification checks `info.title` for a case-insensitive substring 'zed'. Substring matching on title is loose — a hypothetical client titled e.g. 'Blazed IDE' would match and receive file-based recovery. Consider matching only on `info.name == "zed"` (exact, case-insensitive) or an allowlist, and drop the title-substring heuristic unless there is a known Zed build that omits/renames `name`.
S7. In parse_servers_json, a document containing both `context_servers` and `mcpServers` now silently prefers `context_servers` and ignores `mcpServers`. Consider logging or documenting this precedence (a one-line note in the function doc and the config comment) so users merging formats into one shared file aren't surprised.

### Responses to Prior Points
Re R1: Acknowledged. The reported isolated cargo check and passing test counts (agent_mcp 5/5, zed_config 4/4, ACP precedence 1/1) are consistent with the revised diff, and the expanded resolve_mcp_specs_fallback_gating test now exercises all five precedence scenarios I asked about, including the tier-3 shared-file recovery path (satisfying S4). I cannot independently execute tests, so I accept this as reported validation evidence.
Re R2: Confirmed against the revised diff: the call-site comments were rewritten to describe harness-first, Zed-gated recovery (S1); docs/zed-acp-setup.md now warns about transient invalid JSONC while Zed edits its settings file (S2); and the tier-3 test was added (S4). I accept leaving S3 as-is — fail-open with a warning is a defensible, pre-existing bridge behavior. S5 is indeed mooted since disabled handling now lives only in the Zed parser via specs_from_settings; mcpServers documents retain their prior strict semantics.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** REVISE

<!-- event id="4" artifact path="mcp-shared-config-review-mcp-shared-config/artifacts/round-2-rebuttal.diff" sha256="aabc15671e3d35091cb00db8ec4c6c147edf750a194312baf3db2ee4883461b9" -->
## Rebuttal — Round 2
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B5: ACCEPT — I accept that the review artifact should exclude the foreign evidence hunk even though it exists in the shared worktree. I created a clean HEAD worktree, copied only the six owned task files, removed that one foreign line from the scoped artifact, and generated the revised diff there.
Re B6: ACCEPT — The revised artifact is now a standalone diff against HEAD with no evidence hunk. It applies and cargo check passes without patching unrelated code. Targeted tests pass: agent_mcp 5/5, zed_config 4/4, ACP precedence 1/1.

### Revised Code / Diff
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
index 2cbfa8f..c7ef451 100644
--- i/docs/zed-acp-setup.md
+++ w/docs/zed-acp-setup.md
@@ -13,6 +13,35 @@ used together.
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
diff --git i/src/zed_config.rs w/src/zed_config.rs
index 438210d..967903e 100644
--- i/src/zed_config.rs
+++ w/src/zed_config.rs
@@ -43,7 +43,7 @@ pub fn context_server_specs(override_path: Option<&str>) -> anyhow::Result<Vec<S
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
         Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
     };
-    let json = strip_trailing_commas(&strip_jsonc(&raw));
+    let json = clean_jsonc(&raw);
     let value: serde_json::Value = serde_json::from_str(&json)
         .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
     Ok(specs_from_settings(&value))
@@ -61,7 +61,7 @@ fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
 
 /// Map the `context_servers` object of a parsed Zed settings document to
 /// [`ServerSpec`]s. Unknown/incomplete entries are skipped.
-fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
+pub(crate) fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
     let Some(servers) = value.get("context_servers").and_then(|v| v.as_object()) else {
         return Vec::new();
     };
@@ -119,6 +119,12 @@ fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
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
R3. Accepted S6: Zed detection now uses exact case-insensitive client_info.name == zed only; title substring matching was removed.
R4. Accepted S7: parser docs and user docs now state that context_servers wins when both top-level formats exist.

**Requesting:** RE-REVIEW

<!-- event id="5" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 3
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S8. Minor doc formatting: in docs/zed-acp-setup.md the sentence 'The selected file may contain either `mcpServers` or `context_servers`; if it contains both, `context_servers` takes precedence. A harness that cannot select a path may symlink...' now runs the precedence note and the symlink guidance together on one long line; consider rewrapping for readability. Non-blocking.

### Responses to Prior Points
Re R3: Confirmed in the round-2 revised diff: Zed identification now uses only an exact case-insensitive match on client_info.name == "zed" (is_some_and(|info| info.name.eq_ignore_ascii_case("zed"))); the title-substring heuristic is gone. This satisfies S6 and closes the false-positive risk for clients with 'zed' in their titles.
Re R4: Confirmed: the parse_servers_json doc comment now states 'If both top-level keys exist, `context_servers` takes precedence', and docs/zed-acp-setup.md carries the same note ('if it contains both, `context_servers` takes precedence'). This satisfies S7.
B5: resolved
B6: resolved

**Verdict:** APPROVE
