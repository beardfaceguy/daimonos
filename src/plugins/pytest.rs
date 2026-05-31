use std::collections::HashMap;
use std::path::Path;

use serde_json::json;
use tokio::process::Command;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

/// Pytest plugin: runs Python tests via the `pytest` binary and returns
/// structured pass/fail counts plus a list of failed test ids.
///
/// Commands:
/// - `run`: execute pytest, parse the summary line + FAILED lines
/// - `collect`: run `pytest --collect-only -q` and return the test id list
pub struct PytestPlugin {
    descriptor: ToolDescriptor,
}

impl PytestPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for name in ["run", "collect"] {
            commands.insert(
                name.to_string(),
                ToolCommand {
                    bin: "pytest".into(),
                    args: vec![],
                    output: "structured".into(),
                },
            );
        }

        Self {
            descriptor: ToolDescriptor {
                id: "pytest".into(),
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
impl ToolPlugin for PytestPlugin {
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
            "run" => pytest_run(cwd, args).await?,
            "collect" => pytest_collect(cwd, args).await?,
            _ => return Err(format!("unknown pytest command: {command}")),
        };

        Ok(ToolResult {
            tool: "pytest".into(),
            command: command.into(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

async fn run_pytest(cwd: &Path, args: &[&str]) -> Result<(String, String, bool), String> {
    let output = Command::new("pytest")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("pytest exec: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((stdout, stderr, output.status.success()))
}

async fn pytest_run(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let path = args
        .and_then(|a| a.get("path"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let filter = args
        .and_then(|a| a.get("filter"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let markers = args
        .and_then(|a| a.get("markers"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let failfast = args
        .and_then(|a| a.get("failfast"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let verbose = args
        .and_then(|a| a.get("verbose"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Use `--tb=line` for compact one-line failure summaries; `-q` to suppress the
    // per-test progress dots and headers, but keep the FAILED/ERROR summary.
    let mut pytest_args: Vec<String> = vec!["--tb=line".into(), "-q".into(), "--no-header".into()];
    if verbose {
        pytest_args.push("-v".into());
    }
    if failfast {
        pytest_args.push("-x".into());
    }
    if let Some(ref f) = filter {
        pytest_args.push("-k".into());
        pytest_args.push(f.clone());
    }
    if let Some(ref m) = markers {
        pytest_args.push("-m".into());
        pytest_args.push(m.clone());
    }
    if let Some(ref p) = path {
        pytest_args.push(p.clone());
    }

    let arg_refs: Vec<&str> = pytest_args.iter().map(|s| s.as_str()).collect();
    let (stdout, stderr, success) = run_pytest(cwd, &arg_refs).await?;
    let combined = format!("{stdout}\n{stderr}");

    let summary = parse_pytest_summary(&combined);
    let failures = collect_failed_tests(&combined);

    let mut result = json!({
        "ok": success,
        "passed": summary.passed,
        "failed": summary.failed,
        "skipped": summary.skipped,
        "errors": summary.errors,
    });
    if let Some(d) = summary.duration_s {
        result["duration_s"] = json!(d);
    }
    if !failures.is_empty() {
        result["failures"] = json!(failures);
    }

    Ok(result)
}

async fn pytest_collect(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let path = args
        .and_then(|a| a.get("path"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut pytest_args: Vec<String> = vec!["--collect-only".into(), "-q".into()];
    if let Some(ref p) = path {
        pytest_args.push(p.clone());
    }

    let arg_refs: Vec<&str> = pytest_args.iter().map(|s| s.as_str()).collect();
    let (stdout, _stderr, success) = run_pytest(cwd, &arg_refs).await?;

    // `--collect-only -q` prints one test id per line, then a blank line, then the
    // summary "N tests collected in 0.05s". We take everything before the blank line
    // / summary that looks like a test id (contains '::' or ends with '.py').
    let mut tests: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.contains("::") || trimmed.ends_with(".py") {
            tests.push(trimmed.to_string());
        }
    }

    Ok(json!({
        "ok": success,
        "tests": tests,
        "count": tests.len(),
    }))
}

#[derive(Default, Debug, PartialEq)]
struct PytestSummary {
    passed: i64,
    failed: i64,
    skipped: i64,
    errors: i64,
    duration_s: Option<f64>,
}

/// Parse pytest's terminal summary line, e.g.
/// `========= 5 passed, 2 failed, 1 skipped, 1 error in 0.42s =========`
/// `========= 3 passed in 0.05s =========`
/// `========= no tests ran in 0.01s =========`
fn parse_pytest_summary(output: &str) -> PytestSummary {
    let mut summary = PytestSummary::default();

    // Pytest writes the summary as the last `===...===` framed line. Walk lines
    // bottom-up and use the first one that contains "passed", "failed", "error",
    // "skipped" alongside an `in <duration>s`.
    for line in output.lines().rev() {
        let trimmed = line.trim_matches(|c: char| c == '=' || c.is_whitespace());
        if trimmed.is_empty() {
            continue;
        }
        // Heuristic: must mention `in ` followed by seconds and at least one count word.
        let has_count = trimmed.contains("passed")
            || trimmed.contains("failed")
            || trimmed.contains("error")
            || trimmed.contains("skipped")
            || trimmed.contains("no tests ran");
        if !has_count || !trimmed.contains(" in ") {
            continue;
        }

        // Tokenize on commas + " in " and parse `<n> <word>` pairs.
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let mut i = 0;
        while i + 1 < words.len() {
            let n_str = words[i].trim_end_matches(',');
            let w = words[i + 1].trim_end_matches(',');
            if let Ok(n) = n_str.parse::<i64>() {
                match w {
                    "passed" => summary.passed = n,
                    "failed" => summary.failed = n,
                    "skipped" | "deselected" => summary.skipped = n,
                    "error" | "errors" => summary.errors = n,
                    _ => {}
                }
            }
            i += 1;
        }

        // Duration: word after "in" and ending in "s".
        if let Some(in_idx) = words.iter().position(|w| *w == "in") {
            if let Some(dur) = words.get(in_idx + 1) {
                let dur = dur.trim_end_matches('s').trim_end_matches(',');
                if let Ok(d) = dur.parse::<f64>() {
                    summary.duration_s = Some(d);
                }
            }
        }
        break;
    }

    summary
}

/// Pull failed test ids out of `FAILED <id> - <reason>` lines in the
/// `=== short test summary info ===` section.
fn collect_failed_tests(output: &str) -> Vec<String> {
    let mut failures = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("FAILED ") {
            let id = rest.split(" - ").next().unwrap_or(rest).trim();
            if !id.is_empty() {
                failures.push(id.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("ERROR ") {
            let id = rest.split(" - ").next().unwrap_or(rest).trim();
            if !id.is_empty() {
                failures.push(id.to_string());
            }
        }
    }
    failures
}

/// Check if pytest is available on PATH.
pub fn is_available() -> bool {
    std::process::Command::new("pytest")
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

    const PASSING_TEST: &str = r#"def test_add():
    assert 1 + 1 == 2

def test_str():
    assert "hello".upper() == "HELLO"
"#;

    const MIXED_TEST: &str = r#"def test_pass():
    assert True

def test_fail():
    assert 1 == 2

def test_skip():
    import pytest
    pytest.skip("not yet")
"#;

    fn write_test_file(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests").join(name), body).unwrap();
        std::fs::write(dir.join("tests").join("__init__.py"), "").unwrap();
    }

    #[tokio::test]
    async fn parse_summary_all_passed() {
        let out = "\
============================= test session starts ==============================
collected 3 items

tests/test_foo.py ...                                                    [100%]

============================== 3 passed in 0.05s ===============================
";
        let s = parse_pytest_summary(out);
        assert_eq!(s.passed, 3);
        assert_eq!(s.failed, 0);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.errors, 0);
        assert_eq!(s.duration_s, Some(0.05));
    }

    #[tokio::test]
    async fn parse_summary_mixed() {
        let out = "\
========================= 1 failed, 2 passed, 1 skipped in 0.05s =========================
";
        let s = parse_pytest_summary(out);
        assert_eq!(s.passed, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.errors, 0);
    }

    #[tokio::test]
    async fn parse_summary_with_errors() {
        let out = "\
==================== 1 failed, 2 passed, 1 error in 0.10s ====================
";
        let s = parse_pytest_summary(out);
        assert_eq!(s.passed, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.errors, 1);
    }

    #[tokio::test]
    async fn parse_summary_no_tests() {
        let out =
            "============================ no tests ran in 0.01s ============================\n";
        let s = parse_pytest_summary(out);
        assert_eq!(s.passed, 0);
        assert_eq!(s.failed, 0);
        assert_eq!(s.duration_s, Some(0.01));
    }

    #[tokio::test]
    async fn collect_failed_tests_basic() {
        let out = "\
=========================== short test summary info ============================
FAILED tests/test_foo.py::test_bar - assert 1 == 2
FAILED tests/test_foo.py::test_baz - ZeroDivisionError
ERROR tests/test_qux.py::test_setup - fixture failed
========================= 2 failed, 1 error in 0.05s =========================
";
        let failures = collect_failed_tests(out);
        assert_eq!(failures.len(), 3);
        assert!(failures.contains(&"tests/test_foo.py::test_bar".to_string()));
        assert!(failures.contains(&"tests/test_foo.py::test_baz".to_string()));
        assert!(failures.contains(&"tests/test_qux.py::test_setup".to_string()));
    }

    #[tokio::test]
    async fn collect_failed_tests_empty() {
        let out = "1 passed in 0.01s\n";
        assert_eq!(collect_failed_tests(out).len(), 0);
    }

    #[tokio::test]
    async fn plugin_run_passing() {
        if !is_available() {
            eprintln!("skipping: pytest not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_test_file(dir.path(), "test_pass.py", PASSING_TEST);

        let plugin = PytestPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("run", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output["ok"], true);
        assert_eq!(result.output["passed"], 2);
        assert_eq!(result.output["failed"], 0);
    }

    #[tokio::test]
    async fn plugin_run_with_failure() {
        if !is_available() {
            eprintln!("skipping: pytest not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_test_file(dir.path(), "test_mixed.py", MIXED_TEST);

        let plugin = PytestPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("run", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["ok"], false);
        assert_eq!(result.output["passed"], 1);
        assert_eq!(result.output["failed"], 1);
        assert_eq!(result.output["skipped"], 1);
        let failures = result.output["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].as_str().unwrap().contains("test_fail"));
    }

    #[tokio::test]
    async fn plugin_run_with_filter() {
        if !is_available() {
            eprintln!("skipping: pytest not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_test_file(dir.path(), "test_filt.py", PASSING_TEST);

        let plugin = PytestPlugin::new();
        let env = HashMap::new();
        let args = json!({"filter": "test_add"});
        let result = plugin
            .run_command("run", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();
        assert_eq!(result.output["ok"], true);
        assert_eq!(result.output["passed"], 1);
    }

    #[tokio::test]
    async fn plugin_collect() {
        if !is_available() {
            eprintln!("skipping: pytest not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_test_file(dir.path(), "test_coll.py", PASSING_TEST);

        let plugin = PytestPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("collect", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["ok"], true);
        let tests = result.output["tests"].as_array().unwrap();
        assert!(tests.len() >= 2);
        assert!(tests
            .iter()
            .any(|t| t.as_str().unwrap().contains("test_add")));
    }

    #[tokio::test]
    async fn plugin_unknown_command() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = PytestPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("nuke", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown pytest command"));
    }

    #[tokio::test]
    async fn plugin_via_registry() {
        if !is_available() {
            eprintln!("skipping: pytest not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_test_file(dir.path(), "test_reg.py", PASSING_TEST);

        let registry = ToolRegistry::new();
        registry.register(Arc::new(PytestPlugin::new())).await;

        let env = HashMap::new();
        let result = registry
            .run("pytest", "run", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["ok"], true);
        assert_eq!(result.tool, "pytest");
    }
}
