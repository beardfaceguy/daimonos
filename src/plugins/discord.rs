use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::config::DiscordConfig;
use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

struct ApiFetchResult {
    body: serde_json::Value,
    rate_limit_retries: u32,
}

pub struct DiscordPlugin {
    descriptor: ToolDescriptor,
    cfg: DiscordConfig,
    client: reqwest::Client,
}

impl DiscordPlugin {
    pub fn new(cfg: DiscordConfig) -> Self {
        let mut commands = HashMap::new();
        for name in [
            "list_guilds",
            "list_channels",
            "read_messages",
            "search_messages",
        ] {
            commands.insert(
                name.to_string(),
                ToolCommand {
                    bin: "discord".into(),
                    args: vec![name.into()],
                    output: "structured".into(),
                },
            );
        }
        Self {
            descriptor: ToolDescriptor {
                id: "discord".into(),
                commands,
                source_pattern: None,
                manifest: None,
                diagnostics_format: "none".into(),
                supports_quickfix: false,
                quickfix_format: None,
            },
            cfg,
            client: reqwest::Client::new(),
        }
    }

    fn build_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.cfg.api_base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    async fn get_json(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<ApiFetchResult, String> {
        let token = self.cfg.resolve_bot_token()?;
        let auth = format!("Bot {token}");
        let mut retries_used: u32 = 0;

        loop {
            let mut req = self
                .client
                .get(self.build_url(path))
                .header(reqwest::header::AUTHORIZATION, auth.clone());
            for (k, v) in query {
                req = req.query(&[(k, v)]);
            }

            let resp = req.send().await.map_err(|e| {
                self.cfg
                    .redact_sensitive(&format!("discord request failed for {path}: {e}"))
            })?;
            let status = resp.status();
            let retry_after_header = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_secs);
            let text = resp.text().await.map_err(|e| {
                self.cfg
                    .redact_sensitive(&format!("discord response read failed for {path}: {e}"))
            })?;

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                && (retries_used as usize) < self.cfg.rate_limit_max_retries
            {
                let retry_after_body = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()));
                let secs = retry_after_header
                    .or(retry_after_body)
                    .unwrap_or(0.5)
                    .max(0.0);
                let sleep_ms = ((secs * 1000.0) as u64)
                    .max(1)
                    .min(self.cfg.rate_limit_max_sleep_ms);
                retries_used += 1;
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                continue;
            }

            if !status.is_success() {
                let msg = self.cfg.redact_sensitive(&text);
                return Err(format!("discord api {path} returned {status}: {msg}"));
            }
            let body = serde_json::from_str(&text)
                .map_err(|e| format!("discord api {path} invalid json: {e}"))?;
            return Ok(ApiFetchResult {
                body,
                rate_limit_retries: retries_used,
            });
        }
    }

    fn require_allowed_guild(&self, guild_id: &str) -> Result<(), String> {
        if self.cfg.is_guild_allowed(guild_id) {
            Ok(())
        } else {
            Err(format!("guild '{guild_id}' is not allowlisted"))
        }
    }

    fn require_allowed_channel(&self, channel_id: &str) -> Result<(), String> {
        if self.cfg.is_channel_allowed(channel_id) {
            Ok(())
        } else {
            Err(format!("channel '{channel_id}' is not allowlisted"))
        }
    }
}

