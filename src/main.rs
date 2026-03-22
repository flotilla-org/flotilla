use std::{ffi::OsString, path::PathBuf, sync::Arc};

use clap::Parser;
use color_eyre::Result;
use flotilla_core::{agents, config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    output::OutputFormat, AgentHookEvent, AttachableId, CheckoutSelector, CheckoutTarget, Command, CommandAction, HostName, RepoSelector,
};
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
    /// Receive agent hook events (called by agent hook systems)
    Hook {
        /// Agent harness name (e.g. claude-code, codex, gemini)
        harness: String,
        /// Event type (e.g. session-start, stop, notification)
        event_type: String,
    },
    /// Install or uninstall agent hook configuration
    Hooks {
        #[command(subcommand)]
        command: HooksSubCommand,
    },
}

#[derive(clap::Subcommand)]
enum HooksSubCommand {
    /// Install hooks for an agent harness
    Install {
        /// Agent harness (e.g. claude-code)
        harness: String,
        /// Install to user settings (~/.claude/settings.json)
        #[arg(long)]
        user: bool,
        /// Install to project settings (.claude/settings.json, committed)
        #[arg(long)]
        project: bool,
        /// Install to local project settings (.claude/settings.local.json, gitignored)
        #[arg(long)]
        local: bool,
        /// Show plugin marketplace install instructions instead
        #[arg(long)]
        plugin: bool,
    },
    /// Remove hooks for an agent harness
    Uninstall {
        /// Agent harness (e.g. claude-code)
        harness: String,
        /// Remove from user settings
        #[arg(long)]
        user: bool,
        /// Remove from project settings
        #[arg(long)]
        project: bool,
        /// Remove from local project settings
        #[arg(long)]
        local: bool,
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
        Some(SubCommand::Hook { harness, event_type }) => run_hook(&cli, harness, event_type).await,
        Some(SubCommand::Hooks { command }) => run_hooks_command(command).await,
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
        let roots = flotilla_core::config::resolve_repo_roots(&cli.repo_root, &config).await;
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
            if args.len() == 3 && args[1] == "prepare-terminal" {
                return Ok(RepoCommand::Control(Command {
                    host: None,
                    context_repo: Some(RepoSelector::Query(slug.into())),
                    action: CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from(&args[2]), commands: vec![] },
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
        "refresh" if args.len() <= 2 => Command {
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

async fn run_hook(cli: &Cli, harness: &str, event_type: &str) -> Result<()> {
    use std::io::Read;

    // 1. Resolve harness parser
    let (harness_enum, parser) = agents::parser_for_harness(harness).map_err(|e| color_eyre::eyre::eyre!("unknown harness: {e}"))?;

    // 2. Read native payload from stdin
    let mut payload = Vec::new();
    std::io::stdin().read_to_end(&mut payload).map_err(|e| color_eyre::eyre::eyre!("failed to read stdin: {e}"))?;

    // 3. Parse the event
    let parsed = parser.parse_event(event_type, &payload).map_err(|e| color_eyre::eyre::eyre!("parse error: {e}"))?;

    // 4. Resolve attachable_id from env, or allocate a fresh one.
    // When the daemon receives the event it handles session_id → attachable_id
    // mapping and persistence.
    let attachable_id = match std::env::var("FLOTILLA_ATTACHABLE_ID") {
        Ok(id) if !id.is_empty() => AttachableId::new(id),
        _ => agents::allocate_attachable_id(),
    };

    // 5. Build the event
    let event = AgentHookEvent {
        attachable_id,
        harness: harness_enum,
        event_type: parsed.event_type,
        session_id: parsed.session_id,
        model: parsed.model,
        cwd: parsed.cwd,
    };

    // 6. Send to daemon via socket. The daemon owns agent state as a single
    // actor — no file-level races between concurrent hook processes.
    // Priority: FLOTILLA_DAEMON_SOCKET env > --socket CLI flag > global default.
    let socket_path = std::env::var("FLOTILLA_DAEMON_SOCKET").map(std::path::PathBuf::from).unwrap_or_else(|_| cli.socket_path());

    send_hook_event(&socket_path, event).await
}

/// One-shot client: connect to daemon, send an AgentHook request, read one response, exit.
async fn send_hook_event(socket_path: &std::path::Path, event: AgentHookEvent) -> Result<()> {
    use flotilla_protocol::{framing, Message, Request, ResponseResult};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::UnixStream,
    };

    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("failed to connect to daemon at {}: {e}", socket_path.display()))?;

    let (reader, mut writer) = stream.into_split();

    // Send request
    let msg = Message::Request { id: 1, request: Request::AgentHook { event } };
    framing::write_message_line(&mut writer, &msg).await.map_err(|e| color_eyre::eyre::eyre!("write error: {e}"))?;

    // Read response
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await.map_err(|e| color_eyre::eyre::eyre!("read error: {e}"))?;

    let response: Message = serde_json::from_str(line.trim()).map_err(|e| color_eyre::eyre::eyre!("parse response: {e}"))?;
    match response {
        Message::Response { response, .. } => match *response {
            ResponseResult::Ok { .. } => Ok(()),
            ResponseResult::Err { message } => Err(color_eyre::eyre::eyre!("daemon error: {message}")),
        },
        other => Err(color_eyre::eyre::eyre!("unexpected response: {other:?}")),
    }
}

async fn run_hooks_command(command: &HooksSubCommand) -> Result<()> {
    match command {
        HooksSubCommand::Install { harness, user, project, local, plugin } => {
            if harness != "claude-code" {
                return Err(color_eyre::eyre::eyre!("unknown harness: {harness}. Supported: claude-code"));
            }

            if *plugin {
                println!("To install flotilla hooks as a Claude Code plugin:");
                println!();
                println!("  1. Add the marketplace:");
                println!("     /plugin marketplace add flotilla-org/marketplace");
                println!();
                println!("  2. Install the plugin:");
                println!("     /plugin install flotilla-hooks@flotilla-marketplace");
                return Ok(());
            }

            let scope = resolve_settings_scope(*user, *project, *local)?;
            let path = scope.path();

            install_claude_code_hooks(&path)?;
            println!("Installed flotilla hooks for claude-code in {}", path.display());
            Ok(())
        }
        HooksSubCommand::Uninstall { harness, user, project, local } => {
            if harness != "claude-code" {
                return Err(color_eyre::eyre::eyre!("unknown harness: {harness}. Supported: claude-code"));
            }

            let scope = resolve_settings_scope(*user, *project, *local)?;
            let path = scope.path();

            uninstall_claude_code_hooks(&path)?;
            println!("Removed flotilla hooks for claude-code from {}", path.display());
            Ok(())
        }
    }
}

enum SettingsScope {
    User,
    Project,
    Local,
}

impl SettingsScope {
    fn path(&self) -> PathBuf {
        match self {
            SettingsScope::User => {
                std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("~")).join(".claude/settings.json")
            }
            SettingsScope::Project => find_repo_root().join(".claude/settings.json"),
            SettingsScope::Local => find_repo_root().join(".claude/settings.local.json"),
        }
    }
}

/// Walk up from cwd to find the git repo root (directory containing .git).
/// Falls back to cwd if no .git found.
fn find_repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

fn resolve_settings_scope(user: bool, project: bool, local: bool) -> Result<SettingsScope> {
    match (user, project, local) {
        (true, false, false) => Ok(SettingsScope::User),
        (false, true, false) => Ok(SettingsScope::Project),
        (false, false, true) => Ok(SettingsScope::Local),
        (false, false, false) => Ok(SettingsScope::User), // default
        _ => Err(color_eyre::eyre::eyre!("specify at most one of --user, --project, --local")),
    }
}

fn claude_code_hook_entries() -> serde_json::Value {
    serde_json::json!({
        "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": "flotilla hook claude-code session-start"}]}],
        "SessionEnd": [{"matcher": "", "hooks": [{"type": "command", "command": "flotilla hook claude-code session-end"}]}],
        "UserPromptSubmit": [{"matcher": "", "hooks": [{"type": "command", "command": "flotilla hook claude-code user-prompt-submit"}]}],
        "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "flotilla hook claude-code stop"}]}],
        "Notification": [{"matcher": "permission_prompt", "hooks": [{"type": "command", "command": "flotilla hook claude-code notification"}]}]
    })
}

