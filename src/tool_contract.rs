//! WP15 (FR-13): the effective tool contract — one deterministic, session-free
//! view of the tools daimonos actually serves over MCP, plus a standalone
//! contract version.
//!
//! Context: docs/adr/029-effective-tool-contract.md and the reviewer-runtime
//! INTERFACES.md WP15 rows. ADR-029 requires that *equal inputs produce equal
//! served and exported definitions*, so this module does not maintain a second
//! handwritten tool list: it reuses the exact serving functions
//! ([`crate::tools::tool_definitions`], [`crate::tools::initial_exposed_tools`],
//! [`crate::tools::render_list_tool`]) and the same context/confinement filter
//! the MCP `tools/list` handler applies. The exported tool array therefore
//! equals the served array for the same inputs by construction (AC-13.1).
//!
//! The contract version is deliberately separate from the Cargo package version
//! (binary-version contract) and from WP12 source identity. A breaking change to
//! any tool's name, argument, or result shape bumps [`CONTRACT_VERSION`] and adds
//! an entry to `docs/tool-contract-changelog.md`.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::state_root::RunFailure;
use crate::tools;
use crate::verbosity::Verbosity;

/// The standalone tool-contract semantic version (ADR-029). Independent of the
/// Cargo package version and of WP12 source identity. Bump the major on any
/// breaking tool change (name/argument/result shape); minor for additive
/// compatible changes; patch for corrections. Every bump needs a
/// `docs/tool-contract-changelog.md` entry.
pub const CONTRACT_VERSION: &str = "1.1.0";

/// MCP initialization `_meta` key carrying [`CONTRACT_VERSION`] (AC-13.2). It is
/// distinct from the server-kind key and from `serverInfo.version` (the binary
/// version), so callers can pin the tool contract independently.
pub const CONTRACT_VERSION_META_KEY: &str = "io.daimonos/tool-contract-version";

/// The effective inputs that shape both serving and export (ADR-029). These are
/// the same values the MCP `tools/list` handler reads from the live session, so
/// a frozen catalog built from them reproduces serving exactly.
///
/// Integration note (for I): ADR-029 names the reserved shape
/// `freeze(definitions, policy, filesystem)`. Here `descriptions` carries the
/// definitions, `confined` is the filesystem policy signal
/// (`FilesystemAuthority`), and sibling options become additional fields as they
/// merge: WP06 `--tool-allowlist` (a `ToolPolicy` filter applied to both serving
/// and this dump), WP11 `--ignore-config-files` (changes the resolved config and
/// plugin discovery feeding these inputs), and WP17 `--full-tool-schemas`
/// (already captured by `full_tool_schemas`). WP04 read-only and WP14 no-follow
/// act at dispatch/open time and do not change the served list, so they are not
/// inputs here.
pub struct EffectiveInputs<'a> {
    pub workspace: &'a Path,
    pub descriptions: &'a crate::tool_descriptions::ToolDescriptions,
    pub verbosity: Verbosity,
    pub full_tool_schemas: bool,
    pub confined: bool,
    pub kgl_state_directory: Option<PathBuf>,
    // Keep the legacy export unchanged. Offline export uses the same caller-narrowed
    // catalog as listing and direct dispatch.
    pub offline_policy: Option<crate::run_policy::ToolPolicy>,
}

impl<'a> EffectiveInputs<'a> {
    /// Build the effective inputs from a resolved run `Config` and workspace,
    /// mirroring exactly what the MCP stdio server reads: `Session::new` copies
    /// `cfg.mcp.default_verbosity`; `tools/list` reads
    /// `config::effective_full_tool_schemas`; `passes_session_context_check`
    /// consults the filesystem authority and the KGL state directory.
    pub fn from_config(cfg: &'a Config, workspace: &'a Path) -> Self {
        Self {
            workspace,
            descriptions: &cfg.prompts.resolved_tool_descriptions,
            verbosity: cfg.mcp.default_verbosity,
            full_tool_schemas: crate::config::effective_full_tool_schemas(cfg),
            confined: cfg.filesystem_authority.is_some(),
            kgl_state_directory: cfg.kgl.state_directory.clone(),
            offline_policy: cfg
                .network
                .offline()
                .then(|| crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref())),
        }
    }
}

