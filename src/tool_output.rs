//! Model-visible tool-output bounding and deterministic intra-turn pruning.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::io::AsyncWriteExt;

use crate::config::{ReformatConfig, ToolOutputConfig};
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
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("daimonos-tool-output-{}", std::process::id()))
            })
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

/// Whether this tool result is eligible for the paid LLM reformatting pass
/// (vikunja #1235). Pure and cheap so the decision is testable and can be made
/// before anything is allocated or sent anywhere.
///
/// Every gate here exists to stop money being spent:
/// - `enabled` is the master switch.
/// - The allowlist is explicit; an empty list matches nothing, so enabling
///   without naming tools is a no-op rather than a surprise bill.
/// - Small outputs are already actionable, so a call would cost more than it
///   saves.
/// - **Errors pass through verbatim.** A rewrite could soften or bury the
///   failure, and the raw failure text is precisely what the agent must react
///   to. This is the one gate that is about correctness rather than cost.
pub fn should_reformat(
    cfg: &ReformatConfig,
    tool_name: &str,
    is_error: bool,
    content_chars: usize,
) -> bool {
    cfg.enabled
        && !is_error
        && content_chars >= cfg.min_chars
        && cfg.tools.iter().any(|t| t == tool_name)
}

/// The working tree's uncommitted diff, for the reformatter prompt (#1235).
///
/// Best-effort and always bounded: any failure — not a repo, git missing,
/// non-zero exit — yields `None`, because a missing diff only makes the summary
/// slightly less specific, while blocking on git would make a *tool result*
/// depend on VCS state.
///
/// `--stat`-style full diffs can be enormous, so the output is capped; the
/// prompt only needs enough to correlate a failure with a recent edit.
pub(crate) async fn working_tree_diff(workspace: &Path, max_chars: usize) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["diff", "--no-color"])
        .current_dir(workspace)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max_chars).collect())
}

/// Build the reformatter prompt (vikunja #1235, adapted from Kwaak's
/// `templates/tool_summarizer.md`).
///
/// Three ingredients carry the value, and each is why the result is
/// *actionable* rather than merely shorter:
///
/// 1. **The working-tree diff.** Lets the summary say "your own edit to X
///    likely caused this failure" and cite real paths, instead of describing
///    the failure in the abstract. Omitted entirely when there is none — an
///    empty diff section reads as "nothing changed" and would mislead.
/// 2. **The live tool catalog.** Every proposed fix must be phrased in terms of
///    a tool the agent can actually call, so recovery paths survive
///    compression instead of being summarised into unusable prose. The model is
///    told explicitly not to invent tools.
/// 3. **Reformat, do not drop.** Repeats collapse to one instance plus a count.
///    That is the difference between this and truncation: no information is
///    discarded, it is restructured.
pub fn build_reformat_prompt(
    tool_name: &str,
    args: &serde_json::Value,
    output: &str,
    diff: Option<&str>,
    tool_catalog: &[(&str, &str)],
) -> String {
    let mut p = String::new();
    p.push_str(
        "You are reformatting the output of a developer tool for another AI agent.\n\
         Reformat it — do NOT drop information. Collapse repeating patterns to ONE \
         instance plus a count (e.g. \"37 more identical failures\"). Keep every \
         distinct error, file path and line number.\n\
         Be concise and actionable: lead with what broke and the most likely cause.\n\n",
    );
    if !tool_catalog.is_empty() {
        p.push_str(
            "Tools the agent can call. Phrase any suggested next step using ONLY \
             these — never invent a tool:\n",
        );
        for (name, desc) in tool_catalog {
            // First line only: the catalog is context, not documentation.
            let first = desc.lines().next().unwrap_or("").trim();
            p.push_str(&format!("- {name}: {first}\n"));
        }
        p.push('\n');
    }
    if let Some(diff) = diff.map(str::trim).filter(|d| !d.is_empty()) {
        p.push_str(
            "Uncommitted changes in the working tree. If the failure is plausibly \
             caused by one of these edits, say so and name the file:\n```diff\n",
        );
        p.push_str(diff);
        p.push_str("\n```\n\n");
    }
    p.push_str(&format!(
        "Tool: {tool_name}\nArguments: {args}\n\nOutput:\n"
    ));
    p.push_str(output);
    p
}

