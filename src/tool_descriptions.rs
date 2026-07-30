//! Runtime-configurable tool and parameter descriptions.

use serde::Deserialize;
use std::collections::HashMap;

pub const DEFAULT_TEXT: &str = include_str!("../prompts/tool_descriptions.toml");

/// Migration sentinel: the number of model-facing parameter strings moved
/// from structural Rust schemas into the embedded catalog by #1007. Kept as
/// an independent literal (rather than derived from the catalog) so accidental
/// deletion fails parity tests. Future intentional additions/removals update
/// this single value.
#[cfg(test)]
pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 136;
#[cfg(test)]
pub(crate) const AGENT_ONLY_PARAMETER_DESCRIPTION_COUNT: usize = 1;
#[cfg(test)]
pub(crate) const MCP_PARAMETER_DESCRIPTION_COUNT: usize =
    DEFAULT_PARAMETER_DESCRIPTION_COUNT - AGENT_ONLY_PARAMETER_DESCRIPTION_COUNT;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct DescriptionEntry {
    full: Option<String>,
    terse: Option<String>,
    parameters: HashMap<String, String>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone)]
pub struct ToolDescriptions {
    entries: HashMap<String, DescriptionEntry>,
}

impl Default for ToolDescriptions {
    fn default() -> Self {
        Self {
            entries: parse(DEFAULT_TEXT)
                .expect("embedded prompts/tool_descriptions.toml must be valid TOML"),
        }
    }
}

impl ToolDescriptions {
    pub fn full(&self, name: &str) -> Option<&str> {
        self.entries.get(name)?.full.as_deref()
    }

    pub fn terse(&self, name: &str) -> Option<&str> {
        self.entries.get(name)?.terse.as_deref()
    }

    #[cfg(test)]
    fn parameter(&self, tool: &str, parameter: &str) -> Option<&str> {
        self.entries
            .get(tool)?
            .parameters
            .get(parameter)
            .map(String::as_str)
    }

    /// Clone a tool's structural schema and inject the resolved parameter
    /// descriptions into its top-level `properties` entries. Schema shape,
    /// validation keywords, and lazy exposure policy remain owned by
    /// `tools.rs`; only model-facing text comes from this catalog (#1007).
    pub fn schema_with_parameters(
        &self,
        tool: &str,
        schema: &serde_json::Value,
    ) -> serde_json::Value {
        let mut schema = schema.clone();
        let Some(parameters) = self.entries.get(tool).map(|entry| &entry.parameters) else {
            return schema;
        };
        let Some(properties) = schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return schema;
        };
        for (name, description) in parameters {
            let Some(property) = properties
                .get_mut(name)
                .and_then(serde_json::Value::as_object_mut)
            else {
                continue;
            };
            property.insert(
                "description".to_string(),
                serde_json::Value::String(description.clone()),
            );
        }
        schema
    }

    /// Full description with a visible, nonempty fallback if code/catalog
    /// parity ever drifts despite the invariant tests.
    pub fn full_or_name<'a>(&'a self, name: &'a str) -> &'a str {
        match self.full(name) {
            Some(description) => description,
            None => {
                eprintln!(
                    "daimonos: no full description registered for tool '{name}'; using tool name"
                );
                name
            }
        }
    }

    /// Load and overlay a user catalog. Missing entries keep embedded defaults;
    /// unknown tools and empty values are ignored with a warning.
    pub async fn load(override_path: Option<&str>) -> Self {
        let mut descriptions = Self::default();
        let Some(path) = override_path.filter(|path| !path.trim().is_empty()) else {
            return descriptions;
        };
        let text = match tokio::fs::read_to_string(crate::paths::expand_tilde(path)).await {
            Ok(text) => text,
            Err(error) => {
                eprintln!(
                    "daimonos: tool description override ({path}) unreadable: {error}; using embedded defaults"
                );
                return descriptions;
            }
        };
        let overrides = match parse(&text) {
            Ok(overrides) => overrides,
            Err(error) => {
                eprintln!(
                    "daimonos: tool description override ({path}) invalid: {error}; using embedded defaults"
                );
                return descriptions;
            }
        };

        for (name, entry) in overrides {
            let Some(default) = descriptions.entries.get_mut(&name) else {
                eprintln!(
                    "daimonos: tool description override contains unknown tool '{name}'; ignoring"
                );
                continue;
            };
            for variant in entry.extra.keys() {
                eprintln!(
                    "daimonos: unknown description variant '{variant}' for tool '{name}'; ignoring"
                );
            }
            merge_value(&name, "full", &mut default.full, entry.full);
            merge_value(&name, "terse", &mut default.terse, entry.terse);
            for (parameter, value) in entry.parameters {
                let Some(target) = default.parameters.get_mut(&parameter) else {
                    eprintln!(
                        "daimonos: tool description override contains unknown parameter '{parameter}' for tool '{name}'; ignoring"
                    );
                    continue;
                };
                if value.trim().is_empty() {
                    eprintln!(
                        "daimonos: empty parameter description for '{name}.{parameter}'; ignoring"
                    );
                    continue;
                }
                *target = value;
            }
        }
        descriptions
    }
}