/// A frozen effective tool catalog: the exact tool set the MCP server would
/// serve for the inputs it was frozen from, captured once so serving, lazy
/// expansion, export, and schema lookup all read one source (ADR-029).
pub struct EffectiveToolCatalog {
    tools: Vec<rust_mcp_sdk::schema::Tool>,
    options: EffectiveOptions,
}

impl EffectiveToolCatalog {
    /// Freeze the effective catalog from the run's effective inputs.
    ///
    /// Reuses the serving pipeline verbatim — `tool_definitions`, the
    /// `initial_exposed_tools` set, the context/confinement filter, and
    /// `render_list_tool` — so the frozen array equals `tools/list` for these
    /// inputs. Returns `RunFailure` (exit 2, `run_configuration_invalid`) on an
    /// invalid selection; with no allowlist configured in this build there is no
    /// such failure, but the fallible shape is reserved for WP06.
    pub fn freeze(inputs: &EffectiveInputs<'_>) -> Result<Self, RunFailure> {
        let exposed = tools::initial_exposed_tools();
        let rendered: Vec<rust_mcp_sdk::schema::Tool> =
            tools::tool_definitions(inputs.descriptions)
                .into_iter()
                .filter(|t| {
                    inputs.offline_policy.as_ref().map_or_else(
                        || exposed.contains(&t.name),
                        |policy| policy.allows(&t.name),
                    )
                })
                .filter(|t| {
                    passes_effective_context_check(
                        &t.name,
                        inputs.workspace,
                        inputs.confined,
                        inputs.kgl_state_directory.as_deref(),
                    )
                })
                .map(|t| {
                    tools::render_list_tool(
                        t,
                        inputs.descriptions,
                        inputs.verbosity,
                        inputs.full_tool_schemas,
                        // The dump is a fresh view: no tool has been "used" yet, so
                        // progressive-exposure schema stripping matches a fresh
                        // session's first `tools/list`.
                        false,
                    )
                })
                .collect();

        // WP17 (FR-15): in full-schema mode every served/exported schema must be
        // a valid, self-contained, non-recursive Draft 2020-12 document, with no
        // cross-tool definition-name collision (ADR-030). Validate before the
        // dump is emitted or any model request is made; an unusable schema is a
        // `schema_unusable` run. The stripped/lazy default mode needs no such
        // gate (its placeholders are trivially self-contained), so this is
        // scoped to the opt-in mode and leaves default behavior unchanged.
        if inputs.full_tool_schemas {
            validate_self_contained(&rendered)?;
        }

        Ok(Self {
            tools: rendered,
            options: EffectiveOptions {
                workspace: inputs.workspace.display().to_string(),
                full_tool_schemas: inputs.full_tool_schemas,
                verbosity: inputs.verbosity.as_str().to_string(),
                confined: inputs.confined,
            },
        })
    }

    /// Render the deterministic, self-contained contract document (ADR-029).
    pub fn export(&self) -> ToolContractDocument {
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                // Serialize through the SDK type so each entry's schema is
                // byte-identical to what the server sends over `tools/list`.
                let value = serde_json::to_value(tool).unwrap_or(Value::Null);
                let name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let description = value
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input_schema = value.get("inputSchema").cloned().unwrap_or(Value::Null);
                let (tier, route) = registry_metadata(&name);
                ContractTool {
                    name,
                    description,
                    input_schema,
                    tier,
                    route,
                }
            })
            .collect();
        ToolContractDocument {
            contract_version: CONTRACT_VERSION,
            effective_options: self.options.clone(),
            tools,
        }
    }

    /// Serialize the exported document as deterministic pretty JSON. `serde_json`
    /// orders `Value` object keys lexically (no `preserve_order` feature), and
    /// struct fields serialize in declaration order, so the output is stable
    /// across runs of the same build (AC-13.1).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.export()).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Mirror of `mcp::passes_session_context_check` without a live `Session`: a
