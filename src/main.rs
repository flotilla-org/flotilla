use std::{ffi::OsString, path::PathBuf, sync::Arc};

use clap::Parser;
use color_eyre::Result;
use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{output::OutputFormat, CheckoutSelector, CheckoutTarget, Command, CommandAction, HostName, RepoSelector};
use flotilla_tui::{app, event_log, theme};
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

    /// Theme name (catppuccin-mocha, classic)
    #[arg(long)]
    theme: Option<String>,

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
    /// Trigger a refresh for one repo or all tracked repos
    Refresh {
        /// Repo path, name, or slug (e.g. "owner/repo")
        repo: Option<String>,
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Query or control repositories
    Repo {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
        /// Repo arguments, e.g. `owner/repo`, `add /path`, `remove owner/repo`,
        /// or `owner/repo checkout --fresh feature/x`
        #[arg(value_name = "ARGS", num_args = 1.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Remove a checkout
    Checkout {
        /// Checkout path or branch name
        checkout: String,
        #[command(subcommand)]
        command: CheckoutSubCommand,
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Route a control command to a specific host
    Host {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
        /// Host query or control arguments
        #[arg(value_name = "ARGS", num_args = 1.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show the daemon's current multi-host routing view
    Topology {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
enum CheckoutSubCommand {
    Remove,
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
    let cli = try_parse_cli_from(std::env::args_os()).unwrap_or_else(|err| err.exit());

    match &cli.command {
        Some(SubCommand::Daemon { timeout }) => run_daemon(&cli, *timeout).await,
        Some(SubCommand::Status { json }) => run_status(&cli, OutputFormat::from_json_flag(*json)).await,
        Some(SubCommand::Watch { json }) => run_watch(&cli, OutputFormat::from_json_flag(*json)).await,
        Some(SubCommand::Refresh { repo, json }) => {
            run_control_command(
                &cli,
                Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::Refresh { repo: repo.as_ref().map(|value| RepoSelector::Query(value.clone())) },
                },
                OutputFormat::from_json_flag(*json),
            )
            .await
        }
        Some(SubCommand::Repo { args, json }) => run_repo(&cli, args, OutputFormat::from_json_flag(*json)).await,
        Some(SubCommand::Checkout { checkout, command: CheckoutSubCommand::Remove, json }) => {
            run_control_command(
                &cli,
                Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(checkout.clone()), terminal_keys: vec![] },
                },
                OutputFormat::from_json_flag(*json),
            )
            .await
        }
        Some(SubCommand::Host { args, json }) => run_host(&cli, args, OutputFormat::from_json_flag(*json)).await,
        Some(SubCommand::Topology { json }) => run_topology_command(&cli, OutputFormat::from_json_flag(*json)).await,
        None => run_tui(cli).await,
    }
}

fn try_parse_cli_from<I, T>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    Cli::try_parse_from(normalize_cli_args(args))
}

fn normalize_cli_args<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.last().and_then(|value| value.to_str()) != Some("--json") {
        return args;
    }

    let Some(subcommand_idx) = find_subcommand_index(&args) else {
        return args;
    };
    let Some(subcommand) = args[subcommand_idx].to_str() else {
        return args;
    };
    // Only `repo` and `host` need normalization because they capture a trailing
    // variadic positional. `topology --json` parses directly as a named flag.
    if !matches!(subcommand, "repo" | "host") || subcommand_idx + 1 >= args.len() - 1 {
        return args;
    }

    let json = args.pop().expect("checked trailing --json");
    args.insert(subcommand_idx + 1, json);
    args
}

fn find_subcommand_index(args: &[OsString]) -> Option<usize> {
    let mut idx = 1;
    while idx < args.len() {
        match args[idx].to_str() {
            Some("--embedded") => idx += 1,
            Some("--repo-root") | Some("--config-dir") | Some("--socket") | Some("--theme") => idx += 2,
            Some(value)
                if value.starts_with("--embedded=")
                    || value.starts_with("--repo-root=")
                    || value.starts_with("--config-dir=")
                    || value.starts_with("--socket=")
                    || value.starts_with("--theme=") =>
            {
                idx += 1;
            }
            Some(_) => return Some(idx),
            None => return None,
        }
    }
    None
}

