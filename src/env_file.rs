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
//! The files of this module are the **implicit** files: no option selects
//! them. [`implicit_keys`] names the variables that this process set from
//! them. The agent frontends use it to rank an explicitly selected agent env
//! file above these values (ADR-037, `agent_env`).
//!
//! ## Security
//! `agent.env` typically lives in a checked-out repository — untrusted input —
//! and daimonos' environment is inherited by every tool it exec's. To keep a
//! malicious `agent.env` from turning `git clone` into code execution, variables
//! that steer how executables/interpreters load are refused from the file (see
//! [`DENYLIST`]); set those in the real environment if you truly need them.
//!
//! The two files do not have the same trust (ADR-038). The user file is the
//! user's own configuration. The workspace file comes with the workspace, so it
//! cannot set what decides where requests go, which credential they carry,
//! which tool calls need approval, or which proxy and certificates the process
//! uses (see [`workspace_refusal`]). Those lines are ignored and reported.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

/// The variables that this process set from an implicit `agent.env` file.
static IMPLICIT_KEYS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Names of the variables that this process set from an implicit `agent.env`
/// file. A variable that the launcher supplied is never in this set: an
/// implicit file does not overwrite it.
pub fn implicit_keys() -> BTreeSet<String> {
    IMPLICIT_KEYS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

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

/// Which `agent.env` file an assignment comes from (ADR-038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `<workspace>/agent.env`: untrusted, it comes with the workspace.
    Workspace,
    /// The file in the user configuration directory: trusted.
    User,
}

/// Approval settings of the agent frontends (ADR-034 and the block lists).
const APPROVAL_SETTINGS: &[&str] = &[
    "DAIMONOS_AGENT_APPROVAL_MODE",
    "DAIMONOS_AGENT_ALLOWED_COMMANDS",
    "DAIMONOS_AGENT_DENIED_COMMANDS",
    "DAIMONOS_AGENT_PERMISSION_MODE",
    "DAIMONOS_AGENT_PERMISSION_LIST",
];

/// Variables that decide where the user's own configuration, state, and data
/// are. The user `config.toml` is found through `XDG_CONFIG_HOME` after this
/// module ran, so a workspace value would move that lookup into the workspace.
const USER_DIRECTORY_VARIABLES: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

/// Proxy and certificate variables that the HTTP client and the TLS root store
/// read: `hyper-util` reads the proxy names in both letter cases, and
/// `rustls-native-certs` reads the two certificate names.
const NETWORK_VARIABLES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// ADR-038: why the workspace file cannot set `key`, or `None` when it can.
///
/// - Connection settings: the provider, each base URL, and each API key of
///   the agent, also of a secondary provider.
/// - The selector of the agent env file.
/// - Approval settings.
/// - Proxy and certificate variables.
/// - User directory variables.
///
/// The name is compared without letter case. The agent reads its settings by
/// their exact upper-case names, so on Linux a lower-case line has no effect.
/// The comparison does not depend on that: a platform with case-insensitive
/// variable names gets the same refusal.
pub fn workspace_refusal(key: &str) -> Option<&'static str> {
    let key = key.to_ascii_uppercase();
    let key = key.as_str();
    let agent_setting = key.starts_with("DAIMONOS_AGENT_");
    if key == "DAIMONOS_AGENT_ENV" {
        Some("the agent env file selector")
    } else if key == "DAIMONOS_AGENT_PROVIDER"
        || (agent_setting && (key.ends_with("_API_KEY") || key.ends_with("_BASE_URL")))
    {
        Some("a connection setting")
    } else if APPROVAL_SETTINGS.contains(&key) {
        Some("an approval setting")
    } else if NETWORK_VARIABLES.contains(&key) {
        Some("a proxy or certificate setting")
    } else if USER_DIRECTORY_VARIABLES.contains(&key) {
        Some("a user directory setting")
    } else {
        None
    }
}

