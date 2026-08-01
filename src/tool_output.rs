//! Model-visible tool-output bounding and deterministic intra-turn pruning.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::io::AsyncWriteExt;

use crate::config::ToolOutputConfig;
use crate::providers::{ContentBlock, Message};

const TRUNCATED_MARKER: &str = "truncated; full output saved to";
const PRUNED_MARKER: &str = "old tool result pruned; full output saved to";
const ARGUMENT_MARKER: &str = "...(argument truncated)";

#[derive(Debug)]
pub struct BoundText {
    pub content: String,
    pub output_path: Option<PathBuf>,
    pub original_chars: usize,
    pub visible_chars: usize,
}

#[derive(Debug, Default)]
pub struct MicrocompactStats {
    pub results_pruned: usize,
    pub arguments_pruned: usize,
    pub estimated_tokens_saved: u64,
    pub evicted_read_paths: Vec<PathBuf>,
    pub clear_read_cache: bool,
}

impl ToolOutputConfig {
    pub fn resolved_directory(&self) -> PathBuf {
        self.directory
            .as_deref()
            .map(crate::paths::expand_tilde)
            .or_else(|| crate::paths::home_dir().map(|home| home.join(".daimonos/tool-output")))
            .unwrap_or_else(|| PathBuf::from("/tmp/daimonos-tool-output"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_bytes < 256 {
            return Err("tool_output.max_bytes must be at least 256".to_string());
        }
        if self.max_lines < 3 {
            return Err("tool_output.max_lines must be at least 3".to_string());
        }
        if self.retention_days == 0 {
            return Err("tool_output.retention_days must be greater than zero".to_string());
        }
        if self.intra_turn_result_budget_tokens == 0 {
            return Err(
                "tool_output.intra_turn_result_budget_tokens must be greater than zero".to_string(),
            );
        }
        if self.intra_turn_keep_recent_results == 0 {
            return Err(
                "tool_output.intra_turn_keep_recent_results must be greater than zero".to_string(),
            );
        }
        if self.old_argument_max_chars < 20 {
            return Err("tool_output.old_argument_max_chars must be at least 20".to_string());
        }
        Ok(())
    }
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|byte| *byte == b'\n').count() + 1
    }
}

fn floor_char_boundary(text: &str, max: usize) -> usize {
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn take_prefix_bytes(text: &str, max: usize) -> &str {
    &text[..floor_char_boundary(text, max)]
}

fn take_suffix_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn preview(text: &str, marker: &str, max_lines: usize, max_bytes: usize) -> String {
    let marker = take_prefix_bytes(marker, max_bytes);
    if max_lines <= 1 || marker.len() + 2 >= max_bytes {
        return marker.to_string();
    }

    let lines: Vec<&str> = text.lines().collect();
    let data_lines = max_lines.saturating_sub(1);
    let head_lines = data_lines.div_ceil(2);
    let tail_lines = data_lines / 2;
    let head = lines
        .iter()
        .take(head_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let tail = lines
        .iter()
        .skip(lines.len().saturating_sub(tail_lines))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");

    let separators = if tail.is_empty() { 1 } else { 2 };
    let available = max_bytes.saturating_sub(marker.len() + separators);
    let head_budget = available.div_ceil(2);
    let tail_budget = available / 2;
    let head = take_prefix_bytes(&head, head_budget);
    let tail = take_suffix_bytes(&tail, tail_budget);
    if tail.is_empty() {
        format!("{head}\n{marker}")
    } else {
        format!("{head}\n{marker}\n{tail}")
    }
}

fn longest_json_string_len(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => {
            items.iter().map(longest_json_string_len).max().unwrap_or(0)
        }
        serde_json::Value::Object(map) => {
            map.values().map(longest_json_string_len).max().unwrap_or(0)
        }
        _ => 0,
    }
}

fn json_contains_path(value: &serde_json::Value, output_path: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(output_path),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|item| json_contains_path(item, output_path)),
        serde_json::Value::Object(map) => map
            .values()
            .any(|item| json_contains_path(item, output_path)),
        _ => false,
    }
}

