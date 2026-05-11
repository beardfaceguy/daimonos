use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, PartialEq)]
pub enum ExecFilter {
    TestRunner,
    Install,
    Build,
    Linter,
    None,
}

pub struct FilteredOutput {
    pub out: String,
    pub err: String,
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\x1b\[.*?[@-~]|\r").unwrap()
    })
}

pub fn strip_ansi(text: &str) -> String {
    ansi_re().replace_all(text, "").to_string()
}

pub fn classify(cmd: &str) -> ExecFilter {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    if words.is_empty() {
        return ExecFilter::None;
    }

    let base = words[0].rsplit('/').next().unwrap_or(words[0]);
    let second = words.get(1).copied().unwrap_or("");

    // Test runners
    if base == "pytest" || base == "py.test" {
        return ExecFilter::TestRunner;
    }
    if (base == "python" || base == "python3") && cmd.contains("-m pytest") {
        return ExecFilter::TestRunner;
    }
    if base == "cargo" && second == "test" {
        return ExecFilter::TestRunner;
    }
    if base == "go" && second == "test" {
        return ExecFilter::TestRunner;
    }
    if (base == "npm" || base == "yarn" || base == "pnpm") && second == "test" {
        return ExecFilter::TestRunner;
    }
    if base == "npx" && (second == "jest" || second == "vitest" || second == "mocha") {
        return ExecFilter::TestRunner;
    }
    if base == "jest" || base == "vitest" || base == "mocha" {
        return ExecFilter::TestRunner;
    }
    if base == "rspec" {
        return ExecFilter::TestRunner;
    }
    if base == "rake" && second == "test" {
        return ExecFilter::TestRunner;
    }

    // Package install
    if (base == "pip" || base == "pip3" || base == "uv") && second == "install" {
        return ExecFilter::Install;
    }
    if (base == "npm" || base == "yarn" || base == "pnpm")
        && (second == "install" || second == "add" || second == "i")
    {
        return ExecFilter::Install;
    }
    if base == "cargo" && second == "add" {
        return ExecFilter::Install;
    }
    if (base == "apt" || base == "apt-get") && second == "install" {
        return ExecFilter::Install;
    }
    if base == "brew" && second == "install" {
        return ExecFilter::Install;
    }

    // Build
    if base == "make" || base == "cmake" || base == "ninja" {
        return ExecFilter::Build;
    }
    if base == "cargo" && second == "build" {
        return ExecFilter::Build;
    }
    if base == "go" && second == "build" {
        return ExecFilter::Build;
    }

    // Linters
    if base == "ruff" && second == "check" {
        return ExecFilter::Linter;
    }
    if base == "eslint" || base == "pylint" || base == "mypy" || base == "flake8" {
        return ExecFilter::Linter;
    }
    if base == "cargo" && second == "clippy" {
        return ExecFilter::Linter;
    }
    if base == "shellcheck" {
        return ExecFilter::Linter;
    }

    ExecFilter::None
}

