mod config;
mod index;
mod ops;
mod protocol;
mod session;

use clap::Parser;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(name = "daimonos", about = "Daimonos — agent-optimized OS layer")]
struct Cli {
    /// Unix socket path
    #[arg(short, long, default_value = "/tmp/daimonos.sock")]
    socket: PathBuf,

    /// Workspace root directory
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// Human-readable debug output
    #[arg(long, default_value_t = false)]
    debug: bool,

    /// Path to config file (default: search workspace then ~/.config/daimonos/)
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let cfg = std::sync::Arc::new(config::load(cli.config.as_deref(), &workspace));

    if cli.socket.exists() {
        std::fs::remove_file(&cli.socket)?;
    }

    let ws_index = std::sync::Arc::new(index::WorkspaceIndex::new(
        workspace.clone(),
        &cfg.index,
    ));
    ws_index.spawn_reindex();

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

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ws, debug, idx, cfg_clone).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    workspace: PathBuf,
    debug: bool,
    ws_index: std::sync::Arc<index::WorkspaceIndex>,
    cfg: std::sync::Arc<config::Config>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut session = session::Session::new(workspace, cfg);
    session.index = Some(ws_index);
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
