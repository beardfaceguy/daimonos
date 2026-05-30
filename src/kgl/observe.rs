//! Observed-provenance capture: when an agent acts through daimonos, record what
//! it actually did into the per-workspace KGL graph — observed, not authored.
//!
//! v0.1 covers direct file tool calls (write_file/edit_file/read_file). The env
//! gate and the call site live in the MCP layer so default behavior is unchanged.
//! Capturing script-driven ops (the execute_script path) is the documented
//! follow-up — and the literal first step of KGL growing out of the Starlark glue.

use crate::kgl::model::EdgeKind;
use crate::kgl::store::KglStore;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Record a direct file-tool call as an observed reads/mutates edge from the
/// session to the touched file. No-op for tools that aren't file ops.
pub fn record_file_op(
    workspace: &Path,
    session_id: &str,
    tool: &str,
    args: &Value,
    now: &str,
) -> Result<()> {
    let kind = match tool {
        "write_file" | "edit_file" => EdgeKind::Mutates,
        "read_file" => EdgeKind::Reads,
        _ => return Ok(()),
    };
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let resource = file_urn(workspace, path);
    let store = KglStore::open_workspace(workspace)?;
    store.record_observation(session_id, kind, &resource, now)
}

fn file_urn(workspace: &Path, path: &str) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    };
    format!("file://{}", abs.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_write_as_mutates_with_session_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        record_file_op(ws, "sess-9", "write_file", &json!({"path": "src/a.rs"}), "t0").unwrap();

        let store = KglStore::open_workspace(ws).unwrap();
        let urn = format!("file://{}", ws.join("src/a.rs").to_string_lossy());
        let w = store.writers_of(&urn).unwrap();
        assert!(w.iter().any(|r| r.node.name.as_deref() == Some("sess-9")));
    }

    #[test]
    fn ignores_non_file_tools() {
        let tmp = tempfile::tempdir().unwrap();
        // no-op (and does not create a store)
        record_file_op(tmp.path(), "s", "exec", &json!({"command": "ls"}), "t0").unwrap();
        assert!(!tmp.path().join(".kgl").exists());
    }
}
