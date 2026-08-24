//! claude-web — a local multi-agent Claude Code server.
//!
//! Binds loopback only. The OS is the security boundary: the agents execute
//! arbitrary code, so this must never listen on a non-loopback interface
//! without authentication (§7).

mod agent;
mod config;
mod db;
mod repo;
mod web;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use crate::agent::process;
use crate::agent::supervisor::{ServerMsg, Supervisor};
use crate::config::Config;
use crate::db::Db;
use crate::web::routes::AppState;

#[derive(Debug, Parser)]
#[command(name = "claude-web", about = "Local multi-agent Claude Code server")]
struct Cli {
    /// Override the configured port.
    #[arg(long)]
    port: Option<u16>,
    /// Path to config.toml (default: ~/.claude-web/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Path to the SQLite database (default: ~/.claude-web/agents.db).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Do not open a browser on startup.
    #[arg(long)]
    no_open: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("claude_web=info,warn")),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    let mut cfg = Config::load_or_create(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    let open_browser = cfg.open_browser && !cli.no_open;

    let db_path = cli.db.clone().unwrap_or_else(config::default_db_path);
    let db = Db::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    // Agents do not survive server death (§10), so anything left running is stale.
    let stale = db.mark_all_stopped()?;
    if stale > 0 {
        tracing::info!(
            stale,
            "marked stale agents as stopped; Resume will relaunch them"
        );
    }

    let port = cfg.port;
    let claude_bin = cfg.claude_bin.clone();
    let pinned = cfg.pinned_cli_version.clone();
    let sup = Supervisor::new(db, Arc::new(RwLock::new(cfg)));

    // The stream-json protocol carries no stability guarantee, so check the CLI
    // version against the pinned one and say so loudly on a mismatch.
    {
        let sup = sup.clone();
        tokio::spawn(async move {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match process::cli_version(&claude_bin, &cwd).await {
                Ok(found) if found == pinned => {
                    tracing::info!(version = %found, "claude CLI version matches the pin")
                }
                Ok(found) => {
                    let text = format!(
                        "claude CLI is {found}, but this build was verified against {pinned}. \
                         The stream-json protocol is undocumented and may have changed."
                    );
                    tracing::warn!("{text}");
                    sup.broadcast(ServerMsg::Notice {
                        agent_id: None,
                        level: "warn".to_string(),
                        text,
                    });
                }
                Err(err) => {
                    let text = format!("Could not run `{claude_bin} --version`: {err:#}");
                    tracing::warn!("{text}");
                    sup.broadcast(ServerMsg::Notice {
                        agent_id: None,
                        level: "error".to_string(),
                        text,
                    });
                }
            }
        });
    }

    let state = AppState {
        sup: sup.clone(),
        config_path,
        port,
    };
    let app = web::routes::router(state);

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let url = format!("http://{addr}/");
    tracing::info!(%url, "claude-web listening");

    if open_browser && let Err(err) = open::that_detached(&url) {
        tracing::warn!(?err, "could not open a browser");
    }

    let shutdown_sup = sup.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            tracing::info!("shutting down: SIGTERM to every agent, then 5s");
            shutdown_sup.shutdown().await;
        })
        .await
        .context("serving")?;

    // A second pass, in case anything started during the drain.
    sup.shutdown().await;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(?err, "cannot listen for SIGTERM; using ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