/// Apply the appropriate filter to exec output based on the command.
/// Returns None if no filter applies (passthrough).
pub fn filter_exec_output(
    cmd: &str,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> Option<FilteredOutput> {
    let filter = classify(cmd);
    if filter == ExecFilter::None {
        return None;
    }

    let clean_out = strip_ansi(stdout);
    let clean_err = strip_ansi(stderr);

    match filter {
        ExecFilter::TestRunner => Some(filter_test_output(&clean_out, &clean_err, exit_code)),
        ExecFilter::Install => Some(filter_install_output(&clean_out, &clean_err, exit_code)),
        ExecFilter::Build => Some(filter_build_output(&clean_out, &clean_err, exit_code)),
        ExecFilter::Linter => Some(filter_build_output(&clean_out, &clean_err, exit_code)),
        ExecFilter::None => None,
    }
}

// ---------------------------------------------------------------------------
// Test runner filter: show summary + failures only
// ---------------------------------------------------------------------------

fn filter_test_output(stdout: &str, stderr: &str, exit_code: i32) -> FilteredOutput {
    let combined = format!("{stderr}{stdout}");
    let lines: Vec<&str> = combined.lines().collect();

    let mut summary_lines = Vec::new();
    let mut failure_lines = Vec::new();
    let mut in_failure_block = false;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if in_failure_block {
                failure_lines.push(*line);
            }
            continue;
        }

        // Cargo test summary
        if trimmed.starts_with("test result:") {
            summary_lines.push(trimmed.to_string());
            in_failure_block = false;
            continue;
        }
        // Cargo test failure header
        if trimmed.starts_with("failures:") || trimmed.starts_with("---- ") {
            in_failure_block = true;
            failure_lines.push(*line);
            continue;
        }

        // Pytest summary line (e.g. "1 failed, 5 passed in 0.12s")
        if (trimmed.contains(" passed") || trimmed.contains(" failed"))
            && (trimmed.contains(" in ") || trimmed.contains("error"))
            && trimmed.len() < 200
        {
            summary_lines.push(trimmed.to_string());
            continue;
        }
        // Pytest FAILED lines
        if trimmed.starts_with("FAILED ") {
            failure_lines.push(*line);
            continue;
        }
        // Pytest short test summary info section
        if trimmed.contains("short test summary") {
            in_failure_block = true;
            continue;
        }

        // Go test results
        if trimmed.starts_with("ok ") || trimmed.starts_with("FAIL\t") || trimmed.starts_with("--- FAIL") {
            if trimmed.starts_with("ok ") {
                summary_lines.push(trimmed.to_string());
            } else {
                failure_lines.push(*line);
            }
            continue;
        }

        // Jest / vitest summary
        if trimmed.starts_with("Tests:") || trimmed.starts_with("Test Suites:") {
            summary_lines.push(trimmed.to_string());
            continue;
        }
        if trimmed.contains("✕") || trimmed.contains("FAIL ") {
            failure_lines.push(*line);
            continue;
        }

        // Continuation of failure block (indented lines = detail)
        if in_failure_block
            && (line.starts_with(' ') || line.starts_with('\t') || trimmed.starts_with("thread"))
        {
            failure_lines.push(*line);
        } else if in_failure_block && !line.starts_with(' ') && !line.starts_with('\t') {
            in_failure_block = false;
        }
    }

    let mut out = String::new();

    if !failure_lines.is_empty() {
        for line in failure_lines.iter().take(30) {
            out.push_str(line);
            out.push('\n');
        }
        if failure_lines.len() > 30 {
            out.push_str(&format!("... +{} more failure lines\n", failure_lines.len() - 30));
        }
    }

    if !summary_lines.is_empty() {
        for s in &summary_lines {
            out.push_str(s);
            out.push('\n');
        }
    } else if exit_code == 0 {
        out.push_str("ok\n");
    } else {
        out.push_str(&format!("FAILED (exit {})\n", exit_code));
        // Include last 10 lines as fallback context
        let start = lines.len().saturating_sub(10);
        for line in &lines[start..] {
            if !line.trim().is_empty() {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    FilteredOutput {
        out: out.trim_end().to_string(),
        err: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Install filter: on success → "ok", on failure → error lines only
// ---------------------------------------------------------------------------

fn filter_install_output(stdout: &str, stderr: &str, exit_code: i32) -> FilteredOutput {
    if exit_code == 0 {
        // Count installed packages if possible
        let combined = format!("{stdout}{stderr}");
        let pkg_count = combined
            .lines()
            .filter(|l| {
                let t = l.trim().to_lowercase();
                t.starts_with("successfully installed")
                    || t.starts_with("added ")
                    || t.contains("packages in")
                    || t.starts_with("installing ")
            })
            .count();

        let msg = if pkg_count > 0 {
            format!("ok: install complete ({pkg_count} operations)")
        } else {
            "ok: install complete".to_string()
        };
        return FilteredOutput {
            out: msg,
            err: String::new(),
        };
    }

    // On failure, keep only error lines
    let mut error_lines = Vec::new();
    for line in stderr.lines().chain(stdout.lines()) {
        let lower = line.to_lowercase();
        if lower.contains("error")
            || lower.contains("not found")
            || lower.contains("failed")
            || lower.contains("conflict")
            || lower.contains("could not")
            || lower.contains("permission denied")
        {
            error_lines.push(line);
        }
    }

    if error_lines.is_empty() {
        // Fallback: last 10 lines
        let lines: Vec<&str> = stderr.lines().chain(stdout.lines()).collect();
        let start = lines.len().saturating_sub(10);
        error_lines = lines[start..].to_vec();
    }

    let out: String = error_lines
        .iter()
        .take(20)
        .map(|l| format!("{l}\n"))
        .collect();

    FilteredOutput {
        out: format!("FAILED (exit {exit_code})\n{}", out.trim_end()),
        err: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Build / linter filter: keep only error/warning lines + context
// ---------------------------------------------------------------------------

fn error_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^.*?(error|warning|fatal|undefined reference)[\s:\[].{0,200}$").unwrap()
    })
}

fn noise_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(   Compiling |   Finished |   Downloading |   Downloaded |   Updating |    Building |   Packaging |     Running |   Documenting |     Locking |     Waiting |    Blocking |    Fetching )"
        ).unwrap()
    })
}

