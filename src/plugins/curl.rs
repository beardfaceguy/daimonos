use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

pub fn is_available() -> bool {
    std::process::Command::new("curl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct CurlPlugin {
    descriptor: ToolDescriptor,
}

impl CurlPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        commands.insert(
            "request".to_string(),
            ToolCommand {
                bin: "curl".into(),
                args: vec![],
                output: "structured".into(),
            },
        );
        Self {
            descriptor: ToolDescriptor {
                id: "curl".into(),
                commands,
                source_pattern: None,
                manifest: None,
                diagnostics_format: "none".into(),
                supports_quickfix: false,
                quickfix_format: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl ToolPlugin for CurlPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        _env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&Value>,
    ) -> Result<ToolResult, String> {
        match command {
            "request" => {
                let output = curl_request(cwd, args).await?;
                Ok(ToolResult {
                    tool: "curl".into(),
                    command: "request".into(),
                    exit_code: 0,
                    output,
                    stderr: String::new(),
                })
            }
            _ => Err(format!("unknown curl command: {command}")),
        }
    }
}

const METRICS_SENTINEL: &str = "__CURL_METRICS__:";
const MAX_BODY_BYTES: usize = 16 * 1024;

async fn curl_request(cwd: &Path, args: Option<&Value>) -> Result<Value, String> {
    let args = args
        .and_then(|v| v.as_object())
        .ok_or("curl: args must be a JSON object")?;

    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("curl: 'url' is required")?;
    let method = args
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_uppercase();
    let timeout = args.get("timeout").and_then(|v| v.as_i64()).unwrap_or(10);

    let mut cmd_args: Vec<String> = vec![
        "-s".into(),
        "-i".into(),
        "--max-time".into(),
        timeout.to_string(),
        "-X".into(),
        method.clone(),
    ];

    if let Some(headers) = args.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in headers {
            if let Some(v_str) = v.as_str() {
                cmd_args.push("-H".into());
                cmd_args.push(format!("{k}: {v_str}"));
            }
        }
    }

    if let Some(body) = args.get("body").and_then(|v| v.as_str()) {
        cmd_args.push("-d".into());
        cmd_args.push(body.to_string());
    }

    cmd_args.push("-w".into());
    cmd_args.push(format!(
        "\n{METRICS_SENTINEL}%{{http_code}}:%{{time_total}}"
    ));

    cmd_args.push(url.to_string());

    let output = Command::new("curl")
        .args(&cmd_args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("curl exec: {e}"))?;

    let raw = String::from_utf8_lossy(&output.stdout).to_string();

    // Non-zero exit means a transport error (connection refused, timeout, DNS, etc.)
    // The sentinel may still be present with status=0 — ignore it and return the error.
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Ok(json!({
            "error": stderr.trim(),
            "exit": output.status.code().unwrap_or(-1),
        }));
    }

    let sentinel_needle = format!("\n{METRICS_SENTINEL}");
    let Some(sentinel_pos) = raw.rfind(&sentinel_needle) else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Ok(json!({
            "error": stderr.trim(),
            "exit": output.status.code().unwrap_or(-1),
        }));
    };

    let content = &raw[..sentinel_pos];
    let metrics_str = raw[sentinel_pos + sentinel_needle.len()..].trim_end();

    let (status_code, timing_ms) = parse_metrics(metrics_str);
    let (headers_raw, body_raw) = split_headers_body(content);
    let headers = parse_headers(headers_raw);

    let body = if body_raw.len() > MAX_BODY_BYTES {
        format!(
            "{}...[{} bytes truncated]",
            &body_raw[..MAX_BODY_BYTES],
            body_raw.len() - MAX_BODY_BYTES
        )
    } else {
        body_raw.to_string()
    };

    Ok(json!({
        "status": status_code,
        "headers": headers,
        "body": body,
        "timing_ms": timing_ms,
        "url": url,
        "method": method,
    }))
}

fn parse_metrics(s: &str) -> (i64, f64) {
    // "200:0.123456"
    let mut parts = s.trim().splitn(2, ':');
    let status = parts
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let time_s = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    (status, (time_s * 1000.0).round())
}

fn split_headers_body(s: &str) -> (&str, &str) {
    if let Some(pos) = s.find("\r\n\r\n") {
        (&s[..pos], &s[pos + 4..])
    } else if let Some(pos) = s.find("\n\n") {
        (&s[..pos], &s[pos + 2..])
    } else {
        (s, "")
    }
}