fn install_claude_code_hooks(path: &std::path::Path) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path).map_err(|e| color_eyre::eyre::eyre!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&content).map_err(|e| color_eyre::eyre::eyre!("failed to parse {}: {e}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let hooks = settings.as_object_mut().expect("settings is object").entry("hooks").or_insert_with(|| serde_json::json!({}));
    let new_entries = claude_code_hook_entries();
    for (event, matchers) in new_entries.as_object().expect("entries is object") {
        let event_hooks = hooks.as_object_mut().expect("hooks is object").entry(event).or_insert_with(|| serde_json::json!([]));
        let existing_arr = event_hooks.as_array().expect("event hooks is array");
        // Check if flotilla hooks are already present
        let already_installed = existing_arr.iter().any(|m| m.to_string().contains("flotilla hook claude-code"));
        if !already_installed {
            let arr = event_hooks.as_array_mut().expect("array");
            for entry in matchers.as_array().expect("matchers array") {
                arr.push(entry.clone());
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| color_eyre::eyre::eyre!("failed to create directory: {e}"))?;
    }
    let json = serde_json::to_string_pretty(&settings).expect("serialize");
    std::fs::write(path, json).map_err(|e| color_eyre::eyre::eyre!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

fn uninstall_claude_code_hooks(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(path).map_err(|e| color_eyre::eyre::eyre!("failed to read {}: {e}", path.display()))?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| color_eyre::eyre::eyre!("failed to parse {}: {e}", path.display()))?;

    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_event, matchers) in hooks.iter_mut() {
            if let Some(arr) = matchers.as_array_mut() {
                arr.retain(|m| !m.to_string().contains("flotilla hook claude-code"));
            }
        }
        // Remove empty event arrays
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    }

    let json = serde_json::to_string_pretty(&settings).expect("serialize");
    std::fs::write(path, json).map_err(|e| color_eyre::eyre::eyre!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use flotilla_protocol::{CheckoutSelector, Command, CommandAction, RepoSelector};

    use super::{parse_host_command, parse_repo_command, try_parse_cli_from, HostCommand, HostQueryCommand, RepoCommand, SubCommand};

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
    fn parse_host_refresh_accepts_bare_and_repo() {
        let parsed = parse_host_command(&["alpha".into(), "refresh".into()]).expect("bare refresh should parse");
        assert!(matches!(parsed, HostCommand::Control(cmd) if matches!(cmd.action, CommandAction::Refresh { repo: None })));

        let parsed = parse_host_command(&["alpha".into(), "refresh".into(), "my-repo".into()]).expect("refresh with repo should parse");
        assert!(
            matches!(parsed, HostCommand::Control(cmd) if matches!(cmd.action, CommandAction::Refresh { repo: Some(RepoSelector::Query(ref q)) } if q == "my-repo"))
        );
    }

    #[test]
    fn parse_host_refresh_rejects_extra_args() {
        let result = parse_host_command(&["alpha".into(), "refresh".into(), "my-repo".into(), "garbage".into()]);
        assert!(result.is_err(), "extra args after refresh repo should be rejected");
    }

    #[test]
    fn cli_parses_repo_query_with_trailing_json() {
        let cli = try_parse_cli_from(["flotilla", "repo", "owner/repo", "--json"]).expect("repo json should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Repo { args, json: true }) if args == vec!["owner/repo"]
        ));
    }

    #[test]
    fn parse_repo_prepare_terminal_command() {
        let parsed = parse_repo_command(&["owner/repo".into(), "prepare-terminal".into(), "/tmp/repo.feat-x".into()])
            .expect("prepare-terminal should parse");
        assert!(matches!(
            parsed,
            RepoCommand::Control(Command {
                host: None,
                context_repo: Some(RepoSelector::Query(ref repo)),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path, ref commands },
            }) if repo == "owner/repo" && checkout_path == Path::new("/tmp/repo.feat-x") && commands.is_empty()
        ));
    }

    #[test]
    fn parse_host_repo_prepare_terminal_preserves_context() {
        let parsed =
            parse_host_command(&["alpha".into(), "repo".into(), "owner/repo".into(), "prepare-terminal".into(), "/tmp/repo.feat-x".into()])
                .expect("host repo prepare-terminal should parse");
        assert!(matches!(
            parsed,
            HostCommand::Control(Command {
                host: Some(ref host),
                context_repo: Some(RepoSelector::Query(ref repo)),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path, ref commands },
            }) if host.as_str() == "alpha" && repo == "owner/repo" && checkout_path == Path::new("/tmp/repo.feat-x") && commands.is_empty()
        ));
    }
}