fn filter_build_output(stdout: &str, stderr: &str, exit_code: i32) -> FilteredOutput {
    if exit_code == 0 {
        // On success: check if there are warnings
        let combined = format!("{stderr}{stdout}");
        let warning_count = combined
            .lines()
            .filter(|l| {
                let t = l.trim();
                (t.contains("warning:") || t.contains("warning["))
                    && !t.contains("generated")
                    && !t.contains("warnings emitted")
            })
            .count();

        let msg = if warning_count > 0 {
            format!("ok ({warning_count} warnings)")
        } else {
            "ok".to_string()
        };

        return FilteredOutput {
            out: msg,
            err: String::new(),
        };
    }

    // On failure: extract error/warning blocks
    let combined = format!("{stderr}{stdout}");
    let lines: Vec<&str> = combined.lines().collect();
    let error_re = error_line_re();
    let noise_re = noise_line_re();

    let mut kept = Vec::new();
    let mut in_error_block = false;
    let mut blank_count = 0usize;

    for line in &lines {
        if noise_re.is_match(line) {
            continue;
        }

        let is_error = error_re.is_match(line);
        // Rust-specific: source location pointers
        let is_location = line.trim_start().starts_with("--> ");

        if is_error || is_location {
            in_error_block = true;
            blank_count = 0;
            kept.push(*line);
        } else if in_error_block {
            if line.trim().is_empty() {
                blank_count += 1;
                if blank_count >= 2 {
                    in_error_block = false;
                } else {
                    kept.push(*line);
                }
            } else if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('|') {
                blank_count = 0;
                kept.push(*line);
            } else {
                in_error_block = false;
            }
        }
    }

    if kept.is_empty() {
        // Fallback: last 15 lines
        let start = lines.len().saturating_sub(15);
        kept = lines[start..].to_vec();
    }

    // Cap at 50 lines
    let mut out = String::new();
    for line in kept.iter().take(50) {
        out.push_str(line);
        out.push('\n');
    }
    if kept.len() > 50 {
        out.push_str(&format!("... +{} more diagnostic lines\n", kept.len() - 50));
    }

    FilteredOutput {
        out: format!("FAILED (exit {exit_code})\n{}", out.trim_end()),
        err: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- classify tests ---

    #[test]
    fn classify_cargo_test() {
        assert_eq!(classify("cargo test"), ExecFilter::TestRunner);
        assert_eq!(classify("cargo test --lib"), ExecFilter::TestRunner);
        assert_eq!(classify("cargo test -- test_foo"), ExecFilter::TestRunner);
    }

    #[test]
    fn classify_pytest() {
        assert_eq!(classify("pytest"), ExecFilter::TestRunner);
        assert_eq!(classify("pytest tests/ -v"), ExecFilter::TestRunner);
        assert_eq!(classify("python3 -m pytest tests/"), ExecFilter::TestRunner);
        assert_eq!(classify("python -m pytest"), ExecFilter::TestRunner);
    }

    #[test]
    fn classify_go_test() {
        assert_eq!(classify("go test ./..."), ExecFilter::TestRunner);
    }

    #[test]
    fn classify_npm_test() {
        assert_eq!(classify("npm test"), ExecFilter::TestRunner);
        assert_eq!(classify("yarn test"), ExecFilter::TestRunner);
        assert_eq!(classify("pnpm test"), ExecFilter::TestRunner);
    }

    #[test]
    fn classify_jest_vitest() {
        assert_eq!(classify("jest"), ExecFilter::TestRunner);
        assert_eq!(classify("vitest"), ExecFilter::TestRunner);
        assert_eq!(classify("npx jest --coverage"), ExecFilter::TestRunner);
    }

    #[test]
    fn classify_install() {
        assert_eq!(classify("pip install requests"), ExecFilter::Install);
        assert_eq!(classify("pip3 install -r requirements.txt"), ExecFilter::Install);
        assert_eq!(classify("npm install"), ExecFilter::Install);
        assert_eq!(classify("npm i express"), ExecFilter::Install);
        assert_eq!(classify("yarn add lodash"), ExecFilter::Install);
        assert_eq!(classify("pnpm install"), ExecFilter::Install);
        assert_eq!(classify("cargo add serde"), ExecFilter::Install);
        assert_eq!(classify("brew install jq"), ExecFilter::Install);
        assert_eq!(classify("uv install flask"), ExecFilter::Install);
    }

    #[test]
    fn classify_build() {
        assert_eq!(classify("make"), ExecFilter::Build);
        assert_eq!(classify("make -j8"), ExecFilter::Build);
        assert_eq!(classify("cargo build"), ExecFilter::Build);
        assert_eq!(classify("cargo build --release"), ExecFilter::Build);
        assert_eq!(classify("go build ./..."), ExecFilter::Build);
        assert_eq!(classify("cmake --build ."), ExecFilter::Build);
    }

    #[test]
    fn classify_linter() {
        assert_eq!(classify("cargo clippy"), ExecFilter::Linter);
        assert_eq!(classify("ruff check ."), ExecFilter::Linter);
        assert_eq!(classify("eslint src/"), ExecFilter::Linter);
        assert_eq!(classify("pylint mymodule"), ExecFilter::Linter);
        assert_eq!(classify("mypy src/"), ExecFilter::Linter);
    }

    #[test]
    fn classify_none() {
        assert_eq!(classify("echo hello"), ExecFilter::None);
        assert_eq!(classify("ls -la"), ExecFilter::None);
        assert_eq!(classify("cat file.txt"), ExecFilter::None);
        assert_eq!(classify("git status"), ExecFilter::None);
        assert_eq!(classify("curl https://example.com"), ExecFilter::None);
    }

    #[test]
    fn classify_with_path_prefix() {
        assert_eq!(classify("/usr/bin/pytest tests/"), ExecFilter::TestRunner);
        assert_eq!(classify("/usr/local/bin/make"), ExecFilter::Build);
    }

    // --- strip_ansi tests ---

    #[test]
    fn strip_ansi_removes_color_codes() {
        let input = "\x1b[32mPASS\x1b[0m test_foo";
        assert_eq!(strip_ansi(input), "PASS test_foo");
    }

    #[test]
    fn strip_ansi_removes_carriage_returns() {
        let input = "Downloading... 50%\rDownloading... 100%\r\n";
        let cleaned = strip_ansi(input);
        assert!(!cleaned.contains('\r'));
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        let input = "hello world";
        assert_eq!(strip_ansi(input), "hello world");
    }

    // --- test runner filter ---

    #[test]
    fn test_filter_cargo_all_pass() {
        let stdout = "\
running 5 tests
test tests::test_a ... ok
test tests::test_b ... ok
test tests::test_c ... ok
test tests::test_d ... ok
test tests::test_e ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let result = filter_test_output(stdout, "", 0);
        assert!(result.out.contains("5 passed"));
        assert!(result.out.contains("0 failed"));
        assert!(!result.out.contains("test tests::test_a"));
    }

    #[test]
    fn test_filter_cargo_with_failures() {
        let stderr = "\
running 3 tests
test tests::test_a ... ok
test tests::test_b ... FAILED
test tests::test_c ... ok

failures:

---- tests::test_b stdout ----
thread 'tests::test_b' panicked at 'assertion failed: false'

failures:
    tests::test_b

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        let result = filter_test_output("", stderr, 101);
        assert!(result.out.contains("1 failed"));
        assert!(result.out.contains("tests::test_b"));
        assert!(!result.out.contains("test tests::test_a ... ok"));
    }

    #[test]
    fn test_filter_pytest_all_pass() {
        let stdout = "\
============================= test session starts ==============================
platform linux -- Python 3.12.0
collected 8 items

tests/test_foo.py ........                                               [100%]

============================== 8 passed in 0.12s ===============================
";
        let result = filter_test_output(stdout, "", 0);
        assert!(result.out.contains("8 passed"));
        assert!(!result.out.contains("test session starts"));
        assert!(!result.out.contains("[100%]"));
    }

    #[test]
    fn test_filter_pytest_with_failures() {
        let stdout = "\
============================= test session starts ==============================
collected 3 items

tests/test_foo.py .F.                                                    [100%]

=========================== short test summary info ============================
FAILED tests/test_foo.py::test_bar - assert 1 == 2
========================= 1 failed, 2 passed in 0.05s =========================
";
        let result = filter_test_output(stdout, "", 1);
        assert!(result.out.contains("FAILED tests/test_foo.py::test_bar"));
        assert!(result.out.contains("1 failed, 2 passed"));
    }

    // --- install filter ---

    #[test]
    fn install_filter_success() {
        let stdout = "\
Collecting requests
  Downloading requests-2.31.0.tar.gz (110 kB)
  Using cached certifi-2024.2.2.tar.gz
Installing collected packages: requests, certifi
Successfully installed requests-2.31.0 certifi-2024.2.2
";
        let result = filter_install_output(stdout, "", 0);
        assert!(result.out.starts_with("ok: install complete"));
        assert!(!result.out.contains("Collecting"));
        assert!(!result.out.contains("Downloading"));
    }

    #[test]
    fn install_filter_failure() {
        let stderr = "ERROR: Could not find a version that satisfies the requirement nonexistent-pkg\n";
        let result = filter_install_output("", stderr, 1);
        assert!(result.out.contains("FAILED"));
        assert!(result.out.contains("Could not find"));
    }

    // --- build filter ---

    #[test]
    fn build_filter_success_no_warnings() {
        let stderr = "\
   Compiling serde v1.0.0
   Compiling daimonos v0.1.0
    Finished dev [unoptimized + debuginfo] target(s) in 5.23s
";
        let result = filter_build_output("", stderr, 0);
        assert_eq!(result.out, "ok");
    }

    #[test]
    fn build_filter_success_with_warnings() {
        let stderr = "\
   Compiling daimonos v0.1.0
warning: unused variable: `x`
 --> src/main.rs:10:9
  |
10 |     let x = 5;
  |         ^ help: if this is intentional, prefix it with an underscore: `_x`

warning: `daimonos` generated 1 warning
    Finished dev [unoptimized + debuginfo] target(s) in 1.23s
";
        let result = filter_build_output("", stderr, 0);
        assert!(result.out.contains("1 warnings"));
    }

    #[test]
    fn build_filter_failure() {
        let stderr = "\
   Compiling daimonos v0.1.0
error[E0308]: mismatched types
 --> src/main.rs:5:20
  |
5 |     let x: u32 = \"hello\";
  |            ---   ^^^^^^^ expected `u32`, found `&str`
  |            |
  |            expected due to this

error: aborting due to previous error
";
        let result = filter_build_output("", stderr, 101);
        assert!(result.out.contains("FAILED"));
        assert!(result.out.contains("error[E0308]"));
        assert!(result.out.contains("mismatched types"));
        assert!(!result.out.contains("Compiling"));
    }

    // --- filter_exec_output integration ---

    #[test]
    fn filter_exec_applies_to_cargo_test() {
        let result = filter_exec_output("cargo test", "test result: ok. 3 passed; 0 failed; 0 ignored\n", "", 0);
        assert!(result.is_some());
        let filtered = result.unwrap();
        assert!(filtered.out.contains("3 passed"));
    }

    #[test]
    fn filter_exec_none_for_unknown_command() {
        let result = filter_exec_output("echo hello", "hello\n", "", 0);
        assert!(result.is_none());
    }

    #[test]
    fn filter_exec_strips_ansi() {
        let colored = "\x1b[32mtest result: ok. 1 passed; 0 failed; 0 ignored\x1b[0m\n";
        let result = filter_exec_output("cargo test", colored, "", 0);
        assert!(result.is_some());
        let filtered = result.unwrap();
        assert!(!filtered.out.contains("\x1b"));
        assert!(filtered.out.contains("1 passed"));
    }
}