fn truncate_first_json_string(
    value: &mut serde_json::Value,
    longest: usize,
    target_bytes: usize,
    marker: &str,
    max_lines: usize,
) -> bool {
    match value {
        serde_json::Value::String(text) if text.len() == longest => {
            let replacement = preview(text, marker, max_lines, target_bytes);
            if replacement.len() >= text.len() {
                return false;
            }
            *text = replacement;
            true
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .any(|item| truncate_first_json_string(item, longest, target_bytes, marker, max_lines)),
        serde_json::Value::Object(map) => map
            .values_mut()
            .any(|item| truncate_first_json_string(item, longest, target_bytes, marker, max_lines)),
        _ => false,
    }
}

fn structured_preview(
    mut value: serde_json::Value,
    output_path: &Path,
    marker: &str,
    max_lines: usize,
    max_bytes: usize,
) -> Option<String> {
    let output_path_text = output_path.to_string_lossy();
    loop {
        let serialized = serde_json::to_string(&value).ok()?;
        if serialized.len() <= max_bytes {
            if json_contains_path(&value, &output_path_text) {
                return Some(serialized);
            }
            break;
        }
        let longest = longest_json_string_len(&value);
        if longest == 0 {
            break;
        }
        let excess = serialized.len().saturating_sub(max_bytes);
        if excess > longest {
            break;
        }
        let target_bytes = longest.saturating_sub(excess);
        if !truncate_first_json_string(&mut value, longest, target_bytes, marker, max_lines) {
            break;
        }
    }

    let fallback = serde_json::json!({
        "truncated": true,
        "full_output_path": output_path_text,
    })
    .to_string();
    (fallback.len() <= max_bytes).then_some(fallback)
}

fn marker_output_path(text: &str) -> Option<PathBuf> {
    let (_, suffix) = text.split_once(TRUNCATED_MARKER)?;
    let path = suffix.trim_start();
    let end = path.rfind(" ...").unwrap_or(path.len());
    (!path[..end].is_empty()).then(|| PathBuf::from(&path[..end]))
}

fn json_output_path(value: &serde_json::Value) -> Option<PathBuf> {
    match value {
        serde_json::Value::String(text) => marker_output_path(text),
        serde_json::Value::Array(items) => items.iter().find_map(json_output_path),
        serde_json::Value::Object(map) => map
            .get("full_output_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .or_else(|| map.values().find_map(json_output_path)),
        _ => None,
    }
}

fn existing_output_path(content: &str) -> Option<PathBuf> {
    serde_json::from_str(content)
        .ok()
        .and_then(|value| json_output_path(&value))
        .or_else(|| marker_output_path(content))
}

async fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

async fn cleanup_expired(cfg: &ToolOutputConfig, directory: &Path) {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            cfg.retention_days.saturating_mul(86_400),
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

async fn store_full(
    cfg: &ToolOutputConfig,
    tool_name: &str,
    content: &str,
) -> std::io::Result<PathBuf> {
    let directory = cfg.resolved_directory();
    ensure_private_directory(&directory).await?;
    cleanup_expired(cfg, &directory).await;
    let safe_tool: String = tool_name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .take(32)
        .collect();
    let path = directory.join(format!("{safe_tool}-{}.txt", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(&path).await?;
    if let Err(error) = file.write_all(content.as_bytes()).await {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    if let Err(error) = file.flush().await {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    Ok(path)
}

pub async fn bound_text(cfg: &ToolOutputConfig, tool_name: &str, content: String) -> BoundText {
    let original_chars = content.chars().count();
    if content.len() <= cfg.max_bytes && line_count(&content) <= cfg.max_lines {
        return BoundText {
            visible_chars: original_chars,
            content,
            output_path: None,
            original_chars,
        };
    }

    let Ok(path) = store_full(cfg, tool_name, &content).await else {
        // Storage failure must never turn a successful tool call into a failure.
        return BoundText {
            visible_chars: original_chars,
            content,
            output_path: None,
            original_chars,
        };
    };
    let marker = format!("... {TRUNCATED_MARKER} {} ...", path.to_string_lossy());
    let visible = match serde_json::from_str(&content) {
        Ok(value) => {
            let Some(preview) =
                structured_preview(value, &path, &marker, cfg.max_lines, cfg.max_bytes)
            else {
                let _ = tokio::fs::remove_file(path).await;
                return BoundText {
                    visible_chars: original_chars,
                    content,
                    output_path: None,
                    original_chars,
                };
            };
            preview
        }
        Err(_) => preview(&content, &marker, cfg.max_lines, cfg.max_bytes),
    };
    BoundText {
        visible_chars: visible.chars().count(),
        content: visible,
        output_path: Some(path),
        original_chars,
    }
}

#[derive(Clone)]
struct ResultRef {
    message: usize,
    block: usize,
    id: String,
    is_error: bool,
    chars: usize,
}

fn estimated_tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

fn truncate_string(value: &mut String, max_chars: usize) -> Option<u64> {
    let original_chars = value.chars().count();
    if original_chars <= max_chars || value.contains(ARGUMENT_MARKER) {
        return None;
    }
    let prefix: String = value.chars().take(20).collect();
    *value = format!("{prefix}{ARGUMENT_MARKER}");
    Some(estimated_tokens(
        original_chars.saturating_sub(value.chars().count()),
    ))
}

fn truncate_old_arguments(
    block: &mut ContentBlock,
    protected_ids: &HashSet<String>,
    max: usize,
) -> (usize, u64) {
    let ContentBlock::ToolCall { id, name, input } = block else {
        return (0, 0);
    };
    if protected_ids.contains(id) {
        return (0, 0);
    }
    match name.as_str() {
        "write_file" => {
            let Some(value) = input.get_mut("content") else {
                return (0, 0);
            };
            let Some(mut text) = value.as_str().map(str::to_string) else {
                return (0, 0);
            };
            let Some(saved_tokens) = truncate_string(&mut text, max) else {
                return (0, 0);
            };
            *value = serde_json::Value::String(text);
            (1, saved_tokens)
        }
        "edit_file" => {
            let Some(items) = input
                .get_mut("edits")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return (0, 0);
            };
            let mut changed = 0;
            let mut saved_tokens = 0u64;
            for item in items {
                let Some(mut text) = item.as_str().map(str::to_string) else {
                    continue;
                };
                if let Some(saved) = truncate_string(&mut text, max) {
                    *item = serde_json::Value::String(text);
                    changed += 1;
                    saved_tokens = saved_tokens.saturating_add(saved);
                }
            }
            (changed, saved_tokens)
        }
        _ => (0, 0),
    }
}

pub async fn microcompact_history(
    messages: &mut [Message],
    cfg: &ToolOutputConfig,
) -> MicrocompactStats {
    let mut call_info: HashMap<String, (String, Option<PathBuf>)> = HashMap::new();
    let mut results = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            match block {
                ContentBlock::ToolCall { id, name, input } => {
                    let path = input
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from);
                    call_info.insert(id.clone(), (name.clone(), path));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => results.push(ResultRef {
                    message: message_index,
                    block: block_index,
                    id: tool_use_id.clone(),
                    is_error: *is_error,
                    chars: content.chars().count(),
                }),
                _ => {}
            }
        }
    }

    let successful: Vec<_> = results.iter().filter(|item| !item.is_error).collect();
    let protected_recent: HashSet<String> = successful
        .iter()
        .rev()
        .take(cfg.intra_turn_keep_recent_results)
        .map(|item| item.id.clone())
        .collect();
    let protected_errors: HashSet<String> = results
        .iter()
        .filter(|item| item.is_error)
        .map(|item| item.id.clone())
        .collect();
    let protected_ids: HashSet<String> =
        protected_recent.union(&protected_errors).cloned().collect();

    let mut retained_tokens = 0u64;
    let mut to_prune = Vec::new();
    for result in results.iter().rev() {
        let tokens = estimated_tokens(result.chars);
        if result.is_error || protected_recent.contains(&result.id) {
            retained_tokens = retained_tokens.saturating_add(tokens);
            continue;
        }
        if retained_tokens.saturating_add(tokens) <= cfg.intra_turn_result_budget_tokens {
            retained_tokens += tokens;
        } else {
            to_prune.push(result.clone());
        }
    }

    let mut stats = MicrocompactStats::default();
    for item in to_prune {
        let ContentBlock::ToolResult { content, .. } =
            &mut messages[item.message].content[item.block]
        else {
            continue;
        };
        if content.contains(PRUNED_MARKER) {
            continue;
        }
        let tool_name = call_info
            .get(&item.id)
            .map(|(name, _)| name.as_str())
            .unwrap_or("tool");
        let managed_directory = cfg.resolved_directory();
        let existing_path =
            existing_output_path(content).filter(|path| path.starts_with(&managed_directory));
        let (path, created) = if let Some(path) = existing_path {
            match tokio::fs::metadata(&path).await {
                Ok(metadata) if metadata.is_file() => (path, false),
                _ => {
                    let Ok(path) = store_full(cfg, tool_name, content).await else {
                        continue;
                    };
                    (path, true)
                }
            }
        } else {
            let Ok(path) = store_full(cfg, tool_name, content).await else {
                continue;
            };
            (path, true)
        };
        let marker = format!("[{PRUNED_MARKER} {}]", path.to_string_lossy());
        if marker.chars().count() >= item.chars {
            if created {
                let _ = tokio::fs::remove_file(path).await;
            }
            continue;
        }
        stats.estimated_tokens_saved = stats
            .estimated_tokens_saved
            .saturating_add(estimated_tokens(item.chars - marker.chars().count()));
        *content = marker;
        stats.results_pruned += 1;
        if let Some((name, path)) = call_info.get(&item.id) {
            if name == "read_file" {
                match path {
                    Some(path) if path.is_absolute() => {
                        stats.evicted_read_paths.push(path.clone());
                    }
                    Some(_) => stats.clear_read_cache = true,
                    None => stats.clear_read_cache = true,
                }
            }
        }
    }

    for message in messages {
        for block in &mut message.content {
            let (arguments_pruned, tokens_saved) =
                truncate_old_arguments(block, &protected_ids, cfg.old_argument_max_chars);
            stats.arguments_pruned += arguments_pruned;
            stats.estimated_tokens_saved =
                stats.estimated_tokens_saved.saturating_add(tokens_saved);
        }
    }
    stats.evicted_read_paths.sort();
    stats.evicted_read_paths.dedup();
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ContentBlock, Message, Role};
    use serde_json::json;

    fn cfg(dir: &std::path::Path) -> ToolOutputConfig {
        ToolOutputConfig {
            directory: Some(dir.to_string_lossy().to_string()),
            max_bytes: 256,
            max_lines: 6,
            retention_days: 7,
            intra_turn_result_budget_tokens: 12,
            intra_turn_keep_recent_results: 1,
            old_argument_max_chars: 24,
        }
    }

    fn tool_pair(
        id: &str,
        name: &str,
        input: serde_json::Value,
        output: &str,
        error: bool,
    ) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    input,
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: output.to_string(),
                    is_error: error,
                }],
            },
        ]
    }

    #[tokio::test]
    async fn large_output_is_bounded_and_full_text_is_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let original = (0..30)
            .map(|i| format!("line-{i:02}-{}", "x".repeat(20)))
            .collect::<Vec<_>>()
            .join("\n");
        let bounded = bound_text(&cfg(dir.path()), "search", original.clone()).await;

        let path = bounded
            .output_path
            .expect("large output should be offloaded");
        assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), original);
        assert!(bounded.content.contains("full output saved to"));
        assert!(bounded.content.contains("truncated"));
        assert!(bounded.content.lines().count() <= 6);
        assert!(bounded.content.len() <= 256);
    }

    #[tokio::test]
    async fn short_output_is_unchanged_and_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let bounded = bound_text(&cfg(dir.path()), "read_file", "small".to_string()).await;
        assert_eq!(bounded.content, "small");
        assert!(bounded.output_path.is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn storage_failure_fails_open_without_changing_successful_content() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("file");
        std::fs::write(&not_a_directory, "occupied").unwrap();
        let original = "large-output\n".repeat(100);

        let bounded = bound_text(&cfg(&not_a_directory), "read_file", original.clone()).await;

        assert_eq!(bounded.content, original);
        assert_eq!(bounded.original_chars, bounded.visible_chars);
        assert!(bounded.output_path.is_none());
    }

    #[tokio::test]
    async fn utf8_preview_with_long_managed_path_stays_within_caps() {
        let dir = tempfile::tempdir().unwrap();
        let long_directory = dir.path().join("界".repeat(40)).join("nested-tool-output");
        let original = "🙂漢字-output-line\n".repeat(100);

        let bounded = bound_text(&cfg(&long_directory), "read_file", original).await;

        assert!(bounded.output_path.is_some());
        assert!(bounded.content.len() <= 256);
        assert!(bounded.content.lines().count() <= 6);
        assert!(std::str::from_utf8(bounded.content.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn structured_json_remains_valid_when_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let original = json!({
            "exit": 0,
            "out": format!("first\n{}\nlast", "x".repeat(2_000)),
            "err": ""
        })
        .to_string();

        let bounded = bound_text(&cfg(dir.path()), "exec", original.clone()).await;

        assert!(bounded.content.len() <= 256);
        let parsed: serde_json::Value =
            serde_json::from_str(&bounded.content).expect("bounded JSON must remain valid");
        assert_eq!(parsed["exit"], 0);
        assert!(parsed["out"].as_str().unwrap().contains("first"));
        assert!(parsed["out"].as_str().unwrap().contains("last"));
        assert!(parsed["out"]
            .as_str()
            .unwrap()
            .contains("full output saved to"));
        assert_eq!(
            std::fs::read_to_string(bounded.output_path.unwrap()).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn microcompaction_prunes_old_successes_but_keeps_recent_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_pair(
            "old",
            "search",
            json!({"pattern":"x"}),
            &"a".repeat(800),
            false,
        ));
        messages.extend(tool_pair(
            "error",
            "search",
            json!({"pattern":"y"}),
            &"boom".repeat(20),
            true,
        ));
        messages.extend(tool_pair(
            "new",
            "search",
            json!({"pattern":"z"}),
            &"c".repeat(80),
            false,
        ));

        let stats = microcompact_history(&mut messages, &cfg(dir.path())).await;
        assert_eq!(stats.results_pruned, 1);
        assert!(stats.estimated_tokens_saved > 0);

        let mut results = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id, content, is_error)),
                _ => None,
            });
        let old = results.next().unwrap();
        assert_eq!(old.0, "old");
        assert!(old.1.contains("old tool result pruned"));
        assert!(!old.2);
        let error = results.next().unwrap();
        assert_eq!(error.0, "error");
        assert!(error.1.contains("boom"));
        assert!(*error.2);
        let new = results.next().unwrap();
        assert_eq!(new.0, "new");
        assert!(new.1.chars().all(|c| c == 'c'));
    }

    #[tokio::test]
    async fn microcompaction_truncates_old_edit_arguments_and_reports_evicted_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_pair(
            "write",
            "write_file",
            json!({"path":"out.txt", "content":"q".repeat(200)}),
            "written",
            false,
        ));
        messages.extend(tool_pair(
            "read",
            "read_file",
            json!({"path":"src/lib.rs"}),
            &"r".repeat(800),
            false,
        ));
        messages.extend(tool_pair(
            "new",
            "search",
            json!({"pattern":"z"}),
            "recent",
            false,
        ));

        let stats = microcompact_history(&mut messages, &cfg(dir.path())).await;
        assert_eq!(stats.arguments_pruned, 1);
        assert!(stats.evicted_read_paths.is_empty());
        assert!(stats.clear_read_cache);
        let write = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::ToolCall { id, input, .. } if id == "write" => Some(input),
                _ => None,
            })
            .unwrap();
        let content = write["content"].as_str().unwrap();
        assert!(content.len() < 80);
        assert!(content.contains("argument truncated"));
        assert_eq!(write["path"], "out.txt");
    }

    #[tokio::test]
    async fn repeated_microcompaction_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_pair(
            "old",
            "edit_file",
            json!({
                "path": "src/lib.rs",
                "edits": ["old".repeat(100), "new".repeat(100)]
            }),
            &"result".repeat(200),
            false,
        ));
        messages.extend(tool_pair(
            "new",
            "search",
            json!({"pattern":"z"}),
            "recent",
            false,
        ));

        let first = microcompact_history(&mut messages, &cfg(dir.path())).await;
        let after_first = serde_json::to_string(&messages).unwrap();
        let files_after_first = std::fs::read_dir(dir.path()).unwrap().count();
        let second = microcompact_history(&mut messages, &cfg(dir.path())).await;

        assert!(first.results_pruned > 0);
        assert!(first.arguments_pruned > 0);
        assert_eq!(second.results_pruned, 0);
        assert_eq!(second.arguments_pruned, 0);
        assert_eq!(second.estimated_tokens_saved, 0);
        assert_eq!(serde_json::to_string(&messages).unwrap(), after_first);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            files_after_first
        );
    }

    #[tokio::test]
    async fn argument_pruning_contributes_to_saved_token_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = cfg(dir.path());
        config.intra_turn_result_budget_tokens = 10_000;
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_pair(
            "write",
            "write_file",
            json!({"path":"out.txt", "content":"x".repeat(400)}),
            "written",
            false,
        ));
        messages.extend(tool_pair(
            "recent",
            "search",
            json!({"pattern":"x"}),
            "recent",
            false,
        ));

        let stats = microcompact_history(&mut messages, &config).await;

        assert_eq!(stats.results_pruned, 0);
        assert_eq!(stats.arguments_pruned, 1);
        assert!(stats.estimated_tokens_saved > 0);
    }

    #[tokio::test]
    async fn microcompaction_reuses_existing_full_output_path() {
        let dir = tempfile::tempdir().unwrap();
        let original = "large-result\n".repeat(100);
        let bounded = bound_text(&cfg(dir.path()), "read_file", original.clone()).await;
        let original_path = bounded.output_path.unwrap();
        let mut messages = vec![Message::user("task")];
        messages.extend(tool_pair(
            "old",
            "read_file",
            json!({"path":"/tmp/source.txt"}),
            &bounded.content,
            false,
        ));
        messages.extend(tool_pair(
            "recent",
            "search",
            json!({"pattern":"x"}),
            "recent",
            false,
        ));

        let stats = microcompact_history(&mut messages, &cfg(dir.path())).await;

        assert_eq!(stats.results_pruned, 1);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(&original_path).unwrap(), original);
        let result = messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "old" => Some(content),
                _ => None,
            })
            .unwrap();
        assert!(result.contains(&original_path.to_string_lossy().to_string()));
    }
}