async fn run_tui(cli: Cli) -> Result<()> {
    event_log::init();
    let startup = std::time::Instant::now();
    let config = Arc::new(ConfigStore::new());

    // Initialize terminal and show splash immediately for fast visual feedback.
    // Mouse capture is enabled AFTER the splash so mouse events don't cut it short.
    let mut terminal = ratatui::init();
    flotilla_tui::terminal::install_panic_hook();
    #[cfg(unix)]
    flotilla_tui::terminal::install_sigterm_handler();

    // Resolve repos before splash (fast — just reads config files).
    let embedded = cli.embedded;
    let repo_roots = if embedded {
        let roots = flotilla_core::config::resolve_repo_roots(&cli.repo_root, &config);
        if roots.is_empty() {
            flotilla_tui::terminal::restore_terminal();
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
    let cli_theme = cli.theme.clone();

    // Spawn daemon init on a separate task so it runs concurrently with the splash
    // (show_splash uses blocking crossterm::event::poll calls).
    let daemon_log_path = cli.config_dir().join("daemon.log");
    let config_clone = Arc::clone(&config);
    let daemon_task = tokio::spawn(async move {
        let daemon: Result<Arc<dyn DaemonHandle>, String> = if embedded {
            let daemon_config = config_clone.load_daemon_config();
            let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
            let discovery = flotilla_core::providers::discovery::DiscoveryRuntime::for_process(daemon_config.follower);
            let d = InProcessDaemon::new(repo_roots, Arc::clone(&config_clone), discovery, host_name).await;

            match flotilla_daemon::server::spawn_embedded_peer_networking(Arc::clone(&d), &config_clone) {
                Ok(_peer_networking) => {
                    // Detached background task; the TUI owns the daemon handle.
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
            flotilla_tui::terminal::restore_terminal();
            eprintln!("Error: {e}");
            eprintln!("  Check daemon log at {}", daemon_log_path.display());
            std::process::exit(1);
        }
        Err(e) => {
            flotilla_tui::terminal::restore_terminal();
            eprintln!("Error: daemon initialization panicked: {e}");
            std::process::exit(1);
        }
    };

    // Forward --repo-root paths to the daemon (socket mode only;
    // in-process mode already received them via InProcessDaemon::new).
    if !embedded {
        for root in &cli_repo_roots {
            let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if let Err(e) = daemon
                .execute(flotilla_protocol::Command {
                    host: None,
                    context_repo: None,
                    action: flotilla_protocol::CommandAction::TrackRepoPath { path: canonical.clone() },
                })
                .await
            {
                info!(repo = %canonical.display(), err = %e, "failed to add repo");
            }
        }
    }

    let theme_name = cli_theme.or_else(|| config.load_config().ui.theme.clone()).unwrap_or_else(|| "catppuccin-mocha".to_string());
    let initial_theme = theme::theme_by_name(&theme_name);
    if !initial_theme.name.eq_ignore_ascii_case(&theme_name) {
        tracing::warn!(requested = %theme_name, using = %initial_theme.name, "unknown theme, falling back");
    }

    let repos_info = daemon.list_repos().await.unwrap_or_default();
    let app = app::App::new(daemon.clone(), repos_info, Arc::clone(&config), initial_theme);

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

enum RepoCommand {
    Query { slug: String, detail: Option<RepoQueryCommand> },
    Control(Command),
}

enum RepoQueryCommand {
    Providers,
    Work,
}

enum HostCommand {
    List,
    Query { host: String, detail: HostQueryCommand },
    Control(Command),
}

enum HostQueryCommand {
    Status,
    Providers,
}

fn parse_repo_command(args: &[String]) -> Result<RepoCommand, String> {
    if args.is_empty() {
        return Err("missing repo arguments".into());
    }

    match args[0].as_str() {
        "add" if args.len() == 2 => Ok(RepoCommand::Control(Command {
            host: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: PathBuf::from(&args[1]) },
        })),
        "remove" if args.len() == 2 => Ok(RepoCommand::Control(Command {
            host: None,
            context_repo: None,
            action: CommandAction::UntrackRepo { repo: RepoSelector::Query(args[1].clone()) },
        })),
        slug => {
            if args.len() == 1 {
                return Ok(RepoCommand::Query { slug: slug.into(), detail: None });
            }
            if args.len() == 2 {
                return match args[1].as_str() {
                    "providers" => Ok(RepoCommand::Query { slug: slug.into(), detail: Some(RepoQueryCommand::Providers) }),
                    "work" => Ok(RepoCommand::Query { slug: slug.into(), detail: Some(RepoQueryCommand::Work) }),
                    _ => Err(format!("unrecognized repo command: {}", args[1])),
                };
            }
            if args.len() == 3 && args[1] == "checkout" {
                return Ok(RepoCommand::Control(Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::Checkout {
                        repo: RepoSelector::Query(slug.into()),
                        target: CheckoutTarget::Branch(args[2].clone()),
                        issue_ids: vec![],
                    },
                }));
            }
            if args.len() == 4 && args[1] == "checkout" && args[2] == "--fresh" {
                return Ok(RepoCommand::Control(Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::Checkout {
                        repo: RepoSelector::Query(slug.into()),
                        target: CheckoutTarget::FreshBranch(args[3].clone()),
                        issue_ids: vec![],
                    },
                }));
            }
            Err("unsupported repo arguments".into())
        }
    }
}

fn parse_host_control_command(host: &str, args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Err("missing host command".into());
    }

    let mut command = match args[0].as_str() {
        "refresh" => Command {
            host: None,
            context_repo: None,
            action: CommandAction::Refresh { repo: args.get(1).cloned().map(RepoSelector::Query) },
        },
        "repo" => match parse_repo_command(&args[1..])? {
            RepoCommand::Control(command) => command,
            RepoCommand::Query { .. } => return Err("host only supports control commands".into()),
        },
        "checkout" if args.len() == 3 && args[2] == "remove" => Command {
            host: None,
            context_repo: None,
            action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(args[1].clone()), terminal_keys: vec![] },
        },
        _ => return Err("unsupported host control command".into()),
    };
    command.host = Some(flotilla_protocol::HostName::new(host));
    Ok(command)
}

fn parse_host_command(args: &[String]) -> Result<HostCommand, String> {
    if args.is_empty() {
        return Err("missing host command".into());
    }

    // `host list` is the only bare query form, so a peer literally named
    // "list" cannot currently be addressed without additional syntax.
    if args.len() == 1 && args[0] == "list" {
        return Ok(HostCommand::List);
    }

    let host = &args[0];
    let host_args = &args[1..];
    if host_args.is_empty() {
        return Err("missing host command".into());
    }

    match host_args {
        [detail] if detail == "status" => Ok(HostCommand::Query { host: host.clone(), detail: HostQueryCommand::Status }),
        [detail] if detail == "providers" => Ok(HostCommand::Query { host: host.clone(), detail: HostQueryCommand::Providers }),
        _ => parse_host_control_command(host, host_args).map(HostCommand::Control),
    }
}

async fn connect_daemon(cli: &Cli) -> Result<Arc<dyn DaemonHandle>> {
    let socket_path = cli.socket_path();
    let config_dir = cli.config_dir();
    let daemon = flotilla_tui::socket::connect_or_spawn(&socket_path, &config_dir, cli.config_dir.as_deref(), cli.socket.as_deref())
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;
    Ok(daemon as Arc<dyn DaemonHandle>)
}

async fn run_control_command(cli: &Cli, command: Command, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let daemon = connect_daemon(cli).await?;
    flotilla_tui::cli::run_command(&*daemon, command, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_repo(cli: &Cli, args: &[String], format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let parsed = parse_repo_command(args).map_err(|e| color_eyre::eyre::eyre!(e))?;
    match parsed {
        RepoCommand::Control(command) => run_control_command(cli, command, format).await,
        RepoCommand::Query { slug, detail } => {
            let daemon = connect_daemon(cli).await?;
            let result = match detail {
                None => flotilla_tui::cli::run_repo_detail(&*daemon, &slug, format).await,
                Some(RepoQueryCommand::Providers) => flotilla_tui::cli::run_repo_providers(&*daemon, &slug, format).await,
                Some(RepoQueryCommand::Work) => flotilla_tui::cli::run_repo_work(&*daemon, &slug, format).await,
            };
            result.map_err(|e| color_eyre::eyre::eyre!(e))
        }
    }
}

async fn run_host(cli: &Cli, args: &[String], format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let parsed = parse_host_command(args).map_err(|e| color_eyre::eyre::eyre!(e))?;
    match parsed {
        HostCommand::List => {
            let daemon = connect_daemon(cli).await?;
            flotilla_tui::cli::run_host_list(&*daemon, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
        }
        HostCommand::Query { host, detail } => {
            let daemon = connect_daemon(cli).await?;
            let result = match detail {
                HostQueryCommand::Status => flotilla_tui::cli::run_host_status(&*daemon, &host, format).await,
                HostQueryCommand::Providers => flotilla_tui::cli::run_host_providers(&*daemon, &host, format).await,
            };
            result.map_err(|e| color_eyre::eyre::eyre!(e))
        }
        HostCommand::Control(command) => run_control_command(cli, command, format).await,
    }
}

async fn run_topology_command(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let daemon = connect_daemon(cli).await?;
    flotilla_tui::cli::run_topology(&*daemon, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{CheckoutSelector, CommandAction, RepoSelector};

    use super::{parse_host_command, try_parse_cli_from, HostCommand, HostQueryCommand, SubCommand};

    #[test]
    fn parse_host_command_list() {
        let parsed = parse_host_command(&["list".into()]).expect("host list should parse");
        assert!(matches!(parsed, HostCommand::List));
    }

    #[test]
    fn parse_host_command_status() {
        let parsed = parse_host_command(&["alpha".into(), "status".into()]).expect("host status should parse");
        assert!(matches!(
            parsed,
            HostCommand::Query { host, detail: HostQueryCommand::Status } if host == "alpha"
        ));
    }

    #[test]
    fn parse_host_command_providers() {
        let parsed = parse_host_command(&["alpha".into(), "providers".into()]).expect("host providers should parse");
        assert!(matches!(
            parsed,
            HostCommand::Query { host, detail: HostQueryCommand::Providers } if host == "alpha"
        ));
    }

    #[test]
    fn parse_host_command_preserves_control_paths() {
        let parsed = parse_host_command(&["alpha".into(), "repo".into(), "remove".into(), "owner/repo".into()])
            .expect("host repo remove should parse");
        assert!(matches!(
            parsed,
            HostCommand::Control(command)
                if command.host.as_ref().map(|host| host.as_str()) == Some("alpha")
                    && matches!(command.action, CommandAction::UntrackRepo { repo: RepoSelector::Query(ref value) } if value == "owner/repo")
        ));

        let parsed = parse_host_command(&["alpha".into(), "checkout".into(), "/tmp/wt".into(), "remove".into()])
            .expect("host checkout remove should parse");
        assert!(matches!(
            parsed,
            HostCommand::Control(command)
                if matches!(command.action, CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(ref value), .. } if value == "/tmp/wt")
        ));
    }

    #[test]
    fn cli_parses_topology_subcommand() {
        let cli = try_parse_cli_from(["flotilla", "topology"]).expect("topology cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Topology { json: false })));
    }

    #[test]
    fn cli_parses_host_list_with_trailing_json() {
        let cli = try_parse_cli_from(["flotilla", "host", "list", "--json"]).expect("host list json should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Host { args, json: true }) if args == vec!["list"]
        ));
    }

    #[test]
    fn cli_parses_host_status_with_trailing_json() {
        let cli = try_parse_cli_from(["flotilla", "host", "alpha", "status", "--json"]).expect("host status json should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Host { args, json: true }) if args == vec!["alpha", "status"]
        ));
    }

    #[test]
    fn cli_parses_host_providers_with_trailing_json() {
        let cli = try_parse_cli_from(["flotilla", "host", "alpha", "providers", "--json"]).expect("host providers json should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Host { args, json: true }) if args == vec!["alpha", "providers"]
        ));
    }

    #[test]
    fn cli_parses_repo_query_with_trailing_json() {
        let cli = try_parse_cli_from(["flotilla", "repo", "owner/repo", "--json"]).expect("repo json should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Repo { args, json: true }) if args == vec!["owner/repo"]
        ));
    }
}
