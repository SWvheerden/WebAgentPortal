//! claude-web — a local multi-agent Claude Code server.
//!
//! Binds loopback by default. The OS is not the security boundary: the agents
//! execute arbitrary code as this user, so every data route carries a per-boot
//! token (§7). Binding anything other than loopback is opt-in, is restricted to
//! private and tailnet addresses, and requires a paired device key (§12).

mod agent;
mod config;
mod db;
mod remote;
mod repo;
mod web;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use crate::agent::process;
use crate::agent::supervisor::{ServerMsg, Supervisor};
use crate::config::Config;
use crate::db::Db;
use crate::remote::RemoteKey;
use crate::web::routes::{AppState, HostPolicy};

#[derive(Debug, Parser)]
#[command(name = "claude-web", about = "Local multi-agent Claude Code server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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

#[derive(Debug, Subcommand)]
enum Command {
    /// Pair a device: generate a key, store its hash, print a QR code (§12).
    Pair,
    /// Forget the paired key. Also the switch that turns remote access off.
    Unpair,
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
    // `pair` and `unpair` touch only the key file. Neither starts a server, and
    // neither may be gated on a bind being valid, since pairing is what makes a
    // non-loopback bind valid in the first place.
    if let Some(command) = &cli.command {
        return run_command(command, &config_path, cli.port);
    }
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
    let bind_ip = cfg.bind_ip()?;
    let hostnames = cfg.hostnames.clone();
    let claude_bin = cfg.claude_bin.clone();
    let pinned = cfg.pinned_cli_version.clone();
    let sup = Supervisor::new(db, Arc::new(RwLock::new(cfg)));
    // The account's usage outlives the process; the panel should not have to
    // wait for an agent to run before it can say anything.
    sup.restore_rate_limit().await;

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

    // The token lives only here and in the URL below: never on disk, never in a
    // log, never in a served page.
    let token = Arc::new(web::routes::SessionToken::mint());
    // The durable key of §12, if a device has been paired. Only its hash is on
    // disk, and only the hash is ever held here.
    let key_path = remote::key_path();
    let remote_key = RemoteKey::load(&key_path)
        .with_context(|| format!("loading {}", key_path.display()))?
        .map(Arc::new);
    if !bind_ip.is_loopback() && remote_key.is_none() {
        anyhow::bail!(
            "bind = \"{bind_ip}\" is not loopback and no device is paired. Run `claude-web pair`, \
             or set bind = \"127.0.0.1\" in {}.",
            config_path.display()
        );
    }
    let state = AppState {
        sup: sup.clone(),
        config_path,
        hosts: Arc::new(HostPolicy::new(port, bind_ip, &hostnames)),
        token: token.clone(),
        remote: remote_key,
        refusals: Default::default(),
    };
    let app = web::routes::router(state);

    // The loopback listener is always one of these, and is the one the browser
    // is pointed at below.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let addrs = listen_addrs(bind_ip, port);
    let remote_addr = SocketAddr::new(bind_ip, port);
    let mut listeners = Vec::with_capacity(addrs.len());
    for addr in &addrs {
        listeners.push(
            tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr}"))?,
        );
    }
    // The token travels in the URL the browser is opened with; the page keeps
    // it in sessionStorage and strips it from the address bar.
    let url = format!("http://{addr}/?t={}", token.as_str());
    tracing::info!(port, "claude-web listening on loopback");

    // Printed only to a terminal. Redirecting stdout to a file is the ordinary
    // way to run this as a background service, and that would put the token in
    // that file.
    let handoff = write_handoff(&url);
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("\nclaude-web is at:\n\n    {url}\n");
        println!("That link carries this run's session token. It changes on every restart,");
        println!("and without it the API refuses the request.\n");
    } else {
        println!("claude-web is listening on http://{addr}/");
        match &handoff {
            Some(path) => println!(
                "This run's session token is in {} (mode 0600) — open the link it contains.",
                path.display()
            ),
            None => println!(
                "Run with a terminal attached to be shown the link carrying the session token."
            ),
        }
    }

    // Plainly, and without the durable key: it is never printed at startup and
    // never reaches an `open` command line, for the same reason the per-boot
    // token does not — an argv is readable by every process on the machine.
    if !bind_ip.is_loopback() {
        tracing::info!(%remote_addr, "claude-web is reachable from the network");
        println!("This portal is also reachable from the network, at:\n");
        println!("    http://{remote_addr}/\n");
        println!("Only devices paired with `claude-web pair` can use it. `claude-web unpair`");
        println!("turns it off again.\n");
    }

    if open_browser {
        // Open a private local file that redirects, rather than passing the
        // tokened URL to `open`: a command line is readable by every process on
        // the machine, if only for a moment.
        let target = handoff
            .clone()
            .map(|p| format!("file://{}", p.display()))
            .unwrap_or_else(|| url.clone());
        if let Err(err) = open::that_detached(&target) {
            tracing::warn!(?err, "could not open a browser");
        }
    }

    // One signal, however many listeners: each drains when the watch flips.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_sup = sup.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("shutting down: SIGTERM to every agent, then 5s");
        shutdown_sup.shutdown().await;
        let _ = shutdown_tx.send(true);
    });

    let mut servers = Vec::new();
    for listener in listeners {
        let app = app.clone();
        let mut rx = shutdown_rx.clone();
        servers.push(tokio::spawn(async move {
            // The peer address decides which credential is acceptable, so the
            // service is mounted with connect info rather than without (§12).
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = rx.changed().await;
            })
            .await
        }));
    }
    for server in servers {
        server.await.context("serving")?.context("serving")?;
    }

    // A second pass, in case anything started during the drain.
    sup.shutdown().await;
    Ok(())
}