fn parse_headers(raw: &str) -> Value {
    let mut map = serde_json::Map::new();
    let mut lines = raw.lines();
    lines.next(); // skip status line (e.g. "HTTP/1.1 200 OK")
    for line in lines {
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_lowercase();
            let value = line[colon_pos + 1..].trim().to_string();
            map.insert(key, json!(value));
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    // --- Pure parsing tests ---

    #[test]
    fn parse_metrics_extracts_status_and_time() {
        let (status, timing_ms) = parse_metrics("200:0.123456");
        assert_eq!(status, 200);
        assert!(
            (timing_ms - 123.0).abs() < 1.0,
            "expected ~123ms, got {timing_ms}"
        );
    }

    #[test]
    fn parse_metrics_zero_on_empty() {
        let (status, timing_ms) = parse_metrics("");
        assert_eq!(status, 0);
        assert_eq!(timing_ms, 0.0);
    }

    #[test]
    fn parse_metrics_404() {
        let (status, _) = parse_metrics("404:0.050000");
        assert_eq!(status, 404);
    }

    #[test]
    fn split_headers_body_crlf() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nhello world";
        let (headers, body) = split_headers_body(raw);
        assert!(headers.contains("Content-Type"));
        assert_eq!(body, "hello world");
    }

    #[test]
    fn split_headers_body_lf() {
        let raw = "HTTP/1.1 200 OK\nContent-Type: text/plain\n\nhello world";
        let (headers, body) = split_headers_body(raw);
        assert!(headers.contains("Content-Type"));
        assert_eq!(body, "hello world");
    }

    #[test]
    fn split_headers_body_no_blank_line() {
        let raw = "HTTP/1.1 200 OK\nContent-Type: text/plain";
        let (headers, body) = split_headers_body(raw);
        assert_eq!(headers, raw);
        assert_eq!(body, "");
    }

    #[test]
    fn parse_headers_extracts_fields() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Custom: value";
        let headers = parse_headers(raw);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-custom"], "value");
        assert!(
            headers.get("HTTP/1.1 200 OK").is_none(),
            "status line should not appear as header"
        );
    }

    #[test]
    fn curl_is_available() {
        assert!(is_available(), "curl should be on PATH in this environment");
    }

    // --- Integration test with local HTTP server ---

    #[tokio::test]
    async fn curl_request_returns_structured_json() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
                let _ = stream.write_all(response).await;
            }
        });

        let plugin = CurlPlugin::new();
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"url": format!("http://{addr}"), "method": "GET"});

        let result = plugin
            .run_command("request", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["status"], 200);
        assert_eq!(result.output["body"], "hello");
        assert_eq!(result.output["method"], "GET");
        assert!(result.output["timing_ms"].as_f64().unwrap_or(-1.0) >= 0.0);
        assert!(result.output.get("headers").is_some());
    }

    #[tokio::test]
    async fn curl_request_sends_custom_headers() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let captured: Arc<tokio::sync::Mutex<Vec<u8>>> = Arc::new(tokio::sync::Mutex::new(vec![]));
        let captured_clone = captured.clone();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                *captured_clone.lock().await = buf[..n].to_vec();
                let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response).await;
            }
        });

        let plugin = CurlPlugin::new();
        let dir = tempfile::tempdir().unwrap();
        let args = json!({
            "url": format!("http://{addr}"),
            "headers": {"X-Test-Header": "test-value"}
        });

        let result = plugin
            .run_command("request", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["status"], 200);
        let request_bytes = captured.lock().await.clone();
        let request_str = String::from_utf8_lossy(&request_bytes);
        assert!(
            request_str.contains("X-Test-Header: test-value"),
            "custom header should be forwarded; got: {request_str}"
        );
    }

    #[tokio::test]
    async fn curl_request_error_on_unreachable() {
        let plugin = CurlPlugin::new();
        let dir = tempfile::tempdir().unwrap();
        // Port 1 is almost guaranteed to be unreachable
        let args = json!({"url": "http://127.0.0.1:1", "timeout": 2});

        let result = plugin
            .run_command("request", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        // Should return an error object, not crash
        assert!(
            result.output.get("error").is_some(),
            "unreachable host should produce an error field; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn curl_request_post_with_body() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let captured: Arc<tokio::sync::Mutex<Vec<u8>>> = Arc::new(tokio::sync::Mutex::new(vec![]));
        let captured_clone = captured.clone();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                *captured_clone.lock().await = buf[..n].to_vec();
                let response = b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response).await;
            }
        });

        let plugin = CurlPlugin::new();
        let dir = tempfile::tempdir().unwrap();
        let args = json!({
            "url": format!("http://{addr}"),
            "method": "POST",
            "body": "payload=hello"
        });

        let result = plugin
            .run_command("request", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["status"], 201);
        assert_eq!(result.output["method"], "POST");
        let request_bytes = captured.lock().await.clone();
        let request_str = String::from_utf8_lossy(&request_bytes);
        assert!(
            request_str.contains("payload=hello"),
            "body should be forwarded; got: {request_str}"
        );
    }
}
