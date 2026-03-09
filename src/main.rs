use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_tui::app;
use flotilla_tui::event_log;
use flotilla_tui::socket::SocketDaemon;

use clap::Parser;
use color_eyre::Result;
use std::path::PathBuf;
use std::sync::Arc;
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
    Status,
    /// Stream daemon events to stdout
    Watch,
}

impl Cli {
    fn config_dir(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(flotilla_core::config::flotilla_config_dir)
    }

    fn socket_path(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(|| self.config_dir().join("flotilla.sock"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match &cli.command {
        Some(SubCommand::Daemon { timeout }) => run_daemon(&cli, *timeout).await,
        Some(SubCommand::Status) => run_status(&cli).await,
        Some(SubCommand::Watch) => run_watch(&cli).await,
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
            "config loaded: {} repo(s) in {:.0?}",
            roots.len(),
            startup.elapsed()
        );
        roots
    } else {
        if !cli.repo_root.is_empty() {
            eprintln!(
                "Warning: --repo-root is ignored in socket mode (repos are managed by the daemon)"
            );
        }
        vec![]
    };

    // Spawn daemon init on a separate task so it runs concurrently with the splash
    // (show_splash uses blocking crossterm::event::poll calls).
    let config_clone = Arc::clone(&config);
    let daemon_task = tokio::spawn(async move {
        let daemon: Arc<dyn DaemonHandle> = if embedded {
            let d = InProcessDaemon::new(repo_roots, config_clone).await;
            d as Arc<dyn DaemonHandle>
        } else {
            let socket_path = cli.socket_path();
            flotilla_tui::socket::connect_or_spawn(
                &socket_path,
                &cli.config_dir(),
                cli.config_dir.as_deref(),
                cli.socket.as_deref(),
            )
            .await
            .map_err(|e| color_eyre::eyre::eyre!(e))
            .expect("failed to connect to daemon")
        };
        info!("daemon ready in {:.0?}", startup.elapsed());
        daemon
    });

    flotilla_tui::splash::show_splash(&mut terminal).await?;
    let daemon = daemon_task.await.expect("daemon init panicked");

    let repos_info = daemon.list_repos().await.unwrap_or_default();
    let app = app::App::new(daemon.clone(), repos_info, Arc::clone(&config));

    flotilla_tui::run::run_event_loop(terminal, app).await
}

async fn run_daemon(cli: &Cli, timeout_secs: u64) -> Result<()> {
    flotilla_daemon::cli::run(&cli.socket_path(), timeout_secs)
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_status(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    let daemon = SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon: {e}"))?;

    let repos = daemon
        .list_repos()
        .await
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;

    if repos.is_empty() {
        println!("No repos tracked.");
        return Ok(());
    }

    for repo in &repos {
        let name = &repo.name;
        let path = repo.path.display();
        let health: Vec<String> = repo
            .provider_health
            .iter()
            .map(|(k, v)| format!("{k}: {}", if *v { "ok" } else { "error" }))
            .collect();
        let loading = if repo.loading { " (loading)" } else { "" };
        println!("{name}{loading}  {path}");
        if !health.is_empty() {
            println!("  providers: {}", health.join(", "));
        }
    }

    Ok(())
}

async fn run_watch(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    let daemon = SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();
    println!("watching events (Ctrl-C to stop)...");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let json =
                    serde_json::to_string_pretty(&event).unwrap_or_else(|_| format!("{event:?}"));
                println!("{json}");
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("warning: skipped {n} events");
            }
            Err(_) => {
                eprintln!("daemon disconnected");
                break;
            }
        }
    }

    Ok(())
}