/// Every address to listen on, v4 loopback first.
///
/// Loopback is always served, whatever `bind` says: the local browser is opened
/// on the loopback tokened URL, and the per-boot token is refused from anywhere
/// else (§12). `bind` therefore adds a listener rather than replacing one — and
/// it must actually add it, including for the `::1` case, or a configuration
/// this accepts would be one it silently ignores.
fn listen_addrs(bind_ip: IpAddr, port: u16) -> Vec<SocketAddr> {
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let bound = SocketAddr::new(bind_ip, port);
    if bound == loopback {
        vec![loopback]
    } else {
        vec![loopback, bound]
    }
}

/// `claude-web pair` and `claude-web unpair` (§12).
fn run_command(command: &Command, config_path: &std::path::Path, port: Option<u16>) -> Result<()> {
    let key_path = remote::key_path();
    match command {
        Command::Pair => {
            let cfg = Config::load_lenient(config_path)
                .with_context(|| format!("loading {}", config_path.display()))?;
            let port = port.unwrap_or(cfg.port);
            let bind = cfg.bind.trim();
            // If a hostname is configured at all, that is the way in.
            let host = cfg
                .hostnames
                .iter()
                .map(|h| h.trim())
                .find(|h| !h.is_empty())
                .unwrap_or(bind);
            let host = match host.parse::<IpAddr>() {
                // An IPv6 literal needs its brackets back in a URL.
                Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
                _ => host.to_string(),
            };
            remote::pair(&key_path, &format!("http://{host}:{port}/"))?;
            if bind.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
                println!(
                    "Note: bind is still \"{bind}\", so this key only works from this machine.\n\
                     Set `bind` in {} to a private or tailnet address to reach it from a phone.\n",
                    config_path.display()
                );
            }
            Ok(())
        }
        Command::Unpair => {
            if remote::unpair(&key_path)? {
                println!(
                    "Deleted {}. Every paired device is now refused, and a non-loopback bind \
                     will not start.",
                    key_path.display()
                );
            } else {
                println!(
                    "No device was paired ({} does not exist).",
                    key_path.display()
                );
            }
            Ok(())
        }
    }
}

/// Write the tokened URL to a private file for the browser to be pointed at.
///
/// Mode 0600, so it is no more exposed than the browser profile that will hold
/// the token anyway — and unlike a command line, not readable by every process.
fn write_handoff(url: &str) -> Option<PathBuf> {
    let path = config::state_dir().join("session-url.html");
    std::fs::create_dir_all(config::state_dir()).ok()?;
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\">\
         <meta http-equiv=\"refresh\" content=\"0; url={url}\">\
         <title>claude-web</title>\
         <p>Opening <a href=\"{url}\">claude-web</a>…"
    );
    std::fs::write(&path, body).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok()?;
    }
    Some(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v6_loopback_bind_opens_a_v6_socket() {
        // `bind = "::1"` is accepted by `Config::validate`, so it has to mean
        // something: a browser sent to `http://localhost:7717` that resolves
        // localhost to ::1 must find something listening.
        assert_eq!(
            listen_addrs("::1".parse().expect("ip"), 7717),
            vec![
                "127.0.0.1:7717".parse::<SocketAddr>().expect("addr"),
                "[::1]:7717".parse::<SocketAddr>().expect("addr"),
            ]
        );

        // The default binds once, not twice: the loopback listener is the one
        // `bind` already names.
        assert_eq!(
            listen_addrs(
                config::DEFAULT_BIND.parse().expect("ip"),
                config::DEFAULT_PORT
            ),
            vec!["127.0.0.1:7717".parse::<SocketAddr>().expect("addr")]
        );

        // A remote bind is served alongside loopback, not instead of it.
        assert_eq!(
            listen_addrs("100.64.0.7".parse().expect("ip"), 7717),
            vec![
                "127.0.0.1:7717".parse::<SocketAddr>().expect("addr"),
                "100.64.0.7:7717".parse::<SocketAddr>().expect("addr"),
            ]
        );

        // Every address `Config::validate` accepts gets a listener of its own.
        for bind in ["::1", "10.0.0.4", "192.168.1.5", "172.16.9.9", "100.64.0.7"] {
            let ip: IpAddr = bind.parse().expect("ip");
            assert!(
                config::bind_allowed(ip),
                "{bind} is part of the accepted shape"
            );
            assert!(
                listen_addrs(ip, 7717).contains(&SocketAddr::new(ip, 7717)),
                "{bind} is accepted by validate and must actually be bound"
            );
        }
    }
}