#[async_trait::async_trait]
impl ToolPlugin for DiscordPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn run_command_with_config(
        &self,
        command: &str,
        _cwd: &Path,
        _env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
        _process_cfg: &crate::config::ProcessConfig,
    ) -> Result<ToolResult, String> {
        if !self.cfg.enabled {
            return Err("discord integration disabled in config ([discord].enabled=false)".into());
        }

        let output = match command {
            "list_guilds" => list_guilds(self).await?,
            "list_channels" => list_channels(self, args).await?,
            "read_messages" => read_messages(self, args).await?,
            "search_messages" => search_messages(self, args).await?,
            _ => return Err(format!("unknown discord command: {command}")),
        };

        Ok(ToolResult {
            tool: "discord".into(),
            command: command.to_string(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

async fn list_guilds(plugin: &DiscordPlugin) -> Result<serde_json::Value, String> {
    let fetched = plugin.get_json("users/@me/guilds", &[]).await?;
    let guilds = fetched
        .body
        .as_array()
        .ok_or_else(|| "discord users/@me/guilds returned non-array".to_string())?;

    let filtered: Vec<serde_json::Value> = guilds
        .iter()
        .filter_map(|g| {
            let id = g.get("id")?.as_str()?;
            if !plugin.cfg.is_guild_allowed(id) {
                return None;
            }
            Some(json!({
                "id": id,
                "name": g.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "owner": g.get("owner").and_then(|v| v.as_bool()).unwrap_or(false),
                "permissions": g.get("permissions"),
            }))
        })
        .collect();

    Ok(json!({
        "guilds": filtered,
        "count": filtered.len(),
        "allowlist_size": plugin.cfg.allow_guild_ids.len(),
        "observability": {
            "rate_limit_retries": fetched.rate_limit_retries,
            "rate_limited": fetched.rate_limit_retries > 0
        }
    }))
}

async fn list_channels(
    plugin: &DiscordPlugin,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let guild_id = args
        .and_then(|a| a.get("guild_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "list_channels requires guild_id".to_string())?;
    plugin.require_allowed_guild(guild_id)?;

    let fetched = plugin
        .get_json(&format!("guilds/{guild_id}/channels"), &[])
        .await?;
    let channels = fetched
        .body
        .as_array()
        .ok_or_else(|| "discord guild channels returned non-array".to_string())?;

    let filtered: Vec<serde_json::Value> = channels
        .iter()
        .filter_map(|c| {
            let id = c.get("id")?.as_str()?;
            if !plugin.cfg.is_channel_allowed(id) {
                return None;
            }
            Some(json!({
                "id": id,
                "guild_id": guild_id,
                "name": c.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "type": c.get("type").and_then(|v| v.as_i64()).unwrap_or(-1),
                "parent_id": c.get("parent_id"),
            }))
        })
        .collect();

    Ok(json!({
        "channels": filtered,
        "count": filtered.len(),
        "guild_id": guild_id,
        "observability": {
            "rate_limit_retries": fetched.rate_limit_retries,
            "rate_limited": fetched.rate_limit_retries > 0
        }
    }))
}

async fn read_messages(
    plugin: &DiscordPlugin,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let channel_id = args
        .and_then(|a| a.get("channel_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "read_messages requires channel_id".to_string())?;
    plugin.require_allowed_channel(channel_id)?;

    let requested_limit = args
        .and_then(|a| a.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;
    let limit = requested_limit.clamp(1, plugin.cfg.max_messages_per_call);

    let fetched = plugin
        .get_json(
            &format!("channels/{channel_id}/messages"),
            &[("limit", limit.to_string())],
        )
        .await?;
    let messages = fetched
        .body
        .as_array()
        .ok_or_else(|| "discord channel messages returned non-array".to_string())?;

    let mut compact: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| compact_message(m, &plugin.cfg))
        .collect();

    // Enforce global response cap by trimming oldest entries in this payload.
    let mut response = json!({
        "channel_id": channel_id,
        "messages": compact,
        "count": messages.len(),
        "requested_limit": requested_limit,
        "applied_limit": limit,
        "observability": {
            "rate_limit_retries": fetched.rate_limit_retries,
            "rate_limited": fetched.rate_limit_retries > 0
        }
    });
    let mut truncated = false;
    while serde_json::to_string(&response)
        .map(|s| s.len())
        .unwrap_or(0)
        > plugin.cfg.max_response_chars
        && !compact.is_empty()
    {
        compact.pop();
        truncated = true;
        response["messages"] = json!(compact);
        response["count"] = json!(response["messages"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0));
    }
    if truncated {
        response["truncated"] = json!(true);
    }
    Ok(response)
}

async fn search_messages(
    plugin: &DiscordPlugin,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let channel_id = args
        .and_then(|a| a.get("channel_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "search_messages requires channel_id".to_string())?;
    plugin.require_allowed_channel(channel_id)?;

    let query = args
        .and_then(|a| a.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "search_messages requires query".to_string())?
        .trim()
        .to_string();
    if query.is_empty() {
        return Err("search_messages query must not be empty".to_string());
    }

    let requested_limit = args
        .and_then(|a| a.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(plugin.cfg.max_messages_per_call as u64) as usize;
    let limit = requested_limit.clamp(1, plugin.cfg.max_messages_per_call);

    let fetched = plugin
        .get_json(
            &format!("channels/{channel_id}/messages"),
            &[("limit", limit.to_string())],
        )
        .await?;
    let messages = fetched
        .body
        .as_array()
        .ok_or_else(|| "discord channel messages returned non-array".to_string())?;

    let query_lc = query.to_lowercase();
    let mut matches: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| {
            m.get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_lowercase().contains(&query_lc))
                .unwrap_or(false)
        })
        .map(|m| compact_message(m, &plugin.cfg))
        .collect();

    let mut response = json!({
        "channel_id": channel_id,
        "query": query,
        "matches": matches,
        "count": 0,
        "scanned": messages.len(),
        "applied_limit": limit,
        "observability": {
            "rate_limit_retries": fetched.rate_limit_retries,
            "rate_limited": fetched.rate_limit_retries > 0
        }
    });
    response["count"] = json!(response["matches"].as_array().map(|a| a.len()).unwrap_or(0));
    let mut truncated = false;
    while serde_json::to_string(&response)
        .map(|s| s.len())
        .unwrap_or(0)
        > plugin.cfg.max_response_chars
        && !matches.is_empty()
    {
        matches.pop();
        truncated = true;
        response["matches"] = json!(matches);
        response["count"] = json!(response["matches"].as_array().map(|a| a.len()).unwrap_or(0));
    }
    if truncated {
        response["truncated"] = json!(true);
    }
    Ok(response)
}

fn compact_message(m: &serde_json::Value, cfg: &DiscordConfig) -> serde_json::Value {
    let content = m
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| sanitize_content(s, cfg.max_message_chars))
        .unwrap_or_default();
    let attachments = m
        .get("attachments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    json!({
                        "id": a.get("id"),
                        "filename": a.get("filename"),
                        "size": a.get("size"),
                        "content_type": a.get("content_type"),
                        "url": a.get("url"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "id": m.get("id"),
        "channel_id": m.get("channel_id"),
        "timestamp": m.get("timestamp"),
        "author": {
            "id": m.get("author").and_then(|a| a.get("id")).cloned().unwrap_or(serde_json::Value::Null),
            "username": m.get("author").and_then(|a| a.get("username")).cloned().unwrap_or(serde_json::Value::Null),
        },
        "content": content,
        "attachments": attachments,
    })
}

fn sanitize_content(input: &str, max_chars: usize) -> String {
    let mut out = input
        .replace("@everyone", "@ everyone")
        .replace("@here", "@ here")
        .replace("<@", "< @")
        .replace("<#", "< #");
    out = out.replace('\r', "");
    truncate_chars(&out, max_chars)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

fn parse_retry_after_secs(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_discord_server(body: serde_json::Value) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            let body_str = serde_json::to_string(&body).unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_str.len(),
                body_str
            );
            socket.write_all(resp.as_bytes()).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn spawn_mock_discord_rate_limit_then_success_server(
        success_body: serde_json::Value,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // First request: 429 with retry_after.
            let (mut socket1, _) = listener.accept().await.unwrap();
            let mut buf1 = vec![0u8; 4096];
            let _ = socket1.read(&mut buf1).await.unwrap();
            let body1 = r#"{"retry_after":0}"#;
            let resp1 = format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body1.len(),
                body1
            );
            socket1.write_all(resp1.as_bytes()).await.unwrap();

            // Second request: success.
            let (mut socket2, _) = listener.accept().await.unwrap();
            let mut buf2 = vec![0u8; 4096];
            let _ = socket2.read(&mut buf2).await.unwrap();
            let body2 = serde_json::to_string(&success_body).unwrap();
            let resp2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body2.len(),
                body2
            );
            socket2.write_all(resp2.as_bytes()).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn mk_plugin(cfg: DiscordConfig) -> DiscordPlugin {
        DiscordPlugin::new(cfg)
    }

    #[test]
    fn truncate_chars_respects_unicode_boundary() {
        let s = "hello-🙂-world";
        let out = truncate_chars(s, 7);
        assert_eq!(out.chars().count(), 7);
    }

    #[test]
    fn sanitize_content_neutralizes_mentions() {
        let s = "@everyone ping <@123> <#456> @here";
        let out = sanitize_content(s, 200);
        assert!(!out.contains("@everyone"));
        assert!(!out.contains("@here"));
        assert!(!out.contains("<@"));
        assert!(!out.contains("<#"));
        assert!(out.contains("@ everyone"));
        assert!(out.contains("@ here"));
        assert!(out.contains("< @123>"));
        assert!(out.contains("< #456>"));
    }

    #[test]
    fn parse_retry_after_parses_float_seconds() {
        assert_eq!(parse_retry_after_secs("0.75"), Some(0.75));
        assert_eq!(parse_retry_after_secs("invalid"), None);
    }

    #[tokio::test]
    async fn read_messages_rejects_non_allowlisted_channel() {
        let mut cfg = DiscordConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.bot_token_env_var = "DAIMONOS_TEST_DISCORD_TOKEN_REJECT_CHANNEL".to_string();
        cfg.allow_channel_ids = vec!["123456789012345678".to_string()];
        std::env::set_var(&cfg.bot_token_env_var, "test-token");
        let plugin = mk_plugin(cfg);
        let err = plugin
            .run_command(
                "read_messages",
                Path::new("."),
                &HashMap::new(),
                None,
                Some(&json!({"channel_id": "999999999999999999"})),
            )
            .await
            .unwrap_err();
        std::env::remove_var("DAIMONOS_TEST_DISCORD_TOKEN_REJECT_CHANNEL");
        assert!(err.contains("not allowlisted"));
    }

    #[tokio::test]
    async fn list_guilds_filters_to_allowlist() {
        let server = spawn_mock_discord_server(json!([
            {"id": "123456789012345678", "name": "Allowed"},
            {"id": "223456789012345678", "name": "Blocked"}
        ]))
        .await;
        let mut cfg = DiscordConfig {
            enabled: true,
            api_base_url: server,
            ..Default::default()
        };
        cfg.bot_token_env_var = "DAIMONOS_TEST_DISCORD_TOKEN_LIST_GUILDS".to_string();
        cfg.allow_guild_ids = vec!["123456789012345678".to_string()];
        std::env::set_var(&cfg.bot_token_env_var, "test-token");

        let plugin = Arc::new(mk_plugin(cfg));
        let out = plugin
            .run_command("list_guilds", Path::new("."), &HashMap::new(), None, None)
            .await
            .unwrap()
            .output;
        std::env::remove_var("DAIMONOS_TEST_DISCORD_TOKEN_LIST_GUILDS");

        assert_eq!(out["count"], 1);
        assert_eq!(out["guilds"][0]["name"], "Allowed");
    }

    #[tokio::test]
    async fn read_messages_applies_message_and_response_caps() {
        let server = spawn_mock_discord_server(json!([
            {
                "id": "m1",
                "channel_id": "123456789012345678",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "author": {"id": "u1", "username": "alice"},
                "content": "@everyone abcdefghijabcdefghij",
                "attachments": []
            },
            {
                "id": "m2",
                "channel_id": "123456789012345678",
                "timestamp": "2026-01-01T00:00:01.000Z",
                "author": {"id": "u2", "username": "bob"},
                "content": "klmnopqrstklmnopqrstklmnopqrst",
                "attachments": []
            }
        ]))
        .await;
        let mut cfg = DiscordConfig {
            enabled: true,
            api_base_url: server,
            max_message_chars: 10,
            max_response_chars: 260,
            ..Default::default()
        };
        cfg.bot_token_env_var = "DAIMONOS_TEST_DISCORD_TOKEN_READ_MESSAGES".to_string();
        cfg.allow_channel_ids = vec!["123456789012345678".to_string()];
        std::env::set_var(&cfg.bot_token_env_var, "test-token");
        let plugin = mk_plugin(cfg);

        let out = plugin
            .run_command(
                "read_messages",
                Path::new("."),
                &HashMap::new(),
                None,
                Some(&json!({"channel_id": "123456789012345678", "limit": 2})),
            )
            .await
            .unwrap()
            .output;
        std::env::remove_var("DAIMONOS_TEST_DISCORD_TOKEN_READ_MESSAGES");

        assert_eq!(out["applied_limit"], 2);
        let msgs = out["messages"].as_array().unwrap();
        assert!(msgs.len() <= 2);
        if let Some(first) = msgs.first() {
            let first_content = first["content"].as_str().unwrap();
            assert_eq!(first_content.chars().count(), 10);
            assert!(!first_content.contains("@everyone"));
        }
        assert!(out.get("observability").is_some());
        if out["messages"].as_array().unwrap().len() < 2 {
            assert_eq!(out["truncated"], true);
        }
    }

    #[tokio::test]
    async fn search_messages_filters_and_sanitizes_content() {
        let server = spawn_mock_discord_server(json!([
            {
                "id": "m1",
                "channel_id": "123456789012345678",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "author": {"id": "u1", "username": "alice"},
                "content": "@everyone deploy to prod",
                "attachments": []
            },
            {
                "id": "m2",
                "channel_id": "123456789012345678",
                "timestamp": "2026-01-01T00:00:01.000Z",
                "author": {"id": "u2", "username": "bob"},
                "content": "casual chat only",
                "attachments": []
            }
        ]))
        .await;
        let mut cfg = DiscordConfig {
            enabled: true,
            api_base_url: server,
            max_message_chars: 128,
            max_response_chars: 2000,
            ..Default::default()
        };
        cfg.bot_token_env_var = "DAIMONOS_TEST_DISCORD_TOKEN_SEARCH_MESSAGES".to_string();
        cfg.allow_channel_ids = vec!["123456789012345678".to_string()];
        std::env::set_var(&cfg.bot_token_env_var, "test-token");
        let plugin = mk_plugin(cfg);

        let out = plugin
            .run_command(
                "search_messages",
                Path::new("."),
                &HashMap::new(),
                None,
                Some(&json!({
                    "channel_id": "123456789012345678",
                    "query": "DEPLOY",
                    "limit": 20
                })),
            )
            .await
            .unwrap()
            .output;
        std::env::remove_var("DAIMONOS_TEST_DISCORD_TOKEN_SEARCH_MESSAGES");

        assert_eq!(out["count"], 1);
        assert_eq!(out["scanned"], 2);
        assert_eq!(out["applied_limit"], 20);
        let content = out["matches"][0]["content"].as_str().unwrap();
        assert!(!content.contains("@everyone"));
        assert!(content.contains("@ everyone"));
        assert!(out.get("observability").is_some());
    }

    #[tokio::test]
    async fn read_messages_retries_on_rate_limit_and_reports_observability() {
        let server = spawn_mock_discord_rate_limit_then_success_server(json!([
            {
                "id": "m1",
                "channel_id": "123456789012345678",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "author": {"id": "u1", "username": "alice"},
                "content": "hello",
                "attachments": []
            }
        ]))
        .await;
        let mut cfg = DiscordConfig {
            enabled: true,
            api_base_url: server,
            rate_limit_max_retries: 2,
            rate_limit_max_sleep_ms: 10,
            ..Default::default()
        };
        cfg.bot_token_env_var = "DAIMONOS_TEST_DISCORD_TOKEN_RATE_LIMIT".to_string();
        cfg.allow_channel_ids = vec!["123456789012345678".to_string()];
        std::env::set_var(&cfg.bot_token_env_var, "test-token");
        let plugin = mk_plugin(cfg);

        let out = plugin
            .run_command(
                "read_messages",
                Path::new("."),
                &HashMap::new(),
                None,
                Some(&json!({"channel_id": "123456789012345678", "limit": 1})),
            )
            .await
            .unwrap()
            .output;
        std::env::remove_var("DAIMONOS_TEST_DISCORD_TOKEN_RATE_LIMIT");

        assert_eq!(out["count"], 1);
        assert_eq!(out["observability"]["rate_limited"], true);
        assert_eq!(out["observability"]["rate_limit_retries"], 1);
    }
}
