use crate::tool_runner::{ToolDescriptor, ToolPlugin};

/// Generic CLI plugin for any tool with structured JSON output.
/// No language-specific optimizations -- just runs the registered commands
/// and parses JSON output.
pub struct GenericCliPlugin {
    descriptor: ToolDescriptor,
}

impl GenericCliPlugin {
    pub fn new(descriptor: ToolDescriptor) -> Self {
        Self { descriptor }
    }
}

#[async_trait::async_trait]
impl ToolPlugin for GenericCliPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_runner::{ToolCommand, ToolPlugin};
    use std::collections::HashMap;

    fn make_descriptor() -> ToolDescriptor {
        let mut commands = HashMap::new();
        commands.insert(
            "build".into(),
            ToolCommand {
                bin: "cargo".into(),
                args: vec!["build".into()],
                output: "json".into(),
            },
        );
        commands.insert(
            "test".into(),
            ToolCommand {
                bin: "cargo".into(),
                args: vec!["test".into()],
                output: "text".into(),
            },
        );
        ToolDescriptor {
            id: "rust".into(),
            commands,
            source_pattern: Some("**/*.rs".into()),
            manifest: Some("Cargo.toml".into()),
            diagnostics_format: "json".into(),
            supports_quickfix: false,
            quickfix_format: None,
        }
    }

    #[test]
    fn new_stores_descriptor() {
        let desc = make_descriptor();
        let plugin = GenericCliPlugin::new(desc.clone());
        assert_eq!(plugin.descriptor().id, "rust");
        assert_eq!(plugin.descriptor().commands.len(), 2);
    }

    #[test]
    fn descriptor_round_trip() {
        let desc = make_descriptor();
        let plugin = GenericCliPlugin::new(desc);
        let d = plugin.descriptor();
        assert_eq!(d.source_pattern.as_deref(), Some("**/*.rs"));
        assert_eq!(d.manifest.as_deref(), Some("Cargo.toml"));
        assert!(!d.supports_quickfix);
    }

    #[test]
    fn no_quickfixes_by_default() {
        let desc = make_descriptor();
        let plugin = GenericCliPlugin::new(desc);
        let fixes = plugin.extract_quickfixes(&serde_json::json!({"diagnostics": []}));
        assert!(fixes.is_empty());
    }
}
