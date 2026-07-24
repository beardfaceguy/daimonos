//! Read Zed's `context_servers` settings directly, as a self-sufficient
//! fallback for the ACP MCP bridge (ADR-003).
//!
//! Zed forwards the user's configured MCP servers to an external ACP agent
//! only inside `session/new` / `session/load`, built synchronously from its
//! `ContextServerStore`. That store is populated asynchronously from settings,
//! so a session created immediately after a Zed (re)start — e.g. an
//! auto-restored thread — can race ahead of it and receive an EMPTY server
//! list, and Zed never re-forwards to a live session. The result is a daimonos
//! ACP session with zero MCP tools until it is reloaded.
//!
//! To stay robust on *unpatched* Zed, when the forwarded list is empty the ACP
//! frontend falls back to this module: it reads Zed's own `settings.json`,
//! extracts `context_servers`, and maps each enabled entry to a
//! [`ServerSpec`] using the same stdio/http rules Zed itself uses
//! (`mcp_servers_for_project`). The bridge then connects them exactly as if
//! Zed had forwarded them.
//!
//! Limitations (documented in ADR-003): servers that rely on Zed's OAuth /
//! keychain sessions (no usable token in the settings file, e.g. an
//! OAuth-only HTTP server) cannot be recovered here — only what the settings
//! file itself carries (stdio command/env, inline HTTP headers). Project-local
//! `.zed/settings.json` overrides are not merged in this version.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::mcp_bridge::ServerSpec;

/// Resolve MCP [`ServerSpec`]s from Zed's `settings.json` `context_servers`.
///
/// `override_path` wins when set; otherwise the standard Linux path
/// (`$XDG_CONFIG_HOME/zed/settings.json`, falling back to
/// `$HOME/.config/zed/settings.json`) is used. A missing file yields an empty
/// list (not an error); a malformed file is an error the caller logs and
/// treats as "no fallback available".
pub fn context_server_specs(override_path: Option<&str>) -> anyhow::Result<Vec<ServerSpec>> {
    let Some(path) = settings_path(override_path) else {
        return Ok(Vec::new());
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    let json = strip_trailing_commas(&strip_jsonc(&raw));
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    Ok(specs_from_settings(&value))
}

fn settings_path(override_path: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("zed").join("settings.json"))
}

/// Map the `context_servers` object of a parsed Zed settings document to
/// [`ServerSpec`]s. Unknown/incomplete entries are skipped.
fn specs_from_settings(value: &serde_json::Value) -> Vec<ServerSpec> {
    let Some(servers) = value.get("context_servers").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, entry)| spec_from_entry(name, entry))
        .collect()
}

fn spec_from_entry(name: &str, entry: &serde_json::Value) -> Option<ServerSpec> {
    let obj = entry.as_object()?;
    // `enabled` defaults to true in Zed; only an explicit `false` disables.
    if obj.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
        return None;
    }
    // Stdio takes precedence (a `command` marks a stdio server), matching
    // Zed's own precedence in `mcp_servers_for_project`.
    if let Some(command) = obj.get("command").and_then(|v| v.as_str()) {
        let args = obj
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        return Some(ServerSpec::Stdio {
            name: name.to_string(),
            command: command.to_string(),
            args,
            env: string_map(obj.get("env")),
        });
    }
    if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
        return Some(ServerSpec::Http {
            name: name.to_string(),
            url: url.to_string(),
            headers: string_map(obj.get("headers")),
        });
    }
    None
}

/// Collect a JSON object of string values into a map, dropping non-string
/// values.
fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Remove `//` line comments and `/* */` block comments from a JSONC document,
/// preserving anything inside string literals (so `"http://…"` and comment-like
/// content in strings survive).
fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    while i < n {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < n {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            i += 2;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2; // consume the closing */
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Drop a `,` when the next non-whitespace character is `}` or `]` (trailing
/// commas, which JSON rejects but Zed's JSONC allows). String-aware.
fn strip_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    while i < n {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < n {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            if j < n && (chars[j] == '}' || chars[j] == ']') {
                i += 1; // skip the trailing comma
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_stdio_http_disabled_and_trailing_commas() {
        let settings = r#"{
            // top-level comment
            "theme": "dark",
            "context_servers": {
                "codejung": {
                    "enabled": true,
                    "command": "python3",
                    "args": ["/path/to/server.py"], // inline comment
                    "env": { "TOKEN": "abc123" },
                },
                "agent-mail": {
                    "url": "http://127.0.0.1:8765/mcp/", // not a comment: in string
                },
                "linear": {
                    "url": "https://mcp.linear.app/mcp",
                    "headers": { "Authorization": "Bearer lin_api_xyz" },
                },
                "disabled-one": {
                    "enabled": false,
                    "command": "should-not-appear",
                },
                "no-transport": { "enabled": true },
            },
        }"#;
        let value: serde_json::Value =
            serde_json::from_str(&strip_trailing_commas(&strip_jsonc(settings)))
                .expect("JSONC should parse after stripping");
        let mut specs = specs_from_settings(&value);
        specs.sort_by(|a, b| server_name(a).cmp(server_name(b)));

        // disabled-one and no-transport are dropped.
        assert_eq!(specs.len(), 3, "got: {specs:?}");

        let agent_mail = specs
            .iter()
            .find(|s| server_name(s) == "agent-mail")
            .unwrap();
        assert!(
            matches!(agent_mail, ServerSpec::Http { url, .. } if url == "http://127.0.0.1:8765/mcp/")
        );

        let codejung = specs.iter().find(|s| server_name(s) == "codejung").unwrap();
        match codejung {
            ServerSpec::Stdio {
                command, args, env, ..
            } => {
                assert_eq!(command, "python3");
                assert_eq!(args, &vec!["/path/to/server.py".to_string()]);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc123"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }

        let linear = specs.iter().find(|s| server_name(s) == "linear").unwrap();
        match linear {
            ServerSpec::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer lin_api_xyz")
                );
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn missing_context_servers_key_yields_empty() {
        let value: serde_json::Value = serde_json::json!({"theme": "dark"});
        assert!(specs_from_settings(&value).is_empty());
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let specs = context_server_specs(Some("/nonexistent/path/zed/settings.json")).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn url_with_double_slash_survives_comment_stripping() {
        // The `//` in a URL string must not be treated as a comment.
        let stripped = strip_jsonc(r#"{"url": "https://example.com/mcp"}"#);
        assert!(
            stripped.contains("https://example.com/mcp"),
            "got: {stripped}"
        );
    }

    fn server_name(spec: &ServerSpec) -> &str {
        match spec {
            ServerSpec::Stdio { name, .. } | ServerSpec::Http { name, .. } => name,
        }
    }
}