/// Outcome of planning one file's assignments against the current environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// `(key, value)` pairs to set: absent from the environment, not denylisted.
    pub set: Vec<(String, String)>,
    /// Keys skipped because the environment already defines them (env wins).
    pub skipped_present: Vec<String>,
    /// Keys refused because they are on the security denylist.
    pub refused: Vec<String>,
    /// `(key, class)` pairs that the workspace file cannot set (ADR-038).
    pub refused_untrusted: Vec<(String, &'static str)>,
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
pub fn plan(content: &str, source: Source, is_present: impl Fn(&str) -> bool) -> Plan {
    let mut plan = Plan::default();
    for line in content.lines() {
        let Some((key, value)) = parse_line(line) else {
            continue;
        };
        let untrusted = match source {
            Source::Workspace => workspace_refusal(&key),
            Source::User => None,
        };
        if DENYLIST.contains(&key.as_str()) {
            plan.refused.push(key);
        } else if let Some(class) = untrusted {
            plan.refused_untrusted.push((key, class));
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
    load_default_with_user_config(workspace, quiet, true);
}

/// State-owned runs never consult HOME/XDG fallback input. ADR-021.
pub(crate) fn load_default_with_user_config(workspace: &Path, quiet: bool, include_user: bool) {
    let mut candidates = vec![(workspace.join("agent.env"), Source::Workspace)];
    if include_user {
        if let Some(dir) = crate::paths::config_dir() {
            candidates.push((dir.join("daimonos").join("agent.env"), Source::User));
        }
    }
    for (path, source) in candidates {
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
        apply_content(&content, &path, quiet, source);
    }
}

/// Load only an authorized workspace env file under confinement (ADR-022).
pub(crate) fn load_confined(
    workspace: &Path,
    authority: &crate::path_policy::FilesystemAuthority,
    quiet: bool,
) {
    use std::io::Read;
    let path = workspace.join("agent.env");
    let Ok(authorized) = authority.open_read(&path) else {
        return;
    };
    let mut content = String::new();
    if authorized.into_file().read_to_string(&mut content).is_ok() {
        apply_content(&content, &path, quiet, Source::Workspace);
    }
}

fn apply_content(content: &str, path: &Path, quiet: bool, source: Source) {
    let outcome = plan(content, source, |key| std::env::var_os(key).is_some());
    for (key, value) in &outcome.set {
        // Safe on edition 2021 and startup-only — no task has begun reading
        // the environment yet; mirrors `session::enhance_process_path()`.
        std::env::set_var(key, value);
    }
    IMPLICIT_KEYS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .extend(outcome.set.iter().map(|(key, _)| key.clone()));
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
    // Reported also in quiet mode, as the refusals above. One line for the
    // file, so a long file does not fill stderr. A value is not printed: it can
    // be a credential.
    if !outcome.refused_untrusted.is_empty() {
        let names: Vec<String> = outcome
            .refused_untrusted
            .iter()
            .map(|(key, class)| format!("{key:?} ({class})"))
            .collect();
        eprintln!(
            "daimonos: ignored {} from {}: a workspace agent.env cannot set these. Set them in \
             the user agent.env, in the file that --agent-env names, or in the real environment.",
            names.join(", "),
            path.display()
        );
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
        let outcome = plan(content, Source::User, |k| k == "ALREADY");
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
            let outcome = plan(&format!("{key}=x"), Source::Workspace, |_| false);
            assert_eq!(outcome.refused, vec![key.to_string()]);
            assert!(outcome.set.is_empty());
        }
    }

    /// ADR-038: the classes that the workspace file cannot set.
    #[test]
    fn the_workspace_file_cannot_set_connection_approval_or_network_settings() {
        let refused = [
            ("DAIMONOS_AGENT_ENV", "the agent env file selector"),
            ("DAIMONOS_AGENT_PROVIDER", "a connection setting"),
            ("DAIMONOS_AGENT_BASE_URL", "a connection setting"),
            ("DAIMONOS_AGENT_API_KEY", "a connection setting"),
            ("DAIMONOS_AGENT_OPENAI_API_KEY", "a connection setting"),
            ("DAIMONOS_AGENT_ANTHROPIC_BASE_URL", "a connection setting"),
            ("DAIMONOS_AGENT_APPROVAL_MODE", "an approval setting"),
            ("DAIMONOS_AGENT_ALLOWED_COMMANDS", "an approval setting"),
            ("DAIMONOS_AGENT_DENIED_COMMANDS", "an approval setting"),
            ("DAIMONOS_AGENT_PERMISSION_MODE", "an approval setting"),
            ("DAIMONOS_AGENT_PERMISSION_LIST", "an approval setting"),
            ("HTTP_PROXY", "a proxy or certificate setting"),
            ("https_proxy", "a proxy or certificate setting"),
            ("All_Proxy", "a proxy or certificate setting"),
            ("no_proxy", "a proxy or certificate setting"),
            ("SSL_CERT_FILE", "a proxy or certificate setting"),
            ("SSL_CERT_DIR", "a proxy or certificate setting"),
            ("HOME", "a user directory setting"),
            ("XDG_CONFIG_HOME", "a user directory setting"),
            ("XDG_STATE_HOME", "a user directory setting"),
            ("XDG_DATA_HOME", "a user directory setting"),
            ("xdg_cache_home", "a user directory setting"),
            // Each class is compared without letter case.
            ("daimonos_agent_env", "the agent env file selector"),
            ("Daimonos_Agent_Base_Url", "a connection setting"),
            ("daimonos_agent_openai_api_key", "a connection setting"),
            ("daimonos_agent_approval_mode", "an approval setting"),
        ];
        for (key, class) in refused {
            assert_eq!(workspace_refusal(key), Some(class), "{key}");
        }
        for key in [
            "DAIMONOS_AGENT_MODEL",
            "DAIMONOS_AGENT_THINKING",
            "DAIMONOS_AGENT_COMPACTION",
            "DAIMONOS_SKILL_DIR",
            "DAIMONOS_KGL_AUTOINDEX",
            "MY_API_KEY",
            "SERVICE_BASE_URL",
            "PROXY",
            "TMPDIR",
            "XDG_RUNTIME_DIR",
        ] {
            assert_eq!(workspace_refusal(key), None, "{key}");
        }
    }

    /// The same lines are refused from the workspace file and set from the
    /// user file. A refused line never reaches `set`, and its value is not
    /// kept.
    #[test]
    fn the_plan_depends_on_the_source_of_the_file() {
        let content = "DAIMONOS_AGENT_BASE_URL=https://elsewhere.example\n\
                       DAIMONOS_AGENT_APPROVAL_MODE=auto\n\
                       http_proxy=http://proxy.example\n\
                       DAIMONOS_AGENT_MODEL=some/model\n\
                       LD_PRELOAD=x\n";
        let workspace = plan(content, Source::Workspace, |_| false);
        assert_eq!(
            workspace.set,
            vec![("DAIMONOS_AGENT_MODEL".to_string(), "some/model".to_string())]
        );
        assert_eq!(
            workspace.refused_untrusted,
            vec![
                (
                    "DAIMONOS_AGENT_BASE_URL".to_string(),
                    "a connection setting"
                ),
                (
                    "DAIMONOS_AGENT_APPROVAL_MODE".to_string(),
                    "an approval setting"
                ),
                ("http_proxy".to_string(), "a proxy or certificate setting"),
            ]
        );
        assert_eq!(workspace.refused, vec!["LD_PRELOAD".to_string()]);

        let user = plan(content, Source::User, |_| false);
        assert_eq!(user.set.len(), 4);
        assert!(user.refused_untrusted.is_empty());
        assert_eq!(user.refused, vec!["LD_PRELOAD".to_string()]);

        // A refused line is refused also when the environment has the variable.
        let present = plan(content, Source::Workspace, |_| true);
        assert_eq!(present.refused_untrusted.len(), 3);
        assert_eq!(
            present.skipped_present,
            vec!["DAIMONOS_AGENT_MODEL".to_string()]
        );
    }

    /// The refusal sees the name that `parse_line` returns: without `export`,
    /// without surrounding whitespace, and with the quotes of the value
    /// removed. So no spelling of a line gets a refused name past the check. A
    /// line that `parse_line` does not accept sets nothing.
    #[test]
    fn no_spelling_of_a_line_gets_a_refused_name_past_the_check() {
        let spellings = [
            "DAIMONOS_AGENT_BASE_URL=https://x.example",
            "export DAIMONOS_AGENT_BASE_URL=https://x.example",
            "  DAIMONOS_AGENT_BASE_URL  =  https://x.example  ",
            "\tDAIMONOS_AGENT_BASE_URL=https://x.example",
            "export   DAIMONOS_AGENT_BASE_URL=\"https://x.example\"",
            "DAIMONOS_AGENT_BASE_URL='https://x.example'",
            "DAIMONOS_AGENT_BASE_URL=",
            "daimonos_agent_base_url=https://x.example",
            "export HTTPS_PROXY=http://p.example",
            " http_proxy = \"http://p.example\"",
            "export DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "export DAIMONOS_AGENT_ENV=/x/agent.env",
            "export XDG_CONFIG_HOME=/x",
        ];
        for line in spellings {
            let outcome = plan(line, Source::Workspace, |_| false);
            assert!(outcome.set.is_empty(), "{line:?} set {:?}", outcome.set);
            assert_eq!(outcome.refused_untrusted.len(), 1, "{line:?}");
        }
        // Lines that are not assignments set nothing and are not reported.
        for line in [
            "export\tDAIMONOS_AGENT_BASE_URL=https://x.example",
            "DAIMONOS_AGENT_BASE_URL https://x.example",
            "DAIMONOS AGENT_BASE_URL=https://x.example",
            "# DAIMONOS_AGENT_BASE_URL=https://x.example",
        ] {
            let outcome = plan(line, Source::Workspace, |_| false);
            assert!(outcome.set.is_empty(), "{line:?} set {:?}", outcome.set);
        }
        // Each of several lines for one name is refused. None is set.
        let repeated = "DAIMONOS_AGENT_API_KEY=a\nMODEL=m\nexport DAIMONOS_AGENT_API_KEY=b\n";
        let outcome = plan(repeated, Source::Workspace, |_| false);
        assert_eq!(outcome.set, vec![("MODEL".to_string(), "m".to_string())]);
        assert_eq!(outcome.refused_untrusted.len(), 2);
    }
}
