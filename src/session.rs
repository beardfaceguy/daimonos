use crate::config::Config;
use crate::index::WorkspaceIndex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Per-connection session state.
/// Persists working directory, env vars, and background processes across calls.
pub struct Session {
    pub workspace: PathBuf,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub bg_processes: HashMap<u32, BgProcess>,
    pub index: Option<Arc<WorkspaceIndex>>,
    pub cfg: Arc<Config>,
    next_pid: u32,
}

pub struct BgProcess {
    pub child: tokio::process::Child,
    pub output_path: PathBuf,
}

impl Session {
    pub fn new(workspace: PathBuf, cfg: Arc<Config>) -> Self {
        let cwd = workspace.clone();
        Self {
            workspace,
            cwd,
            env: HashMap::new(),
            bg_processes: HashMap::new(),
            index: None,
            cfg,
            next_pid: 1,
        }
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    pub fn alloc_pid(&mut self) -> u32 {
        let pid = self.next_pid;
        self.next_pid += 1;
        pid
    }
}
