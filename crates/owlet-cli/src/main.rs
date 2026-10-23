use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use clap::{Parser, Subcommand};
use owlet::{AdminClient, Config, Hub, Plugin};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Shares MCP servers over Streamable HTTP with lazy start and idle shutdown.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Path to the TOML config (default: ~/.config/owlet/config.toml).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the server (default when no subcommand is given).
    Serve,
    /// Show servers, their state and running instances.
    Status {
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Enable servers on the running owlet.
    Enable(Toggle),
    /// Disable servers on the running owlet; stops their processes and sessions.
    Disable(Toggle),
    /// Download the latest release from GitHub and replace this executable.
    Update {
        /// Only report whether a newer version exists.
        #[arg(long)]
        check: bool,
    },
    /// Install, list and remove WASM plugins (stored in `plugins/` next to the config).
    #[command(subcommand)]
    Plugin(PluginCmd),
}

#[derive(Debug, Subcommand)]
enum PluginCmd {
    /// Show the plugin catalog with install and enable state.
    List,
    /// Install a plugin and add its `[servers.<name>]` entry.
    Install {
        /// Plugin name from `owlet plugin list`.
        name: String,
        /// Install this local `.wasm` instead of downloading the release asset.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Remove a plugin's server entries and its file.
    Uninstall {
        /// Plugin name from `owlet plugin list`.
        name: String,
    },
}

#[derive(Debug, clap::Args)]
struct Toggle {
    /// Server names as configured under `[servers.<name>]`.
    #[arg(required = true)]
    servers: Vec<String>,
    /// Also write `enabled` to the config file so the change survives restarts.
    #[arg(short, long)]
    persist: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let path = match cli.config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    match cli.command.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(path).await,
        Cmd::Status { json } => AdminClient::from_config(&path)?.status(json).await,
        Cmd::Enable(t) => toggle(&path, t, true).await,
        Cmd::Disable(t) => toggle(&path, t, false).await,
        Cmd::Update { check } => owlet::self_update(check).await,
        Cmd::Plugin(cmd) => plugin(&path, cmd).await,
    }
}

async fn toggle(path: &Path, t: Toggle, enabled: bool) -> anyhow::Result<()> {
    let admin = AdminClient::from_config(path)?;
    let mut failed = false;
    for server in &t.servers {
        if let Err(e) = admin.toggle(server, enabled, t.persist).await {
            eprintln!("{server}: {e:#}");
            failed = true;
        }
    }
    if failed {
        anyhow::bail!("some servers could not be updated");
    }
    Ok(())
}

async fn plugin(path: &Path, cmd: PluginCmd) -> anyhow::Result<()> {
    let dir = Plugin::dir(path)?;
    let mut servers = Config::load_servers(path)?;
    match cmd {
        PluginCmd::List => {
            for p in Plugin::catalog() {
                let enabled = |name: &str| servers.get(name).is_some_and(|s| s.enabled);
                let state = match p.server_name(&dir, &servers) {
                    _ if !p.file(&dir).exists() => "not installed".to_owned(),
                    Some(name) if enabled(name) => format!("installed, [servers.{name}]"),
                    Some(name) => format!("installed, [servers.{name}] disabled"),
                    None => "installed, no server entry".to_owned(),
                };
                println!("{:<12} {state}\n{:<12} {}", p.name, "", p.description);
            }
        }
        PluginCmd::Install { name, file } => {
            let p = Plugin::find(&name)?;
            // Fails on a conflicting entry before anything is written.
            p.add_server(&dir, &mut servers)?;
            let installed = match file {
                Some(src) => p.install_file(&dir, &src)?,
                None => p.install(&dir).await?,
            };
            Config::persist_servers(path, &servers)?;
            let server = p.server_name(&dir, &servers).unwrap_or(p.name);
            println!("installed {} as [servers.{server}]", installed.display());
            println!("restart a running owlet to apply");
        }
        PluginCmd::Uninstall { name } => {
            let p = Plugin::find(&name)?;
            let removed = p.remove_servers(&dir, &mut servers);
            Config::persist_servers(path, &servers)?;
            p.uninstall(&dir)?;
            for server in &removed {
                println!("removed [servers.{server}]");
            }
            println!("removed {}", p.file(&dir).display());
            println!("restart a running owlet to apply");
        }
    }
    Ok(())
}

async fn serve(path: PathBuf) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("owlet=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::load(&path)?;
    if cfg.token.is_none() {
        tracing::warn!("no token configured; any local process can call these MCP servers");
    }

    let hub = Arc::new(Hub::new(&cfg));
    let min_idle = cfg
        .servers
        .values()
        .filter_map(|s| s.idle_timeout)
        .fold(cfg.idle_timeout, u64::min);
    let reap_every = Duration::from_secs((min_idle / 2).clamp(1, 15));
    let reaper = tokio::spawn(Arc::clone(&hub).reap_loop(reap_every));

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("bind {}", cfg.listen))?;
    let enabled = cfg.servers.values().filter(|s| s.enabled).count();
    info!(
        "listening on http://{} ({} servers, {enabled} enabled)",
        cfg.listen,
        cfg.servers.len()
    );

    axum::serve(listener, owlet::router(Arc::clone(&hub), cfg.token, path))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    reaper.abort();
    hub.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    info!("shutting down");
}
