pub mod cargo;
pub mod curl;
pub mod discord;
pub mod docker;
pub mod generic_cli;
pub mod gh;
pub mod git;
pub mod npm;
pub mod pytest;
pub mod shellcheck;
pub mod x07;

use std::sync::Arc;

use crate::config::Config;
use crate::tool_runner::{ToolPlugin, ToolRegistry};

/// One built-in CLI plugin's registration entry: its id, an availability probe
/// (is the underlying CLI on PATH?), and a constructor for the plugin object.
type BuiltinPlugin = (&'static str, fn() -> bool, fn() -> Arc<dyn ToolPlugin>);

/// The canonical set of built-in CLI tool plugins, as (id, is_available,
/// constructor) triples. This is the SINGLE source of truth for which
/// built-in plugin tools daimonos exposes. Every front-door (MCP, agent, ACP)
/// provisions its tool registry through [`register_builtin_plugins`], so a
/// tool added here is automatically available in all three modes — making a
/// tool mode-specific requires deliberate extra work, not the reverse
/// (vikunja: unified tool provisioning).
///
/// `discord` is intentionally NOT here: it always registers (no availability
/// gate) and needs `cfg.discord`, so it is handled directly in
/// [`register_builtin_plugins`]. The invariant test asserts every other
/// `plugins::*` CLI plugin module appears in this table.
fn builtin_cli_plugins() -> Vec<BuiltinPlugin> {
    vec![
        ("git", git::is_available, || Arc::new(git::GitPlugin::new())),
        ("docker", docker::is_available, || {
            Arc::new(docker::DockerPlugin::new())
        }),
        ("cargo", cargo::is_available, || {
            Arc::new(cargo::CargoPlugin::new())
        }),
        ("gh", gh::is_available, || Arc::new(gh::GhPlugin::new())),
        ("pytest", pytest::is_available, || {
            Arc::new(pytest::PytestPlugin::new())
        }),
        ("curl", curl::is_available, || {
            Arc::new(curl::CurlPlugin::new())
        }),
        ("shellcheck", shellcheck::is_available, || {
            Arc::new(shellcheck::ShellcheckPlugin::new())
        }),
        ("npm", npm::is_available, || Arc::new(npm::NpmPlugin::new())),
    ]
}

/// Build the complete tool registry every front-door should use: the
/// config-driven plugins (via [`crate::config::register_tools`]) plus the
/// built-in CLI plugins ([`builtin_cli_plugins`]) that pass their availability
/// check, plus the always-on discord plugin. This is the one place the tool
/// set is assembled; MCP, agent, and ACP all call it, so they can never drift
/// out of sync on which tools exist.
pub async fn register_builtin_plugins(cfg: &Config, registry: &ToolRegistry, quiet_stderr: bool) {
    // Config-driven / generic-CLI plugins first (x07, user-declared tools).
    crate::config::register_tools(cfg, registry, quiet_stderr).await;

    for (id, available, make) in builtin_cli_plugins() {
        if available() {
            registry.register(make()).await;
            if !quiet_stderr {
                eprintln!("auto-registered {id} tool plugin");
            }
        }
    }

    // Discord always registers (no availability gate); needs config.
    registry
        .register(Arc::new(discord::DiscordPlugin::new(cfg.discord.clone())))
        .await;
    if !quiet_stderr {
        eprintln!("auto-registered discord tool plugin");
    }
}

/// Largest char boundary `<= max` in `s`. Call this before byte-slicing
/// `&s[..n]` when capping tool output: `String::from_utf8_lossy` output can
/// contain multi-byte UTF-8, and a raw byte offset that lands mid-character
/// panics. `std::str::floor_char_boundary` would do this but is unstable on the
/// pinned stable toolchain, so we implement it here.
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::{builtin_cli_plugins, floor_char_boundary, register_builtin_plugins};
    use crate::config::Config;
    use crate::tool_runner::ToolRegistry;
    use std::collections::BTreeSet;

    #[tokio::test]
    async fn every_builtin_cli_module_is_in_the_canonical_registry() {
        let source_modules: BTreeSet<_> = include_str!("mod.rs")
            .lines()
            .filter_map(|line| line.strip_prefix("pub mod "))
            .filter_map(|line| line.strip_suffix(';'))
            .filter(|module| !matches!(*module, "discord" | "generic_cli" | "x07"))
            .collect();
        let table_modules: BTreeSet<_> = builtin_cli_plugins()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(
            table_modules, source_modules,
            "every built-in CLI plugin module must be added to builtin_cli_plugins()"
        );

        let registry = ToolRegistry::new();
        register_builtin_plugins(&Config::default(), &registry, true).await;
        for (id, available, _) in builtin_cli_plugins() {
            assert_eq!(
                registry.get(id).await.is_some(),
                available(),
                "canonical registration must follow {id}'s availability probe"
            );
        }
        assert!(registry.get("discord").await.is_some());
    }

    #[test]
    fn floor_char_boundary_never_splits_a_char() {
        // "€" is 3 bytes (E2 82 AC). A cap at byte 1 or 2 must floor to 0.
        let s = "€uro";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3); // boundary after '€'
        assert!(s.is_char_boundary(floor_char_boundary(s, 2)));
        // Slicing at the returned offset must not panic.
        let _ = &s[..floor_char_boundary(s, 2)];
    }

    #[test]
    fn floor_char_boundary_caps_at_len() {
        let s = "abc";
        assert_eq!(floor_char_boundary(s, 99), 3);
        assert_eq!(floor_char_boundary(s, 3), 3);
    }
}
