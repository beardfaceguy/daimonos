//! Optional `agent.env` loading (dotenv-style), applied process-wide at startup
//! — before config load and before mode dispatch — so every runtime (mcp,
//! agent, chat, acp, daemon) and every later `std::env::var` read observes the
//! same values from one place, without per-mode duplication.
//!
//! Search order (project-local overrides user-global; the real environment
//! overrides both):
//!   1. `<workspace>/agent.env`
//!   2. `$XDG_CONFIG_HOME/daimonos/agent.env` (else `~/.config/daimonos/agent.env`)
//!
//! A variable already present in the process environment is never overwritten,
//! so values injected by the launcher (e.g. Zed's ACP `env` block) always win.
//!
//! ## Security
//! `agent.env` typically lives in a checked-out repository — untrusted input —
//! and daimonos' environment is inherited by every tool it exec's. To keep a
//! malicious `agent.env` from turning `git clone` into code execution, variables
//! that steer how executables/interpreters load are refused from the file (see
//! [`DENYLIST`]); set those in the real environment if you truly need them.

use std::path::Path;

/// Variables that alter loader/interpreter behaviour and are inherited by
/// exec'd tools — classic injection vectors. Never settable from a file.
const DENYLIST: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "LD_PROFILE",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "PATH",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "IFS",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_EXTERNAL_DIFF",
];

/// Outcome of planning one file's assignments against the current environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// `(key, value)` pairs to set: absent from the environment, not denylisted.
    pub set: Vec<(String, String)>,
    /// Keys skipped because the environment already defines them (env wins).
    pub skipped_present: Vec<String>,
    /// Keys refused because they are on the security denylist.
    pub refused: Vec<String>,
}

/// Parse one line into a `(key, value)` assignment, or `None` for blank lines,
/// `#` comments, and malformed lines. Accepts an optional leading `export `, and
/// single- or double-quoted values (matching quotes are stripped). Unquoted
/// values are taken verbatim after trimming surrounding whitespace.
fn parse_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, raw_value) = assignment.split_once('=')?;
    let key = key.trim();
    // POSIX-ish name: letters, digits, underscore; must not start with a digit.
    let valid = !key.is_empty()
        && !key.as_bytes()[0].is_ascii_digit()
        && key.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric());
    if !valid {
        return None;
    }
    let v = raw_value.trim();
    let value = match (v.chars().next(), v.chars().last()) {
        (Some('"'), Some('"')) | (Some('\''), Some('\'')) if v.len() >= 2 => {
            v[1..v.len() - 1].to_string()
        }
        _ => v.to_string(),
    };
    Some((key.to_string(), value))
}

/// Plan the assignments from `content`, consulting `is_present` to decide which
/// keys the environment already defines. Pure: performs no environment access,
/// which keeps it deterministically testable.
pub fn plan(content: &str, is_present: impl Fn(&str) -> bool) -> Plan {
    let mut plan = Plan::default();
    for line in content.lines() {
        let Some((key, value)) = parse_line(line) else {
            continue;
        };
        if DENYLIST.contains(&key.as_str()) {
            plan.refused.push(key);
        } else if is_present(&key) {
            plan.skipped_present.push(key);
        } else {
            plan.set.push((key, value));
        }
    }
    plan
}

/// Load `agent.env` from the workspace then the user config dir, setting any
/// absent, non-denylisted variables into the process environment. Called once at
/// startup, before config load and mode dispatch. `quiet` suppresses the
/// informational summary (MCP stdio without `--verbose`); denylist refusals are
/// always reported because they signal a potentially hostile file.
pub fn load_default(workspace: &Path, quiet: bool) {
    let mut candidates = vec![workspace.join("agent.env")];
    if let Some(dir) = crate::paths::config_dir() {
        candidates.push(dir.join("daimonos").join("agent.env"));
    }
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                if !quiet {
                    eprintln!("daimonos: could not read {}: {e}", path.display());
                }
                continue;
            }
        };
        let outcome = plan(&content, |key| std::env::var_os(key).is_some());
        for (key, value) in &outcome.set {
            // Safe on edition 2021 and startup-only — no task has begun reading
            // the environment yet; mirrors `session::enhance_process_path()`.
            std::env::set_var(key, value);
        }
        if !quiet && !outcome.set.is_empty() {
            eprintln!(
                "daimonos: loaded {} variable(s) from {}",
                outcome.set.len(),
                path.display()
            );
        }
        for key in &outcome.refused {
            eprintln!(
                "daimonos: refused unsafe variable {:?} from {} \
                 (loader/interpreter hijack vector; set it in the real environment if intended)",
                key,
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_assignments_and_ignores_comments_blanks() {
        assert_eq!(parse_line("FOO=bar"), Some(("FOO".into(), "bar".into())));
        assert_eq!(
            parse_line("  export BAZ=qux  "),
            Some(("BAZ".into(), "qux".into()))
        );
        assert_eq!(parse_line("# comment"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   "), None);
        assert_eq!(parse_line("NOVALUE"), None);
    }

    #[test]
    fn strips_matching_quotes_only() {
        assert_eq!(
            parse_line("A=\"hello world\""),
            Some(("A".into(), "hello world".into()))
        );
        assert_eq!(parse_line("B='x=y'"), Some(("B".into(), "x=y".into())));
        // An unbalanced leading quote is preserved verbatim.
        assert_eq!(
            parse_line("C=\"unbalanced"),
            Some(("C".into(), "\"unbalanced".into()))
        );
    }

    #[test]
    fn rejects_invalid_keys() {
        assert_eq!(parse_line("1BAD=x"), None);
        assert_eq!(parse_line("with-dash=x"), None);
        assert_eq!(parse_line("with space=x"), None);
    }

    #[test]
    fn plan_skips_present_and_refuses_denylisted() {
        let content =
            "DAIMONOS_AGENT_AUTO_CONTINUE=3\nAPI_KEY=secret\nLD_PRELOAD=/evil.so\nALREADY=1\n";
        let outcome = plan(content, |k| k == "ALREADY");
        assert_eq!(
            outcome.set,
            vec![
                ("DAIMONOS_AGENT_AUTO_CONTINUE".to_string(), "3".to_string()),
                ("API_KEY".to_string(), "secret".to_string()),
            ]
        );
        assert_eq!(outcome.skipped_present, vec!["ALREADY".to_string()]);
        assert_eq!(outcome.refused, vec!["LD_PRELOAD".to_string()]);
    }

    #[test]
    fn denylist_blocks_loader_hijack_vars() {
        for key in ["PATH", "DYLD_INSERT_LIBRARIES", "NODE_OPTIONS", "BASH_ENV"] {
            let outcome = plan(&format!("{key}=x"), |_| false);
            assert_eq!(outcome.refused, vec![key.to_string()]);
            assert!(outcome.set.is_empty());
        }
    }
}
