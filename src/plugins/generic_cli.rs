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