/// Replace a noisy tool result with an LLM reformatting, keeping the raw output
/// recoverable on disk (vikunja #1235).
///
/// Call only when [`should_reformat`] returned true — this function assumes the
/// decision to spend a model call has already been made.
///
/// Degradation is the important property: this is a *paid optional
/// enhancement*, so any failure — storage, provider error, empty reply — falls
/// back to [`bound_text`], producing exactly the output the deterministic path
/// would have produced without the feature. A reformatter outage must never
/// destroy or truncate a successful tool result.
#[allow(clippy::too_many_arguments)]
pub async fn reformat_text(
    cfg: &ToolOutputConfig,
    tool_name: &str,
    content: String,
    provider: &dyn crate::providers::LlmProvider,
    model: &str,
    args: &serde_json::Value,
    diff: Option<&str>,
    tool_catalog: &[(&str, &str)],
) -> BoundText {
    let original_chars = content.chars().count();

    // Cap what we pay to summarise. Head and tail are kept because a test run's
    // signal clusters at both ends: the first failures and the final summary
    // line. The middle is the repetitive part the reformatter would collapse
    // anyway.
    let capped = head_tail(&content, cfg.reformat.max_input_chars);
    let prompt = build_reformat_prompt(tool_name, args, &capped, diff, tool_catalog);

    let response = provider
        .complete(
            &crate::providers::Context {
                messages: vec![Message::user(prompt)],
                system: None,
                tools: vec![],
                stable_prefix_len: 0,
            },
            &crate::providers::CompleteOpts {
                model: model.to_string(),
                // No reasoning: this is a mechanical restructuring, and thinking
                // tokens on a cheap model are pure cost here.
                thinking: crate::providers::ThinkingLevel::Off,
                ..crate::providers::CompleteOpts::default()
            },
        )
        .await;

    let summary: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if response.error_message.is_some() || summary.trim().is_empty() {
        tracing::warn!(
            target: "daimonos::tool_output",
            tool = tool_name,
            error = response.error_message.as_deref().unwrap_or("empty reply"),
            "tool-output reformat failed; falling back to deterministic bounding"
        );
        return bound_text(cfg, tool_name, content).await;
    }

    // Only now preserve the raw output: storing before the call would write the
    // file twice whenever we fall back, since `bound_text` stores it too.
    // If storage fails the reformatting would be lossy — there would be nowhere
    // to recover the detail from — so degrade to the deterministic path.
    let Ok(path) = store_full(cfg, tool_name, &content).await else {
        tracing::warn!(
            target: "daimonos::tool_output",
            tool = tool_name,
            "reformat succeeded but the raw output could not be stored; \
             falling back so no detail is lost"
        );
        return bound_text(cfg, tool_name, content).await;
    };

    let visible = format!(
        "{summary}\n\n[reformatted from {original_chars} chars; {TRUNCATED_MARKER} {}]",
        path.display()
    );
    BoundText {
        visible_chars: visible.chars().count(),
        content: visible,
        output_path: Some(path),
        original_chars,
    }
}

