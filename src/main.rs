mod config;
mod index;
mod mcp;
mod ops;
mod pipeline_cache;
mod plugins;
mod protocol;
mod session;
mod snapshot;
mod tool_runner;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(name = "daimonos", about = "Daimonos — agent-optimized OS layer")]
struct Cli {
    /// Unix socket path (ignored in --mcp mode)
    #[arg(short, long, default_value = "/tmp/daimonos.sock")]
    socket: PathBuf,

    /// Workspace root directory
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// Human-readable debug output (socket mode only)
    #[arg(long, default_value_t = false)]
    debug: bool,

    /// Path to config file (default: search workspace then ~/.config/daimonos/)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run as MCP server over stdio (for Cursor integration)
    #[arg(long, default_value_t = false)]
    mcp: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let cfg = Arc::new(config::load(cli.config.as_deref(), &workspace));

    let ws_index = Arc::new(index::WorkspaceIndex::new(
        workspace.clone(),
        &cfg.index,
    ));
    ws_index.spawn_reindex();

    let tool_reg = Arc::new(tool_runner::ToolRegistry::new());
    config::register_tools(&cfg, &tool_reg).await;

    if plugins::git::is_available() {
        tool_reg
            .register(Arc::new(plugins::git::GitPlugin::new()))
            .await;
        eprintln!("auto-registered git tool plugin");
    }

    let pcache = Arc::new(pipeline_cache::PipelineCache::new(&workspace));

    if cli.mcp {
        mcp::run_mcp_server(workspace, cfg, ws_index, tool_reg, pcache).await
    } else {
        run_socket_server(cli, workspace, cfg, ws_index, tool_reg, pcache).await
    }
}

async fn run_socket_server(
    cli: Cli,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    ws_index: Arc<index::WorkspaceIndex>,
    tool_reg: Arc<tool_runner::ToolRegistry>,
    pcache: Arc<pipeline_cache::PipelineCache>,
) -> anyhow::Result<()> {
    if cli.socket.exists() {
        std::fs::remove_file(&cli.socket)?;
    }

    eprintln!("pipeline cache: watching {:?}", workspace);

    let listener = UnixListener::bind(&cli.socket)?;
    eprintln!(
        "daimonos listening on {:?} (workspace: {:?})",
        cli.socket, workspace
    );

    loop {
        let (stream, _addr) = listener.accept().await?;
        let ws = workspace.clone();
        let debug = cli.debug;
        let idx = ws_index.clone();
        let cfg_clone = cfg.clone();
        let tr = tool_reg.clone();
        let pc = pcache.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ws, debug, idx, cfg_clone, tr, pc).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    workspace: PathBuf,
    debug: bool,
    ws_index: Arc<index::WorkspaceIndex>,
    cfg: Arc<config::Config>,
    tool_reg: Arc<tool_runner::ToolRegistry>,
    pcache: Arc<pipeline_cache::PipelineCache>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut session = session::Session::new(workspace, cfg);
    session.index = Some(ws_index);
    session.tool_registry = Some(tool_reg);
    session.pipeline_cache = Some(pcache);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let response = match serde_json::from_str::<protocol::Request>(&line) {
            Ok(req) => ops::dispatch(&mut session, req).await,
            Err(e) => protocol::Response::err(3, &format!("parse error: {e}")),
        };

        let out = if debug {
            serde_json::to_string_pretty(&response)?
        } else {
            serde_json::to_string(&response)?
        };

        writer.write_all(out.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}