fn parse(text: &str) -> Result<HashMap<String, DescriptionEntry>, toml::de::Error> {
    toml::from_str(text)
}

fn merge_value(name: &str, variant: &str, target: &mut Option<String>, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    if value.trim().is_empty() {
        eprintln!("daimonos: empty {variant} description for tool '{name}'; ignoring");
        return;
    }
    *target = Some(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_covers_all_tools_and_terse_variants() {
        let catalog = ToolDescriptions::default();
        let tools = crate::tools::all_tools();
        let names: Vec<_> = tools.iter().map(|tool| tool.name).collect();
        assert_eq!(catalog.entries.len(), names.len());
        for name in &names {
            assert!(
                catalog.full(name).is_some_and(|text| !text.is_empty()),
                "missing full description for {name}"
            );
        }
        assert!(catalog
            .entries
            .keys()
            .all(|name| names.contains(&name.as_str())));

        let parameter_count: usize = catalog
            .entries
            .values()
            .map(|entry| entry.parameters.len())
            .sum();
        assert_eq!(
            parameter_count, DEFAULT_PARAMETER_DESCRIPTION_COUNT,
            "all migrated parameter descriptions must remain in the embedded catalog"
        );
        for (tool_name, entry) in &catalog.entries {
            let tool = tools
                .iter()
                .find(|tool| tool.name == tool_name)
                .expect("catalog tool must exist");
            let properties = tool
                .schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .expect("tool schema must have properties");
            for (parameter, description) in &entry.parameters {
                assert!(
                    properties.contains_key(parameter),
                    "catalog parameter {tool_name}.{parameter} is absent from schema"
                );
                assert!(
                    !description.trim().is_empty(),
                    "catalog parameter {tool_name}.{parameter} has empty text"
                );
            }
        }
    }

    #[test]
    fn execute_script_description_prevents_python_dialect_retries() {
        let catalog = ToolDescriptions::default();
        let description = catalog
            .full("execute_script")
            .expect("execute_script description");
        assert!(description.contains("no `import`"));
        assert!(description.contains("top-level `for`"));
        assert!(description.contains("def main()"));
        assert!(description.contains("list_tool_signatures"));
    }

    #[tokio::test]
    async fn partial_override_merges_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.toml");
        tokio::fs::write(
            &path,
            "[read_file]\nfull = \"Custom read\"\nterse = \"Read custom\"\n\n[read_file.parameters]\npath = \"Custom path\"\noffset = \"   \"\nunknown_parameter = \"ignored\"\n\n[unknown]\nfull = \"ignored\"\nfuture_variant = \"also ignored\"\n",
        )
        .await
        .unwrap();

        let catalog = ToolDescriptions::load(Some(path.to_string_lossy().as_ref())).await;
        assert_eq!(catalog.full("read_file"), Some("Custom read"));
        assert_eq!(catalog.terse("read_file"), Some("Read custom"));
        assert_eq!(catalog.parameter("read_file", "path"), Some("Custom path"));
        assert_eq!(
            catalog.parameter("read_file", "offset"),
            ToolDescriptions::default().parameter("read_file", "offset")
        );
        assert!(catalog
            .parameter("read_file", "unknown_parameter")
            .is_none());
        assert_eq!(
            catalog.full("write_file"),
            ToolDescriptions::default().full("write_file")
        );
        assert!(catalog.full("unknown").is_none());
    }

    #[tokio::test]
    async fn malformed_override_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.toml");
        tokio::fs::write(&path, "[read_file\n").await.unwrap();

        let catalog = ToolDescriptions::load(Some(path.to_string_lossy().as_ref())).await;
        assert_eq!(
            catalog.full("read_file"),
            ToolDescriptions::default().full("read_file")
        );
    }

    #[test]
    fn missing_description_falls_back_to_tool_name() {
        let catalog = ToolDescriptions {
            entries: HashMap::new(),
        };
        assert_eq!(catalog.full_or_name("future_tool"), "future_tool");
    }

    #[test]
    fn schema_with_parameters_injects_text_without_changing_shape() {
        let catalog = ToolDescriptions::default();
        let base = crate::tools::all_tools()
            .into_iter()
            .find(|tool| tool.name == "read_file")
            .unwrap()
            .schema;
        assert!(base["properties"]["path"].get("description").is_none());

        let rendered = catalog.schema_with_parameters("read_file", &base);

        assert_eq!(
            rendered["properties"]["path"]["description"],
            "Relative path"
        );
        assert_eq!(rendered["properties"]["path"]["type"], "string");
        assert_eq!(rendered["required"], serde_json::json!(["path"]));
    }
}
