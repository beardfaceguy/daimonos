use std::collections::HashMap;
use std::path::Path;

use serde_json::json;
use tokio::process::Command;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

pub struct CargoPlugin {
    descriptor: ToolDescriptor,
}

impl CargoPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for name in ["test", "build", "check", "clippy", "fmt", "add"] {
            commands.insert(
                name.to_string(),
                ToolCommand {
                    bin: "cargo".into(),
                    args: vec![name.into()],
                    output: "structured".into(),
                },
            );
        }

        Self {
            descriptor: ToolDescriptor {
                id: "cargo".into(),
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
impl ToolPlugin for CargoPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        _env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
    ) -> Result<ToolResult, String> {
        let output = match command {
            "test" => cargo_test(cwd, args).await?,
            "build" => cargo_diagnostics(cwd, "build", args).await?,
            "check" => cargo_diagnostics(cwd, "check", args).await?,
            "clippy" => cargo_diagnostics(cwd, "clippy", args).await?,
            "fmt" => cargo_fmt(cwd, args).await?,
            "add" => cargo_add(cwd, args).await?,
            _ => return Err(format!("unknown cargo command: {command}")),
        };

        Ok(ToolResult {
            tool: "cargo".into(),
            command: command.into(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

/// Run cargo with given args, returning (stdout, stderr, success).
async fn run_cargo(cwd: &Path, args: &[&str]) -> Result<(String, String, bool), String> {
    let output = Command::new("cargo")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("cargo exec: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((stdout, stderr, output.status.success()))
}

fn append_package_arg<'a>(cargo_args: &mut Vec<&'a str>, pkg: &'a str) {
    cargo_args.push("--package");
    cargo_args.push(pkg);
}

/// Run `cargo test`, parse output into structured results.
async fn cargo_test(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut cargo_args = vec!["test"];

    let pkg = args
        .and_then(|a| a.get("package"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let filter = args
        .and_then(|a| a.get("filter"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let lib = args
        .and_then(|a| a.get("lib"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(ref p) = pkg {
        append_package_arg(&mut cargo_args, p);
    }
    if lib {
        cargo_args.push("--lib");
    }

    if let Some(ref f) = filter {
        cargo_args.push("--");
        cargo_args.push(f);
    }

    let (stdout, stderr, success) = run_cargo(cwd, &cargo_args).await?;
    let combined = format!("{stderr}{stdout}");

    let mut passed: i64 = 0;
    let mut failed: i64 = 0;
    let mut ignored: i64 = 0;
    let mut failures: Vec<String> = Vec::new();

    for line in combined.lines() {
        if line.starts_with("test result:") {
            if let Some(counts) = parse_test_summary(line) {
                passed += counts.0;
                failed += counts.1;
                ignored += counts.2;
            }
        } else if line.starts_with("---- ") && line.ends_with(" ----") {
            let name = line.trim_start_matches("---- ").trim_end_matches(" ----");
            if !name.is_empty() {
                failures.push(name.to_string());
            }
        }
    }

    let mut result = json!({
        "ok": success,
        "passed": passed,
        "failed": failed,
        "ignored": ignored,
    });

    if !failures.is_empty() {
        result["failures"] = json!(failures);
    }

    Ok(result)
}

/// Parse "test result: ok. 3 passed; 0 failed; 1 ignored; ..." into (passed, failed, ignored).
fn parse_test_summary(line: &str) -> Option<(i64, i64, i64)> {
    let mut passed = 0i64;
    let mut failed = 0i64;
    let mut ignored = 0i64;

    let words: Vec<&str> = line.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if i == 0 {
            continue;
        }
        match *w {
            "passed" | "passed;" => {
                if let Ok(n) = words[i - 1].trim_end_matches('.').parse::<i64>() {
                    passed = n;
                }
            }
            "failed" | "failed;" => {
                if let Ok(n) = words[i - 1].parse::<i64>() {
                    failed = n;
                }
            }
            "ignored" | "ignored;" => {
                if let Ok(n) = words[i - 1].parse::<i64>() {
                    ignored = n;
                }
            }
            _ => {}
        }
    }

    Some((passed, failed, ignored))
}

/// Parsed result of a `cargo --message-format=json` run.
struct CargoParse {
    errors: Vec<serde_json::Value>,
    warnings: Vec<serde_json::Value>,
    /// True if cargo recompiled at least one crate this run (any
    /// `compiler-artifact` with `fresh:false`); false means every artifact was
    /// fresh — a no-op build that produced no new binary. Lets callers tell an
    /// up-to-date `ok:true` from one that actually rebuilt (vikunja #946).
    rebuilt: bool,
}

/// Parse a cargo JSON message stream into diagnostics plus whether anything was
/// actually (re)compiled. Pure over captured stdout so it is unit-testable.
fn parse_cargo_json(stdout: &str) -> CargoParse {
    let mut errors: Vec<serde_json::Value> = Vec::new();
    let mut warnings: Vec<serde_json::Value> = Vec::new();
    let mut rebuilt = false;

    for line in stdout.lines() {
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg.get("reason").and_then(|r| r.as_str()) {
            Some("compiler-artifact") => {
                // fresh:false == this crate was recompiled this run.
                if msg.get("fresh").and_then(|f| f.as_bool()) == Some(false) {
                    rebuilt = true;
                }
            }
            Some("compiler-message") => {
                let message = match msg.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let level = message.get("level").and_then(|l| l.as_str()).unwrap_or("");
                let text = message
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let (file, line_num) = extract_span_location(message);
                let mut diag = json!({"message": text});
                if let Some(f) = file {
                    diag["file"] = json!(f);
                }
                if let Some(l) = line_num {
                    diag["line"] = json!(l);
                }
                match level {
                    "error" => errors.push(diag),
                    "warning" => warnings.push(diag),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    CargoParse {
        errors,
        warnings,
        rebuilt,
    }
}

/// Run `cargo build/check/clippy` with `--message-format=json` and parse compiler diagnostics.
async fn cargo_diagnostics(
    cwd: &Path,
    subcommand: &str,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut cargo_args = vec![subcommand, "--message-format=json"];

    let pkg = args
        .and_then(|a| a.get("package"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let release = args
        .and_then(|a| a.get("release"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(ref p) = pkg {
        append_package_arg(&mut cargo_args, p);
    }
    if release {
        cargo_args.push("--release");
    }

    let (stdout, _stderr, success) = run_cargo(cwd, &cargo_args).await?;
    let parsed = parse_cargo_json(&stdout);

    // `rebuilt` distinguishes a real recompile from a cargo no-op: `ok:true`
    // with `rebuilt:false` means cargo considered the tree up-to-date and left
    // the existing artifact in place — which can be stale if a fingerprint quirk
    // (e.g. mtime skew after git ops) fooled cargo. Surfacing it lets a caller
    // that needs a guaranteed-fresh binary notice (vikunja #946).
    let mut result = json!({"ok": success, "rebuilt": parsed.rebuilt});
    if !parsed.errors.is_empty() {
        result["errors"] = json!(parsed.errors);
    }
    if !parsed.warnings.is_empty() {
        result["warnings"] = json!(parsed.warnings);
    }
    Ok(result)
}

/// Extract file and line number from the primary span in a compiler message.
fn extract_span_location(message: &serde_json::Value) -> (Option<String>, Option<i64>) {
    let spans = match message.get("spans").and_then(|s| s.as_array()) {
        Some(s) => s,
        None => return (None, None),
    };

    // Prefer the primary span
    let span = spans
        .iter()
        .find(|s| {
            s.get("is_primary")
                .and_then(|p| p.as_bool())
                .unwrap_or(false)
        })
        .or_else(|| spans.first());

    match span {
        Some(s) => {
            let file = s
                .get("file_name")
                .and_then(|f| f.as_str())
                .map(String::from);
            let line = s.get("line_start").and_then(|l| l.as_i64());
            (file, line)
        }
        None => (None, None),
    }
}

/// Run `cargo fmt --check`, report whether formatting is needed.
async fn cargo_fmt(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut cargo_args = vec!["fmt", "--check"];

    let pkg = args
        .and_then(|a| a.get("package"))
        .and_then(|v| v.as_str())
        .map(String::from);
    if let Some(ref p) = pkg {
        append_package_arg(&mut cargo_args, p);
    }

    let (_stdout, stderr, success) = run_cargo(cwd, &cargo_args).await?;

    let mut unformatted: Vec<String> = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix("Diff in ") {
            let file = rest.split_whitespace().next().unwrap_or(rest);
            unformatted.push(file.to_string());
        }
    }

    let mut result = json!({"formatted": success});
    if !unformatted.is_empty() {
        result["unformatted"] = json!(unformatted);
    }
    Ok(result)
}

/// Run `cargo add <package>`, return success/failure.
async fn cargo_add(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let package = args
        .and_then(|a| a.get("package"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "cargo add requires 'package' argument".to_string())?;

    let dev = args
        .and_then(|a| a.get("dev"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut cargo_args = vec!["add", package];
    if dev {
        cargo_args.push("--dev");
    }

    let (_stdout, stderr, success) = run_cargo(cwd, &cargo_args).await?;

    if !success {
        return Err(format!("cargo add: {}", stderr.trim()));
    }

    Ok(json!({"ok": true, "package": package, "dev": dev}))
}

/// Check if cargo is available on PATH.
pub fn is_available() -> bool {
    std::process::Command::new("cargo")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_runner::ToolRegistry;
    use std::sync::Arc;

    #[test]
    fn parse_cargo_json_flags_recompile_vs_noop() {
        // vikunja #946: an all-fresh (no-op) build must report rebuilt=false so
        // ok:true can't be mistaken for "a new binary was produced".
        let noop = concat!(
            r#"{"reason":"compiler-artifact","fresh":true}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#
        );
        assert!(!parse_cargo_json(noop).rebuilt);

        let did = concat!(
            r#"{"reason":"compiler-artifact","fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","fresh":false}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#
        );
        assert!(parse_cargo_json(did).rebuilt);
    }

    #[test]
    fn parse_cargo_json_collects_diagnostics_and_ignores_junk() {
        let out = concat!(
            r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused variable","spans":[]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","spans":[{"file_name":"src/x.rs","line_start":7,"is_primary":true}]}}"#,
            "\n",
            "not json at all",
            "\n",
            r#"{"reason":"build-finished","success":false}"#
        );
        let p = parse_cargo_json(out);
        assert_eq!(p.warnings.len(), 1);
        assert_eq!(p.errors.len(), 1);
        assert_eq!(p.errors[0]["file"], "src/x.rs");
        assert_eq!(p.errors[0]["line"], 7);
        assert!(!p.rebuilt);
    }

    fn minimal_cargo_toml() -> &'static str {
        r#"[package]
name = "test-crate"
version = "0.1.0"
edition = "2021"
"#
    }

    fn valid_lib_rs() -> &'static str {
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }
}
"#
    }

    async fn setup_cargo_project(dir: &Path) {
        std::fs::write(dir.join("Cargo.toml"), minimal_cargo_toml()).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), valid_lib_rs()).unwrap();
    }

    #[tokio::test]
    async fn plugin_check_valid_code() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_project(dir.path()).await;

        let plugin = CargoPlugin::new();
        let env = HashMap::new();
        let args = json!({"command": "check"});
        let result = plugin
            .run_command("check", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output["ok"], true);
    }

    #[tokio::test]
    async fn plugin_test_passing() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_project(dir.path()).await;

        let plugin = CargoPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("test", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output["ok"], true);
        assert!(result.output["passed"].as_i64().unwrap() >= 1);
        assert_eq!(result.output["failed"], 0);
    }

    #[tokio::test]
    async fn plugin_fmt_check() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_project(dir.path()).await;

        let plugin = CargoPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("fmt", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        // Well-formatted code should report formatted: true
        assert_eq!(result.output["formatted"], true);
    }

    #[tokio::test]
    async fn plugin_unknown_command() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = CargoPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("deploy", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown cargo command"));
    }

    #[tokio::test]
    async fn is_available_returns_true() {
        assert!(is_available());
    }

    #[tokio::test]
    async fn plugin_via_registry() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_project(dir.path()).await;

        let registry = ToolRegistry::new();
        registry.register(Arc::new(CargoPlugin::new())).await;

        let env = HashMap::new();
        let result = registry
            .run("cargo", "check", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["ok"], true);
        assert_eq!(result.tool, "cargo");
    }

    #[test]
    fn parse_test_summary_ok() {
        let line = "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s";
        let (passed, failed, ignored) = parse_test_summary(line).unwrap();
        assert_eq!(passed, 3);
        assert_eq!(failed, 0);
        assert_eq!(ignored, 1);
    }

    #[test]
    fn parse_test_summary_failed() {
        let line = "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s";
        let (passed, failed, ignored) = parse_test_summary(line).unwrap();
        assert_eq!(passed, 2);
        assert_eq!(failed, 1);
        assert_eq!(ignored, 0);
    }

    #[test]
    fn extract_span_location_with_primary() {
        let msg = json!({
            "spans": [
                {"file_name": "src/main.rs", "line_start": 10, "is_primary": false},
                {"file_name": "src/lib.rs", "line_start": 42, "is_primary": true},
            ]
        });
        let (file, line) = extract_span_location(&msg);
        assert_eq!(file.unwrap(), "src/lib.rs");
        assert_eq!(line.unwrap(), 42);
    }

    #[test]
    fn extract_span_location_no_spans() {
        let msg = json!({});
        let (file, line) = extract_span_location(&msg);
        assert!(file.is_none());
        assert!(line.is_none());
    }

    #[tokio::test]
    async fn plugin_add_requires_package() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_project(dir.path()).await;

        let plugin = CargoPlugin::new();
        let env = HashMap::new();
        let args = json!({});
        let result = plugin
            .run_command("add", dir.path(), &env, None, Some(&args))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires 'package'"));
    }
}