/// confined run hides tools refused by `path_policy::tool_allowed`; the KGL
/// probe tools require their state directory to exist; everything else honors
/// the registry context check for the workspace.
fn passes_effective_context_check(
    name: &str,
    workspace: &Path,
    confined: bool,
    kgl_state_directory: Option<&Path>,
) -> bool {
    if confined && !crate::path_policy::tool_allowed(name) {
        return false;
    }
    if matches!(name, "kgl_query" | "kgl_assert") {
        if let Some(directory) = kgl_state_directory {
            return directory.exists();
        }
    }
    tools::passes_context_check(name, workspace)
}

/// Declared ownership/route metadata for a tool, derived from the registry (not
/// a second handwritten list). `route` is `opcode` when the tool has a protocol
/// opcode mapping, else `native` (plugin/meta/special handling).
fn registry_metadata(name: &str) -> (&'static str, &'static str) {
    for tool in tools::all_tools() {
        if tool.name == name {
            let tier = match tool.tier {
                tools::ToolTier::Full => "full",
                tools::ToolTier::Terse => "terse",
                tools::ToolTier::OnDemand => "on_demand",
                tools::ToolTier::AgentOnly => "agent_only",
                tools::ToolTier::McpOnly => "mcp_only",
            };
            let route = if tool.to_request.is_some() {
                "opcode"
            } else {
                "native"
            };
            return (tier, route);
        }
    }
    ("unknown", "unknown")
}

/// WP17 (FR-15): the static full-schema gate (ADR-030). Validate every served
/// schema offline — valid self-contained Draft 2020-12, no external reference,
/// no recursive cycle, and no cross-tool definition-name collision — before any
/// model request, MCP full serving, or export. A failure is a
/// `run_configuration_invalid` run with reason `schema_unusable` (exit 2),
/// detected without touching the network or filesystem. A schema that is valid
/// and self-contained but which a particular endpoint rejects is NOT refused
/// here: that surfaces at the capability probe as `model_unsupported` (WP09).
///
/// Reuses the serialized SDK `Tool` so the validated schema is byte-identical to
/// what the server sends over `tools/list` and the dump exports.
pub fn validate_self_contained(tools: &[rust_mcp_sdk::schema::Tool]) -> Result<(), RunFailure> {
    let schemas: Vec<(String, Value)> = tools
        .iter()
        .map(|tool| {
            let value = serde_json::to_value(tool).unwrap_or(Value::Null);
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let schema = value.get("inputSchema").cloned().unwrap_or(Value::Null);
            (name, schema)
        })
        .collect();
    // Native/served tools are OWNED definitions (S3 / D39): they participate in
    // the cross-tool definition-name collision namespace.
    let entries: Vec<crate::schema_contract::CatalogSchema> = schemas
        .iter()
        .map(|(name, schema)| crate::schema_contract::CatalogSchema {
            tool: name.as_str(),
            schema,
            owned: true,
        })
        .collect();
    crate::schema_contract::ValidatedToolSchema::validate_catalog(&entries)
        .map(|_| ())
        .map_err(|err| RunFailure::configuration_invalid(err.reason(), err.to_string()))
}

/// The effective inputs that shaped the catalog, echoed into the dump so a
/// reader can see which workspace/config/policy/schema-mode produced it.
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveOptions {
    pub workspace: String,
    pub full_tool_schemas: bool,
    pub verbosity: String,
    pub confined: bool,
}

/// One tool in the contract: its effective name, description, and self-contained
/// input schema, plus registry-derived ownership/route metadata (ADR-029).
#[derive(Debug, Clone, Serialize)]
pub struct ContractTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub tier: &'static str,
    pub route: &'static str,
}

