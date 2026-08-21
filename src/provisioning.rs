//! Unified tool provisioning shared by every daimonos front-door (MCP, agent,
//! ACP). Historically each entry point hand-assembled its tool `Session`, and
//! agent/ACP silently omitted the tool registry, the workspace index, and the
//! pipeline cache — so plugin tools (cargo/git/gh/…) failed inside
//! `execute_script` on those paths while working under MCP.
//!
//! This module is the single source of truth: [`build_tool_services`] builds
//! the full service set (via [`crate::plugins::register_builtin_plugins`]) and
//! [`provision_session`] attaches it to a [`Session`]. All three front-doors
//! call these, so a tool added to the canonical plugin list is available in
//! every mode by default — making a tool mode-specific requires deliberate
//! extra work, not the reverse.

use std::sync::Arc;

use crate::analytics::AnalyticsStore;
use crate::config::{self, Config};
use crate::index::{self, WorkspaceIndex};
use crate::pipeline_cache::PipelineCache;
use crate::session::Session;
use crate::tool_runner::ToolRegistry;

/// The shared per-session services every front-door provisions. `analytics` is
/// optional (disabled by config or init failure); the rest are always built.
pub struct ToolServices {
    pub registry: Arc<ToolRegistry>,
    pub index: Arc<WorkspaceIndex>,
    pub pipeline_cache: Arc<PipelineCache>,
    pub analytics: Option<Arc<AnalyticsStore>>,
}

/// Build the full service set for a workspace.
///
/// `eager_index` identifies long-lived frontends that may warm in the
/// background. The configured index mode then decides whether they do:
/// `eager` always warms, `lazy` never does, and `hybrid` uses the bounded
/// project-signal heuristic. One-shot agent runs stay cold in every mode;
/// file search populates the index on demand.
///
/// `background_work` gates the pipeline-cache config watcher the same way.
pub async fn build_tool_services(
    workspace: &std::path::Path,
    cfg: &Arc<Config>,
    quiet_stderr: bool,
    eager_index: bool,
    analytics: Option<Arc<AnalyticsStore>>,
) -> ToolServices {
    let registry = Arc::new(ToolRegistry::with_process_config(cfg.process.clone()));
    crate::plugins::register_builtin_plugins(cfg, &registry, quiet_stderr).await;

    let index = Arc::new(WorkspaceIndex::new(
        workspace.to_path_buf(),
        &cfg.index,
        !quiet_stderr,
    ));
    let background_work = index::should_warm_index(workspace, &cfg.index, eager_index);
    if background_work {
        index.spawn_reindex();
    }

    let pipeline_cache = Arc::new(PipelineCache::with_config_watching(
        workspace,
        &cfg.pipeline_cache,
        background_work,
    ));

    ToolServices {
        registry,
        index,
        pipeline_cache,
        analytics,
    }
}

/// Attach the shared services to a `Session`, plus the startup-edge settings
/// (external session id from the launch env, effective verbosity). This is the
/// one place the service fields are wired, so no front-door can drift by
/// forgetting one.
pub fn provision_session(session: &mut Session, services: &ToolServices) {
    session.index = Some(Arc::clone(&services.index));
    session.tool_registry = Some(Arc::clone(&services.registry));
    session.pipeline_cache = Some(Arc::clone(&services.pipeline_cache));
    session.analytics = services.analytics.clone();
    session.external_session_id = crate::analytics::read_agent_session_id_env();
    session.verbosity = config::effective_verbosity(&session.cfg);
}
