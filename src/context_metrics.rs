//! Metadata-only context composition measurement for agent generations.

use crate::providers::{ContentBlock, Context, Role};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ContextComposition {
    pub messages: usize,
    pub tools_exposed: usize,
    pub stable_prefix_len: usize,
    pub system_bytes: usize,
    pub tool_name_bytes: usize,
    pub tool_description_bytes: usize,
    pub tool_schema_bytes: usize,
    pub user_text_bytes: usize,
    pub assistant_text_bytes: usize,
    pub thinking_bytes: usize,
    pub provider_state_bytes: usize,
    pub tool_call_argument_bytes: usize,
    pub tool_result_ok_bytes: usize,
    pub tool_result_error_bytes: usize,
    pub image_bytes: usize,
    pub image_count: usize,
    /// Sum of neutral content categories only; excludes provider wire framing.
    pub payload_bytes: usize,
    /// Coarse `ceil(payload_bytes / 4)` estimate, not tokenizer occupancy.
    pub payload_tokens_est: u64,
}

impl ContextComposition {
    fn finish(mut self) -> Self {
        self.payload_bytes = [
            self.system_bytes,
            self.tool_name_bytes,
            self.tool_description_bytes,
            self.tool_schema_bytes,
            self.user_text_bytes,
            self.assistant_text_bytes,
            self.thinking_bytes,
            self.provider_state_bytes,
            self.tool_call_argument_bytes,
            self.tool_result_ok_bytes,
            self.tool_result_error_bytes,
            self.image_bytes,
        ]
        .into_iter()
        .fold(0usize, usize::saturating_add);
        self.payload_tokens_est = crate::analytics::estimate_tokens(self.payload_bytes);
        self
    }
}

pub fn measure_context(context: &Context) -> ContextComposition {
    let mut composition = ContextComposition {
        messages: context.messages.len(),
        tools_exposed: context.tools.len(),
        stable_prefix_len: context.stable_prefix_len,
        system_bytes: context.system.as_ref().map_or(0, String::len),
        ..ContextComposition::default()
    };

    for tool in &context.tools {
        composition.tool_name_bytes = composition.tool_name_bytes.saturating_add(tool.name.len());
        composition.tool_description_bytes = composition
            .tool_description_bytes
            .saturating_add(tool.description.len());
        composition.tool_schema_bytes = composition.tool_schema_bytes.saturating_add(
            serde_json::to_vec(&tool.input_schema)
                .map(|serialized| serialized.len())
                .unwrap_or(0),
        );
    }

    for message in &context.messages {
        for block in &message.content {
            match block {
                ContentBlock::Text(text) => match message.role {
                    Role::User => {
                        composition.user_text_bytes =
                            composition.user_text_bytes.saturating_add(text.len());
                    }
                    Role::Assistant => {
                        composition.assistant_text_bytes =
                            composition.assistant_text_bytes.saturating_add(text.len());
                    }
                },
                ContentBlock::Image { data, .. } => {
                    composition.image_bytes = composition.image_bytes.saturating_add(data.len());
                    composition.image_count = composition.image_count.saturating_add(1);
                }
                ContentBlock::Thinking(text) => {
                    composition.thinking_bytes =
                        composition.thinking_bytes.saturating_add(text.len());
                }
                ContentBlock::ProviderState { data, .. } => {
                    composition.provider_state_bytes =
                        composition.provider_state_bytes.saturating_add(
                            serde_json::to_vec(data)
                                .map(|serialized| serialized.len())
                                .unwrap_or(0),
                        );
                }
                ContentBlock::ToolCall { input, .. } => {
                    composition.tool_call_argument_bytes =
                        composition.tool_call_argument_bytes.saturating_add(
                            serde_json::to_vec(input)
                                .map(|serialized| serialized.len())
                                .unwrap_or(0),
                        );
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let target = if *is_error {
                        &mut composition.tool_result_error_bytes
                    } else {
                        &mut composition.tool_result_ok_bytes
                    };
                    *target = target.saturating_add(content.len());
                }
            }
        }
    }

    composition.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ContentBlock, Context, Message, Role, ToolSchema};
    use serde_json::json;

    #[test]
    fn composition_counts_each_neutral_context_category_without_content() {
        let context = Context {
            system: Some("SYSTEM_SECRET".into()),
            tools: vec![ToolSchema {
                name: "TOOL_NAME_SECRET".into(),
                description: "TOOL_DESCRIPTION_SECRET".into(),
                input_schema: json!({"type":"object"}),
            }],
            stable_prefix_len: 1,
            messages: vec![
                Message::user("USER_SECRET"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text("ASSISTANT_SECRET".into()),
                        ContentBlock::Thinking("THINKING_SECRET".into()),
                        ContentBlock::ProviderState {
                            provider: "provider".into(),
                            data: json!({"opaque":"PROVIDER_SECRET"}),
                        },
                        ContentBlock::ToolCall {
                            id: "call".into(),
                            name: "read_file".into(),
                            input: json!({"path":"ARG_SECRET"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call".into(),
                            content: "RESULT_OK_SECRET".into(),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "failed".into(),
                            content: "RESULT_ERROR_SECRET".into(),
                            is_error: true,
                        },
                        ContentBlock::Image {
                            data: "IMAGE_SECRET".into(),
                            media_type: "image/png".into(),
                            uri: Some("file:///PATH_SECRET".into()),
                        },
                    ],
                },
            ],
        };

        let composition = measure_context(&context);

        assert_eq!(composition.messages, 3);
        assert_eq!(composition.tools_exposed, 1);
        assert_eq!(composition.stable_prefix_len, 1);
        assert_eq!(composition.system_bytes, "SYSTEM_SECRET".len());
        assert_eq!(composition.tool_name_bytes, "TOOL_NAME_SECRET".len());
        assert_eq!(
            composition.tool_description_bytes,
            "TOOL_DESCRIPTION_SECRET".len()
        );
        assert_eq!(composition.tool_schema_bytes, 17);
        assert_eq!(composition.user_text_bytes, "USER_SECRET".len());
        assert_eq!(composition.assistant_text_bytes, "ASSISTANT_SECRET".len());
        assert_eq!(composition.thinking_bytes, "THINKING_SECRET".len());
        assert_eq!(composition.provider_state_bytes, 28);
        assert_eq!(composition.tool_call_argument_bytes, 21);
        assert_eq!(composition.tool_result_ok_bytes, "RESULT_OK_SECRET".len());
        assert_eq!(
            composition.tool_result_error_bytes,
            "RESULT_ERROR_SECRET".len()
        );
        assert_eq!(composition.image_bytes, "IMAGE_SECRET".len());
        assert_eq!(composition.image_count, 1);

        let rendered = serde_json::to_string(&composition).unwrap();
        for secret in [
            "SYSTEM_SECRET",
            "TOOL_NAME_SECRET",
            "TOOL_DESCRIPTION_SECRET",
            "USER_SECRET",
            "ASSISTANT_SECRET",
            "THINKING_SECRET",
            "PROVIDER_SECRET",
            "ARG_SECRET",
            "RESULT_OK_SECRET",
            "RESULT_ERROR_SECRET",
            "IMAGE_SECRET",
            "file:///PATH_SECRET",
        ] {
            assert!(!rendered.contains(secret));
        }
    }
}