/// The deterministic tool-contract document (ADR-029 reserved shape:
/// `contract_version`, `tools`, `effective_options`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolContractDocument {
    pub contract_version: &'static str,
    pub effective_options: EffectiveOptions,
    pub tools: Vec<ContractTool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs_for<'a>(
        cfg: &'a Config,
        descriptions: &'a crate::tool_descriptions::ToolDescriptions,
        workspace: &'a Path,
    ) -> EffectiveInputs<'a> {
        EffectiveInputs {
            workspace,
            descriptions,
            verbosity: cfg.mcp.default_verbosity,
            full_tool_schemas: false,
            confined: false,
            kgl_state_directory: None,
            offline_policy: None,
        }
    }

    #[test]
    fn export_has_version_and_nonempty_tools() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        let inputs = inputs_for(&cfg, &descriptions, dir.path());
        let catalog = EffectiveToolCatalog::freeze(&inputs).unwrap();
        let doc = catalog.export();
        assert_eq!(doc.contract_version, "1.1.0");
        assert!(!doc.tools.is_empty());
        for tool in &doc.tools {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
            assert!(tool.input_schema.is_object());
        }
    }

    #[test]
    fn to_json_is_stable_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        let inputs = inputs_for(&cfg, &descriptions, dir.path());
        let catalog = EffectiveToolCatalog::freeze(&inputs).unwrap();
        assert_eq!(catalog.to_json(), catalog.to_json());
    }

    #[test]
    fn git_tool_tracks_workspace_context() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();

        let inputs = inputs_for(&cfg, &descriptions, dir.path());
        let without = EffectiveToolCatalog::freeze(&inputs).unwrap().export();
        assert!(!without.tools.iter().any(|t| t.name == "git"));

        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let inputs = inputs_for(&cfg, &descriptions, dir.path());
        let with = EffectiveToolCatalog::freeze(&inputs).unwrap().export();
        assert!(with.tools.iter().any(|t| t.name == "git"));
    }

    #[test]
    fn from_config_mirrors_server_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.prompts.resolved_tool_descriptions =
            crate::tool_descriptions::ToolDescriptions::default();
        let inputs = EffectiveInputs::from_config(&cfg, dir.path());
        assert_eq!(inputs.verbosity, cfg.mcp.default_verbosity);
        assert!(!inputs.confined);
    }

    /// WP17 (FR-15, AC-15.1): every schema the full-schema catalog serves is a
    /// valid, self-contained, non-recursive Draft 2020-12 document. Proven here
    /// by the authoritative ADR-024 validator over the real native catalog, so
    /// `freeze` in full mode succeeds rather than refusing a native schema.
    #[test]
    fn full_schema_freeze_passes_the_static_gate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let cfg = Config::default();
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        let mut inputs = inputs_for(&cfg, &descriptions, dir.path());
        inputs.full_tool_schemas = true;
        let catalog = EffectiveToolCatalog::freeze(&inputs).expect("full-schema freeze");
        validate_self_contained(&catalog.tools).expect("native full schemas are self-contained");
    }

    /// WP17 (FR-15): a tool schema that is not self-contained is refused before
    /// any model request with run reason `schema_unusable` (PM A to Q-FR-15-2),
    /// not surfaced as a serving-time panic or silently served.
    #[test]
    fn unusable_tool_schema_is_schema_unusable() {
        let tool: rust_mcp_sdk::schema::Tool = serde_json::from_value(serde_json::json!({
            "name": "leaky",
            "description": "external ref",
            "inputSchema": {
                "type": "object",
                "properties": { "x": { "$ref": "https://example.com/remote.json" } }
            }
        }))
        .unwrap();
        let err = validate_self_contained(&[tool]).expect_err("external ref must be refused");
        assert_eq!(err.code, "schema_unusable");
    }
}