/// Keep the first and last `cap/2` chars, marking the elision. A verifier's
/// signal clusters at both ends — the first failures and the closing summary —
/// so a plain head truncation would routinely discard the count line.
fn head_tail(content: &str, cap: usize) -> String {
    if content.chars().count() <= cap {
        return content.to_string();
    }
    let half = cap / 2;
    let head: String = content.chars().take(half).collect();
    let tail: String = content
        .chars()
        .skip(content.chars().count().saturating_sub(half))
        .collect();
    format!("{head}\n...(middle elided for the reformatter)...\n{tail}")
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

    /// vikunja #1235 (adapted from Kwaak): the reformatter is a *paid* LLM pass,
    /// so every gate that keeps it from firing is load-bearing. Daimonos'
    /// context philosophy is deterministic-first (#1193 bounding, #1194
    /// pruning); this is the deliberate opt-in exception for noisy verifier
    /// tools, and must stay opt-in.
    #[test]
    fn reformat_gating_is_opt_in_and_narrow() {
        let base = ReformatConfig {
            enabled: true,
            tools: vec!["pytest".into(), "cargo".into()],
            min_chars: 100,
            model: None,
            max_input_chars: 100_000,
        };
        let big = 500;
        let ok = false; // is_error

        assert!(should_reformat(&base, "pytest", ok, big));
        assert!(should_reformat(&base, "cargo", ok, big));

        // Not on the allowlist: a cheap `ls` must never cost a model call.
        assert!(!should_reformat(&base, "ls", ok, big));

        // Small output is already actionable; reformatting would cost more
        // than it saves.
        assert!(!should_reformat(&base, "pytest", ok, 99));

        // Errors pass through verbatim. An LLM rewrite could soften or bury the
        // failure, and the failure text is exactly what the agent must react to.
        assert!(!should_reformat(&base, "pytest", true, big));

        // Master switch.
        let off = ReformatConfig {
            enabled: false,
            ..base.clone()
        };
        assert!(!should_reformat(&off, "pytest", ok, big));

        // Empty allowlist means nothing matches, even enabled — no accidental
        // "enabled implies everything".
        let no_tools = ReformatConfig {
            tools: vec![],
            ..base.clone()
        };
        assert!(!should_reformat(&no_tools, "pytest", ok, big));
    }

    struct StubModel {
        reply: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for StubModel {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            _opts: &crate::providers::CompleteOpts,
        ) -> crate::providers::LlmResponse {
            match self.reply {
                Some(text) => crate::providers::LlmResponse {
                    content: vec![ContentBlock::Text(text.to_string())],
                    stop_reason: crate::providers::StopReason::EndTurn,
                    error_message: None,
                    context_overflow: false,
                    retryable: false,
                    usage: crate::providers::Usage::default(),
                },
                None => crate::providers::LlmResponse::error("reformatter unavailable"),
            }
        }
    }

    /// The happy path: the model-visible text becomes the reformatting, while
    /// the raw output stays recoverable on disk via the #1193 offload.
    #[tokio::test]
    async fn reformat_replaces_visible_text_and_preserves_the_raw_output() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "FAILED tests/test_a.py::test_x - AssertionError\n".repeat(200);
        let model = StubModel {
            reply: Some("1 distinct failure (x200): test_x AssertionError in tests/test_a.py"),
        };

        let out = reformat_text(
            &cfg(dir.path()),
            "pytest",
            raw.clone(),
            &model,
            "cheap-model",
            &json!({}),
            None,
            &[],
        )
        .await;

        assert!(
            out.content.contains("1 distinct failure"),
            "reformatted text should be what the model sees"
        );
        let path = out.output_path.expect("raw output must be offloaded");
        let stored = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(stored, raw, "the full raw output must be recoverable");
        // The pointer has to be in the visible text or the agent cannot find it.
        assert!(out.content.contains(&path.to_string_lossy().to_string()));
        assert_eq!(out.original_chars, raw.chars().count());
    }

    /// A reformatter failure must never destroy or truncate a successful tool
    /// result. This is a *paid optional enhancement*; if it cannot run, the
    /// deterministic path must carry the output exactly as it would have
    /// without the feature.
    #[tokio::test]
    async fn a_failed_reformat_falls_back_to_deterministic_bounding() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "line of real output\n".repeat(200);
        let model = StubModel { reply: None };

        let out = reformat_text(
            &cfg(dir.path()),
            "pytest",
            raw.clone(),
            &model,
            "cheap-model",
            &json!({}),
            None,
            &[],
        )
        .await;

        let deterministic = bound_text(&cfg(dir.path()), "pytest", raw.clone()).await;
        // Compare shape, not the stored file's random name: the two calls store
        // to different UUIDs by design.
        let strip = |s: &str| {
            s.split(TRUNCATED_MARKER)
                .next()
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(
            strip(&out.content),
            strip(&deterministic.content),
            "must degrade to exactly the deterministic bounding"
        );
        assert!(
            out.content.contains(TRUNCATED_MARKER),
            "fallback still points at the stored output"
        );
        let path = out.output_path.expect("raw still recoverable");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), raw);

        // The fallback must not have written the raw output twice.
        let mut stored = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut files = 0;
        while stored.next_entry().await.unwrap().is_some() {
            files += 1;
        }
        assert_eq!(files, 2, "one file per bound_text call, none orphaned");
    }

    /// The prompt is the whole value of this feature. Kwaak's version carries
    /// three ingredients most reformatters omit, and each one is why the output
    /// is *actionable* rather than merely shorter (#1235 / catalog #1178).
    #[test]
    fn reformat_prompt_carries_diff_catalog_and_no_drop_instruction() {
        let catalog = [
            ("edit_file", "Apply string-replace edits to a file"),
            ("exec", "Run a command"),
        ];
        let prompt = build_reformat_prompt(
            "pytest",
            &json!({"args": ["-q"]}),
            "FAILED tests/test_a.py::test_x - AssertionError\n"
                .repeat(40)
                .as_str(),
            Some("diff --git a/src/config.rs b/src/config.rs\n+broken line\n"),
            &catalog,
        );

        // 1. The diff: lets the summary say "your own edit likely caused this".
        assert!(prompt.contains("src/config.rs"), "diff must be present");
        assert!(prompt.contains("+broken line"));

        // 2. The catalog: every proposed fix must name a tool the agent can
        //    actually call, so recovery paths survive compression instead of
        //    being summarised away into prose.
        assert!(prompt.contains("edit_file"), "tool catalog must be present");
        assert!(prompt.contains("Apply string-replace edits"));

        // 3. Reformat, don't drop — with repeats collapsed to one + a count.
        let lower = prompt.to_lowercase();
        assert!(lower.contains("do not drop") || lower.contains("not drop"));
        assert!(lower.contains("count"));

        // The tool being summarised, and its arguments, are identified.
        assert!(prompt.contains("pytest"));
        assert!(prompt.contains("-q"));
        // And the actual output.
        assert!(prompt.contains("AssertionError"));
    }

    /// With no diff available (no VCS, or nothing changed yet) the prompt must
    /// still be well-formed rather than carrying an empty section that reads as
    /// "nothing changed" and misleads the summary.
    #[test]
    fn reformat_prompt_omits_the_diff_section_when_there_is_none() {
        let prompt = build_reformat_prompt("cargo", &json!({}), "output", None, &[]);
        assert!(!prompt.to_lowercase().contains("diff --git"));
        assert!(prompt.contains("output"));
    }

    #[test]
    fn reformat_is_disabled_by_default() {
        let d = ReformatConfig::default();
        assert!(!d.enabled);
        assert!(d.tools.is_empty());
        assert!(!should_reformat(&d, "pytest", false, 1_000_000));
    }

    fn cfg(dir: &std::path::Path) -> ToolOutputConfig {
        ToolOutputConfig {
            directory: Some(dir.to_string_lossy().to_string()),
            max_bytes: 256,
            max_lines: 6,
            retention_days: 7,
            intra_turn_result_budget_tokens: 12,
            intra_turn_keep_recent_results: 1,
            old_argument_max_chars: 24,
            reformat: ReformatConfig::default(),
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
