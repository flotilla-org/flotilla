use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use color_eyre::Result;
use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{HostName, output::OutputFormat};
use flotilla_tui::{app, event_log};
use tracing::info;

/// Flotilla: TUI dashboard for managing development workspaces
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Git repo roots (repeatable; auto-detected from cwd if omitted)
    #[arg(long)]
    repo_root: Vec<PathBuf>,

    /// Config directory
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Socket path (default: ${config_dir}/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Run in embedded mode (no daemon, in-process)
    #[arg(long)]
    embedded: bool,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Run the daemon server
    Daemon {
        /// Idle timeout in seconds (0 = no timeout)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Print repo list and state
    Status {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Stream daemon events to stdout
    Watch {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Query a specific repo
    Repo {
        /// Repo path, name, or slug (e.g. "owner/repo")
        slug: String,
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<RepoSubCommand>,
    },
}

#[derive(clap::Subcommand)]
enum RepoSubCommand {
    /// Show provider discovery and instances
    Providers {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Show work items
    Work {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
}

impl Cli {
    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone().unwrap_or_else(flotilla_core::config::flotilla_config_dir)
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(|| self.config_dir().join("flotilla.sock"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match &cli.command {
        Some(SubCommand::Daemon { timeout }) => run_daemon(&cli, *timeout).await,
        Some(SubCommand::Status { json }) => run_status(&cli, OutputFormat::from_json_flag(*json)).await,
        Some(SubCommand::Watch { json }) => run_watch(&cli, OutputFormat::from_json_flag(*json)).await,
        Some(SubCommand::Repo { slug, json, command }) => run_repo(&cli, slug, OutputFormat::from_json_flag(*json), command.as_ref()).await,
        None => run_tui(cli).await,
    }
}

async fn run_tui(cli: Cli) -> Result<()> {
    event_log::init();
    let startup = std::time::Instant::now();
    let config = Arc::new(ConfigStore::new());

    // Initialize terminal and show splash immediately for fast visual feedback.
    // Mouse capture is enabled AFTER the splash so mouse events don't cut it short.
    let mut terminal = ratatui::init();

    // Resolve repos before splash (fast — just reads config files).
    let embedded = cli.embedded;
    let repo_roots = if embedded {
        let roots = flotilla_core::config::resolve_repo_roots(&cli.repo_root, &config);
        if roots.is_empty() {
            ratatui::restore();
            eprintln!("Error: no git repositories found (use --repo-root to specify)");
            std::process::exit(1);
        }
        info!(
            repo_count = roots.len(),
            elapsed = ?startup.elapsed(),
            "config loaded"
        );
        roots
    } else {
        vec![]
    };

    let cli_repo_roots = cli.repo_root.clone();

    // Spawn daemon init on a separate task so it runs concurrently with the splash
    // (show_splash uses blocking crossterm::event::poll calls).
    let daemon_log_path = cli.config_dir().join("daemon.log");
    let config_clone = Arc::clone(&config);
    let daemon_task = tokio::spawn(async move {
        let daemon: Result<Arc<dyn DaemonHandle>, String> = if embedded {
            // Load daemon config for host name (peer identity)
            let daemon_config = config_clone.load_daemon_config();
            let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
            let discovery = flotilla_core::providers::discovery::DiscoveryRuntime::for_process(daemon_config.follower);
            let d = InProcessDaemon::new(repo_roots, Arc::clone(&config_clone), discovery, host_name).await;

            // Spawn peer networking if peers are configured
            match flotilla_daemon::peer_networking::PeerNetworkingTask::new(Arc::clone(&d), &config_clone) {
                Ok((peer_networking, _peer_manager, _peer_data_tx)) => {
                    let _ = peer_networking.spawn();
                }
                Err(e) => {
                    tracing::warn!(err = %e, "peer networking not started in embedded mode");
                }
            }

            Ok(d as Arc<dyn DaemonHandle>)
        } else {
            let socket_path = cli.socket_path();
            flotilla_tui::socket::connect_or_spawn(&socket_path, &cli.config_dir(), cli.config_dir.as_deref(), cli.socket.as_deref())
                .await
                .map(|d| d as Arc<dyn DaemonHandle>)
        };
        daemon
    });

    flotilla_tui::splash::show_splash(&mut terminal).await?;
    let daemon = match daemon_task.await {
        Ok(Ok(daemon)) => {
            info!(elapsed = ?startup.elapsed(), "daemon ready");
            daemon
        }
        Ok(Err(e)) => {
            ratatui::restore();
            eprintln!("Error: {e}");
            eprintln!("  Check daemon log at {}", daemon_log_path.display());
            std::process::exit(1);
        }
        Err(e) => {
            ratatui::restore();
            eprintln!("Error: daemon initialization panicked: {e}");
            std::process::exit(1);
        }
    };

    // Forward --repo-root paths to the daemon (socket mode only;
    // in-process mode already received them via InProcessDaemon::new).
    if !embedded {
        for root in &cli_repo_roots {
            let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if let Err(e) = daemon.add_repo(&canonical).await {
                info!(repo = %canonical.display(), err = %e, "failed to add repo");
            }
        }
    }

    let repos_info = daemon.list_repos().await.unwrap_or_default();
    let app = app::App::new(daemon.clone(), repos_info, Arc::clone(&config));

    flotilla_tui::run::run_event_loop(terminal, app).await
}

async fn run_daemon(cli: &Cli, timeout_secs: u64) -> Result<()> {
    flotilla_daemon::cli::run(&cli.socket_path(), timeout_secs).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

/// Reset SIGPIPE so piped CLI commands (e.g. `watch | head`) exit cleanly.
/// Only called for CLI subcommands — not the TUI (which needs terminal restore on exit)
/// or the daemon (which shouldn't be killed by a broken stdout pipe).
#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY: libc::signal is safe to call before I/O begins. Tokio does not configure SIGPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

async fn run_status(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    flotilla_tui::cli::run_status(&cli.socket_path(), format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_watch(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    flotilla_tui::cli::run_watch(&cli.socket_path(), format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_repo(cli: &Cli, slug: &str, format: OutputFormat, command: Option<&RepoSubCommand>) -> Result<()> {
    reset_sigpipe();
    let socket_path = cli.socket_path();
    let config_dir = cli.config_dir();
    let daemon = flotilla_tui::socket::connect_or_spawn(&socket_path, &config_dir, cli.config_dir.as_deref(), cli.socket.as_deref())
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let result = match command {
        None => flotilla_tui::cli::run_repo_detail(&*daemon, slug, format).await,
        Some(RepoSubCommand::Providers { json: sub_json }) => {
            let fmt = if *sub_json { OutputFormat::Json } else { format };
            flotilla_tui::cli::run_repo_providers(&*daemon, slug, fmt).await
        }
        Some(RepoSubCommand::Work { json: sub_json }) => {
            let fmt = if *sub_json { OutputFormat::Json } else { format };
            flotilla_tui::cli::run_repo_work(&*daemon, slug, fmt).await
        }
    };
    result.map_err(|e| color_eyre::eyre::eyre!(e))
}
