use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use clap::Parser;
use color_eyre::Result;
use flotilla_core::{
    agents,
    config::ConfigStore,
    daemon::DaemonHandle,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    path_policy::{daemon_socket_path, PathPolicy},
    providers::{
        vcs::{git::GitVcs, Vcs},
        ProcessCommandRunner,
    },
};
use flotilla_protocol::{
    commands::CommandValue, output::OutputFormat, AgentHookEvent, AttachableId, Command, CommandAction, EnvironmentId, HostName,
    ProjectListResponse, RepoIdentity, RepoInfo, RepoSelector, ViewAddress,
};
use flotilla_tui::{app, event_log, theme};
use tracing::info;

/// Flotilla: TUI dashboard for managing development workspaces
#[derive(Parser)]
#[command(version, long_version = binary_version())]
struct Cli {
    /// Git repo roots (repeatable; auto-detected from cwd if omitted)
    #[arg(long)]
    repo_root: Vec<PathBuf>,

    /// Daemon identity root (config and state use ROOT/config and ROOT/state)
    #[arg(long, conflicts_with_all = ["config_dir", "state_dir"])]
    root: Option<PathBuf>,

    /// Config directory (commands that may start a daemon also require --state-dir)
    #[arg(long, conflicts_with = "root")]
    config_dir: Option<PathBuf>,

    /// State directory (commands that may start a daemon also require --config-dir)
    #[arg(long, conflicts_with = "root")]
    state_dir: Option<PathBuf>,

    /// Socket path (default: ${config_dir}/run/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Theme name (catppuccin-mocha, classic)
    #[arg(long)]
    theme: Option<String>,

    /// Output as JSON instead of human-readable text
    #[arg(long, global = true)]
    json: bool,

    /// Repo context for commands that need it (slug, path, or name)
    #[arg(long)]
    repo: Option<String>,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

fn binary_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        format!("{} (wire={}, proto={})", env!("CARGO_PKG_VERSION"), flotilla_client::BUILD_ID, flotilla_protocol::PROTOCOL_VERSION)
    })
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Run the daemon server
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonSubCommand>,
        /// Idle timeout in seconds (0 = no timeout)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Open the TUI scoped to one View (e.g. `flotilla view convoys/flotilla`).
    ///
    /// Scoped mode shows exactly that View: no tab bar, and the persisted
    /// open-view set is untouched. This is the deep-link entry Presentation
    /// Manager recipes embed (ADR 0013).
    View {
        /// View address: `overview`, `repo/<authority>/<path>`, `convoys/<namespace>`, ...
        /// A `flotilla://` prefix is accepted.
        address: String,
    },
    /// Print repo list and state
    Status,
    /// Stream daemon events to stdout
    Watch,
    /// Block until one condition leaf becomes true
    Wait {
        /// Condition leaf; repeat to wait for any leaf (OR)
        #[arg(long = "for", required = true)]
        leaves: Vec<flotilla_protocol::Leaf>,
        /// Resource namespace
        #[arg(long, default_value = "flotilla")]
        namespace: String,
        /// Require claim evidence observed at or after this RFC3339 instant
        ///
        /// Observation and claim leaves remain Unknown until their evidence
        /// is at least this fresh.
        #[arg(long)]
        fresher_than: Option<chrono::DateTime<chrono::Utc>>,
        /// Maximum seconds to block
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Show the daemon's current multi-host routing view
    Topology,
    /// Read structured daemon logs from this host or a peer
    Logs {
        /// Peer host name; omit to read this host
        #[arg(long)]
        host: Option<String>,
        /// Only show records this old or newer (for example `2h` or `30m`)
        #[arg(long, value_parser = parse_log_duration)]
        since: Option<Duration>,
        /// Minimum level: trace, debug, info, warn, or error
        #[arg(long, value_parser = ["trace", "debug", "info", "warn", "error"])]
        level: Option<String>,
        /// Exact tracing module target and its children
        #[arg(long)]
        target: Option<String>,
    },
    /// Show fleet-wide host health without collapsing independent observations
    Fleet,
    /// List convoy vessels and crew sessions
    Ls,
    /// Attach to a running convoy crew session
    Attach {
        /// Observe without taking the controller seat from another client
        #[arg(long, conflicts_with_all = ["strict", "take"])]
        watch: bool,
        /// Refuse when another attachment holds the controller seat
        #[arg(long, conflicts_with_all = ["watch", "take"])]
        strict: bool,
        /// Take control and demote the current controller to watcher
        #[arg(long, conflicts_with_all = ["watch", "strict"])]
        take: bool,
        /// Convoy, vessel, role, terminal session, or unique prefix
        reference: String,
        /// Internal attach mode used by temporary TUI excursions.
        #[arg(long, hide = true)]
        transient: bool,
        /// Restrict internal attach resolution to the daemon owning this row.
        #[arg(long, hide = true)]
        host: Option<String>,
    },
    /// Emit this host's store-backed fleet replica snapshot
    #[command(hide = true)]
    ReplicaSnapshot,
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
    /// Presentation-manager integration
    Pm {
        #[command(subcommand)]
        command: PmSubCommand,
    },
    /// Inspect raw daemon resources
    Resource {
        #[command(subcommand)]
        command: ResourceSubCommand,
    },
    // --- Domain nouns (generated by flotilla-commands) ---
    /// Manage repositories
    Repo(flotilla_commands::commands::repo::RepoNoun),
    /// Manage environments
    Environment(flotilla_commands::commands::environment::EnvironmentNoun),
    /// Manage checkouts
    Checkout(flotilla_commands::commands::checkout::CheckoutNoun),
    /// Manage convoys
    Convoy(flotilla_commands::commands::convoy::ConvoyNoun),
    /// Inspect proposed dispatch work
    Dispatch(flotilla_commands::commands::dispatch::DispatchNoun),
    /// Communicate with crew members
    Crew(flotilla_commands::commands::crew::CrewNoun),
    /// Code review (alias on CrNoun itself, not duplicated here)
    Cr(flotilla_commands::commands::cr::CrNoun),
    /// Issues
    Issue(flotilla_commands::commands::issue::IssueNoun),
    /// Cloud agents
    Agent(flotilla_commands::commands::agent::AgentNoun),
    /// Workspaces
    Workspace(flotilla_commands::commands::workspace::WorkspaceNoun),
    /// Manage and route to hosts
    Host(flotilla_commands::commands::host::HostNounPartial),
    /// Manage workflow templates
    WorkflowTemplate(flotilla_commands::commands::workflow_template::WorkflowTemplateNoun),
    /// Manage projects
    Project(flotilla_commands::commands::project::ProjectNoun),

    /// Generate completions (hidden, called by shell scripts)
    #[command(hide = true)]
    Complete {
        /// The input line to complete
        line: String,
        /// Cursor position within the line
        #[arg(default_value = "0")]
        cursor_pos: usize,
    },
    /// Output shell completion setup scripts
    Completions {
        /// Shell type
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

#[derive(clap::Subcommand)]
enum DaemonSubCommand {
    /// Gracefully stop the running daemon
    Stop,
    /// Toggle the fleet launchd agent for local daemon development
    DevMode {
        #[command(subcommand)]
        command: DevModeSubCommand,
    },
}

#[derive(clap::Subcommand)]
enum DevModeSubCommand {
    /// Disable and stop the fleet agent so a dev-built daemon may spawn
    Enable,
    /// Re-enable and start the fleet agent
    Disable,
}

#[derive(Clone, clap::ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(clap::Subcommand)]
enum PmSubCommand {
    /// Run the manifest metadata-patch connector for the enclosing PM
    Connect {
        /// Zellij binary for pipe delivery (default: $ZELLIJ_BIN, then `zellij`)
        #[arg(long)]
        zellij_bin: Option<String>,
        /// Restrict pipe delivery to one plugin instance
        #[arg(long)]
        plugin_url: Option<String>,
        /// Publish to a wheelhouse unix socket instead of zellij pipes
        #[arg(long)]
        wheelhouse_socket: Option<PathBuf>,
        /// Binary name minted into materialise recipes the PM executes
        #[arg(long, default_value = "flotilla")]
        flotilla_bin: String,
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
enum ResourceSubCommand {
    /// List resources of a kind
    List(ResourceListArgs),
    /// Create or update a raw resource document
    Apply(ResourceApplyArgs),
    /// Make the manifest overwrite a drifted live spec
    Sync(ResourceManifestResolutionArgs),
    /// Write the live spec back to its manifest
    Adopt(ResourceManifestResolutionArgs),
    /// Replace the status subresource after typed validation
    PatchStatus(ResourceStatusPatchArgs),
    /// Get one resource by name
    Get(ResourceGetArgs),
    /// Delete exactly one raw resource object, bypassing lifecycle gates
    Delete(ResourceDeleteArgs),
    /// Remove a declared transport remote from a Repository
    RemoveRemote(ResourceRemoveRemoteArgs),
    /// Remove standing multi-authored Host and PlacementPolicy records from non-home roots
    DedupSweep(ResourceDedupSweepArgs),
    /// Watch resources of a kind
    Watch(ResourceWatchArgs),
}

#[derive(clap::Args)]
struct ResourceDedupSweepArgs {
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
}

#[derive(clap::Args, bon::Builder)]
struct ResourceListArgs {
    /// Resource kind or plural name, e.g. convoys or WorkflowTemplate
    kind: String,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the query to a peer host
    #[arg(long)]
    host: Option<String>,
    /// Show only records authored by the queried host
    #[arg(long)]
    local_only: bool,
    /// Include read-only replicas from peer roots (the default)
    #[arg(long, conflicts_with = "local_only")]
    include_replicas: bool,
}

#[derive(clap::Args)]
struct ResourceGetArgs {
    /// Resource kind or plural name, e.g. convoys or WorkflowTemplate
    kind: String,
    /// Resource name
    name: String,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the query to a peer host
    #[arg(long)]
    host: Option<String>,
}

#[derive(clap::Args)]
struct ResourceManifestResolutionArgs {
    /// Exact resource kind or plural name
    kind: String,
    /// Exact resource name
    name: String,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the mutation to a peer host
    #[arg(long)]
    host: Option<String>,
}

#[derive(clap::Args, bon::Builder)]
struct ResourceWatchArgs {
    /// Resource kind or plural name, e.g. convoys or WorkflowTemplate
    kind: String,
    /// Restrict the stream to one resource name
    name: Option<String>,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the watch to a peer host
    #[arg(long)]
    host: Option<String>,
    /// Include read-only replicas from peer roots
    #[arg(long)]
    include_replicas: bool,
    /// Resume strictly after a cursor emitted by an earlier read or watch
    #[arg(long)]
    from_cursor: Option<flotilla_protocol::ResourceCursor>,
}

#[derive(clap::Args)]
struct ResourceDeleteArgs {
    /// Exact resource kind or plural name
    kind: String,
    /// Exact resource name
    name: String,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Collect a read-only replica from this origin root
    #[arg(long, value_name = "ORIGIN_ROOT")]
    replica: Option<String>,
    /// Delete the authoritative record on this peer host (including incorrectly authored residue)
    #[arg(long)]
    host: Option<String>,
}

#[derive(clap::Args)]
struct ResourceRemoveRemoteArgs {
    /// Repository resource name (its stable Repository key)
    name: String,
    /// Remote URL to remove
    remote: String,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the mutation to a peer host
    #[arg(long)]
    host: Option<String>,
}

#[derive(clap::Args)]
struct ResourceApplyArgs {
    /// Resource document path (JSON or YAML)
    #[arg(short, long)]
    file: PathBuf,
    /// Default namespace when metadata.namespace is omitted
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the mutation to a peer host
    #[arg(long)]
    host: Option<String>,
}

#[derive(clap::Args)]
struct ResourceStatusPatchArgs {
    /// Exact resource kind or plural name
    kind: String,
    /// Exact resource name
    name: String,
    /// Status document path (JSON or YAML)
    #[arg(short, long)]
    file: PathBuf,
    /// Resource namespace
    #[arg(long, default_value = "flotilla")]
    namespace: String,
    /// Route the mutation to a peer host
    #[arg(long)]
    host: Option<String>,
}

impl Cli {
    fn client_paths(&self) -> Result<CliPaths, String> {
        let policy = PathPolicy::from_process_env();
        let environment_socket = std::env::var_os("FLOTILLA_DAEMON_SOCKET");
        let (config_dir, state_dir) = client_dirs_from(
            self.root.as_deref(),
            self.config_dir.as_deref(),
            self.state_dir.as_deref(),
            policy.config_dir.as_path(),
            policy.state_dir.as_path(),
            environment_socket.as_deref(),
        )?;
        let socket_path = socket_path_from(self.socket.as_deref(), &config_dir, environment_socket.as_deref());
        Ok(CliPaths { config_dir, state_dir, socket_path })
    }

    fn daemon_paths(&self) -> Result<CliPaths, String> {
        let policy = PathPolicy::from_process_env();
        daemon_paths_from(
            self.root.as_deref(),
            self.config_dir.as_deref(),
            self.state_dir.as_deref(),
            self.socket.as_deref(),
            policy.config_dir.as_path(),
            policy.state_dir.as_path(),
        )
    }

    fn socket_path(&self) -> PathBuf {
        // Socket-only commands never spawn, so they may select a socket through
        // --config-dir without resolving the daemon's paired state directory.
        let policy = PathPolicy::from_process_env();
        let config_dir = self
            .root
            .as_deref()
            .map(|root| root.join("config"))
            .or_else(|| self.config_dir.clone())
            .unwrap_or_else(|| policy.config_dir.into_path_buf());
        socket_path_from(self.socket.as_deref(), &config_dir, std::env::var_os("FLOTILLA_DAEMON_SOCKET").as_deref())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CliPaths {
    config_dir: PathBuf,
    state_dir: PathBuf,
    socket_path: PathBuf,
}

fn client_dirs_from(
    explicit_root: Option<&Path>,
    explicit_config_dir: Option<&Path>,
    explicit_state_dir: Option<&Path>,
    default_config_dir: &Path,
    default_state_dir: &Path,
    environment_socket: Option<&std::ffi::OsStr>,
) -> Result<(PathBuf, PathBuf), String> {
    if explicit_root.is_none() && explicit_config_dir.is_none() && explicit_state_dir.is_none() {
        if let Some(dirs) = environment_socket.map(Path::new).and_then(flotilla_core::path_policy::scoped_daemon_dirs) {
            return Ok(dirs);
        }
    }
    daemon_dirs_from(explicit_root, explicit_config_dir, explicit_state_dir, default_config_dir, default_state_dir)
}

fn daemon_dirs_from(
    explicit_root: Option<&Path>,
    explicit_config_dir: Option<&Path>,
    explicit_state_dir: Option<&Path>,
    default_config_dir: &Path,
    default_state_dir: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    if let Some(root) = explicit_root {
        return Ok((root.join("config"), root.join("state")));
    }
    match (explicit_config_dir, explicit_state_dir) {
        (Some(config_dir), Some(state_dir)) => Ok((config_dir.to_path_buf(), state_dir.to_path_buf())),
        (None, None) => Ok((default_config_dir.to_path_buf(), default_state_dir.to_path_buf())),
        _ => Err("this command may start a daemon, so --config-dir and --state-dir must be supplied together; use --root to select both"
            .to_string()),
    }
}

fn socket_path_from(explicit: Option<&Path>, config_dir: &Path, environment: Option<&std::ffi::OsStr>) -> PathBuf {
    environment.map(PathBuf::from).or_else(|| explicit.map(PathBuf::from)).unwrap_or_else(|| daemon_socket_path(config_dir))
}

fn daemon_paths_from(
    explicit_root: Option<&Path>,
    explicit_config_dir: Option<&Path>,
    explicit_state_dir: Option<&Path>,
    explicit_socket: Option<&Path>,
    default_config_dir: &Path,
    default_state_dir: &Path,
) -> Result<CliPaths, String> {
    let (config_dir, state_dir) =
        daemon_dirs_from(explicit_root, explicit_config_dir, explicit_state_dir, default_config_dir, default_state_dir)?;
    let socket_path = explicit_socket.map(PathBuf::from).unwrap_or_else(|| daemon_socket_path(&config_dir));
    Ok(CliPaths { config_dir, state_dir, socket_path })
}

fn host_daemon_socket_required(contained_marker: Option<&std::ffi::OsStr>) -> bool {
    contained_marker.is_some()
}

async fn connect_cli_socket(
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    require_host_daemon: bool,
) -> Result<Arc<flotilla_tui::socket::SocketDaemon>, String> {
    let surface =
        cli_surface_from(std::env::var("FLOTILLA_CREW_ROLE").ok().as_deref(), std::env::var("FLOTILLA_NAMESPACE").ok().as_deref());
    if require_host_daemon {
        flotilla_tui::socket::connect_required_host_daemon_with_surface(socket_path, surface).await
    } else {
        flotilla_tui::socket::connect_or_spawn_with_surface(socket_path, config_dir, state_dir, surface).await
    }
}

fn cli_surface_from(crew_role: Option<&str>, namespace: Option<&str>) -> flotilla_protocol::SurfaceDeclaration {
    let namespace = namespace.unwrap_or("flotilla");
    let principal_ref = match crew_role.filter(|role| !role.trim().is_empty()) {
        Some(role) => flotilla_protocol::PrincipalRef { namespace: namespace.to_string(), name: format!("{role} agent") },
        None => flotilla_protocol::PrincipalRef::implicit_for_namespace(namespace),
    };
    flotilla_protocol::SurfaceDeclaration { principal_ref, character: flotilla_protocol::SurfaceCharacter::Focal }
}

#[tokio::main]
async fn main() -> Result<()> {
    flotilla_core::tls::install_default_provider();
    color_eyre::install()?;
    let mut cli = Cli::try_parse().unwrap_or_else(|error| exit_cli_parse_error(error));
    let format = OutputFormat::from_json_flag(cli.json);
    let command = cli.command.take();

    let result = match command {
        Some(SubCommand::View { address }) => {
            // Parse before touching the terminal so a bad address in a
            // recipe fails loudly at the shell (ADR 0013).
            let address: flotilla_protocol::ViewAddress = match address.parse() {
                Ok(address) => address,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(2);
                }
            };
            run_tui(cli, Some(address)).await
        }
        Some(SubCommand::Daemon { command: Some(DaemonSubCommand::Stop), .. }) => run_daemon_stop(&cli).await,
        Some(SubCommand::Daemon { command: Some(DaemonSubCommand::DevMode { command }), .. }) => run_daemon_dev_mode(&cli, command).await,
        Some(SubCommand::Daemon { command: None, timeout }) => run_daemon(&cli, timeout).await,
        Some(SubCommand::Status) => run_status(&cli, format).await,
        Some(SubCommand::Watch) => run_watch(&cli, format).await,
        Some(SubCommand::Wait { leaves, namespace, fresher_than, timeout }) => {
            run_wait(&cli, leaves, namespace, fresher_than, timeout, format).await
        }
        Some(SubCommand::Topology) => run_topology_command(&cli, format).await,
        Some(SubCommand::Logs { host, since, level, target }) => run_logs(&cli, host.as_deref(), since, level, target).await,
        Some(SubCommand::Fleet) => run_fleet_health(&cli, format).await,
        Some(SubCommand::Ls) => run_fleet_list(&cli, format).await,
        Some(SubCommand::Attach { reference, watch, strict, take, transient, host }) => {
            run_attach(&cli, &reference, attach_mode(watch, strict, take), transient, host.as_deref(), format).await
        }
        Some(SubCommand::ReplicaSnapshot) => run_replica_snapshot(&cli).await,
        Some(SubCommand::Hook { harness, event_type }) => run_hook(&cli, &harness, &event_type).await,
        Some(SubCommand::Hooks { command }) => run_hooks_command(&command).await,
        Some(SubCommand::Pm { command }) => run_pm_command(&cli, command).await,
        Some(SubCommand::Resource { command }) => run_resource_command(&cli, command, format).await,
        Some(SubCommand::Repo(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Environment(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Checkout(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Convoy(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Dispatch(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Crew(noun)) => {
            let crew_id = std::env::var("FLOTILLA_CREW_ID").ok();
            dispatch(noun.resolve_with_crew_id(crew_id).map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await
        }
        Some(SubCommand::Cr(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Issue(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Agent(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Workspace(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Host(partial)) => {
            use flotilla_commands::Refinable;
            dispatch(partial.refine().and_then(|n| n.resolve()).map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await
        }
        Some(SubCommand::WorkflowTemplate(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,
        Some(SubCommand::Project(noun)) => dispatch(noun.resolve().map_err(|e| color_eyre::eyre::eyre!(e))?, &cli, format).await,

        Some(SubCommand::Complete { line, cursor_pos }) => {
            run_complete(&line, cursor_pos);
            Ok(())
        }
        Some(SubCommand::Completions { shell }) => {
            run_completions(shell);
            Ok(())
        }

        None => run_tui(cli, None).await,
    };

    if let Err(error) = &result {
        let message = format!("{error:?}");
        let already_reexecuted = std::env::var(flotilla_tui::socket::reconnect::REEXEC_BUILD_ENV).ok();
        if should_reexec_for_incompatible_daemon(&message, already_reexecuted.as_deref()) {
            std::env::set_var(flotilla_tui::socket::reconnect::REEXEC_BUILD_ENV, flotilla_tui::socket::BUILD_ID);
            if let Err(reexec_error) = reexec_current_process() {
                return Err(color_eyre::eyre::eyre!(incompatible_daemon_reexec_failure(&message, &reexec_error)));
            }
        }
    }
    result
}

fn should_reexec_for_incompatible_daemon(error: &str, already_reexecuted_build: Option<&str>) -> bool {
    flotilla_tui::socket::reconnect::is_incompatible_daemon_error(error) && already_reexecuted_build != Some(flotilla_tui::socket::BUILD_ID)
}

fn incompatible_daemon_reexec_failure(incompatibility: &str, reexec_error: &dyn std::fmt::Display) -> String {
    format!("{incompatibility}; re-exec could not reach a matching build: {reexec_error}")
}

fn reexec_current_process() -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(error.into())
    }
    #[cfg(not(unix))]
    {
        command.spawn()?;
        std::process::exit(0);
    }
}

fn exit_cli_parse_error(error: clap::Error) -> ! {
    let hint = flotilla_commands::subject_parse_hint(&error);
    let exit_code = error.exit_code();
    if let Err(print_error) = error.print() {
        eprintln!("failed to print command-line error: {print_error}");
    }
    if let Some(hint) = hint {
        eprintln!("\n{hint}");
    }
    std::process::exit(exit_code)
}

fn select_startup_repo_roots(cli_roots: &[PathBuf], cwd_repo_root: Option<PathBuf>) -> Vec<PathBuf> {
    if cli_roots.is_empty() {
        cwd_repo_root.into_iter().collect()
    } else {
        let mut roots = Vec::with_capacity(cli_roots.len());
        for root in cli_roots {
            let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        roots
    }
}

async fn startup_repo_roots(cli_roots: &[PathBuf]) -> Vec<PathBuf> {
    let cwd_repo_root = if cli_roots.is_empty() {
        let cwd = std::env::current_dir().ok().map(ExecutionEnvironmentPath::new);
        let vcs = GitVcs::new(Arc::new(ProcessCommandRunner));
        match cwd {
            Some(cwd) => vcs.resolve_repo_root(&cwd).await.map(ExecutionEnvironmentPath::into_path_buf),
            None => None,
        }
    } else {
        None
    };
    select_startup_repo_roots(cli_roots, cwd_repo_root)
}

async fn show_startup_splash<F, Fut>(scoped_view: Option<&flotilla_protocol::ViewAddress>, show_splash: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if scoped_view.is_none() {
        show_splash().await?;
    }
    Ok(())
}

fn default_project_landing(
    repos: &[RepoInfo],
    startup_repo_roots: &[PathBuf],
    projects: &ProjectListResponse,
) -> Option<(RepoIdentity, ViewAddress)> {
    let repo = startup_repo_roots.iter().find_map(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        repos
            .iter()
            .find(|repo| repo.path.as_ref().is_some_and(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()) == root))
    })?;
    let repository_key = repo.repository_key.as_ref()?;
    let project = projects
        .projects
        .iter()
        .filter(|project| {
            matches!(project.repositories.as_slice(), [repository] if &repository.key == repository_key && repository.subpaths.is_empty())
        })
        .min_by_key(|project| (project.name != repo.name, project.namespace.as_str(), project.name.as_str()))?;
    Some((repo.identity.clone(), project.address.clone()))
}

/// Run the TUI. With `scoped_view`, run in scoped mode: exactly that View,
/// no tab shell, no open-view persistence.
async fn run_tui(cli: Cli, scoped_view: Option<flotilla_protocol::ViewAddress>) -> Result<()> {
    let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let resolved_state_dir = DaemonHostPath::new(paths.state_dir);
    event_log::init_with_dir(resolved_state_dir.as_path());
    let startup = std::time::Instant::now();
    let resolved_config_dir = paths.config_dir;
    let config = Arc::new(ConfigStore::new(DaemonHostPath::new(&resolved_config_dir), resolved_state_dir.clone()));

    // Initialize the terminal immediately. Full-app mode shows the splash for
    // fast visual feedback; scoped mode opens directly into its target view.
    let mut terminal = ratatui::init();
    flotilla_tui::terminal::install_panic_hook();
    #[cfg(unix)]
    flotilla_tui::terminal::install_sigterm_handler();

    let startup_repo_roots = startup_repo_roots(&cli.repo_root).await;
    let cli_theme = cli.theme.clone();
    let socket_path = paths.socket_path;
    let require_host_daemon =
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref());

    // Spawn daemon init on a separate task so full-app startup can run it
    // concurrently with the splash (which uses blocking event polling).
    let daemon_log_path =
        resolved_state_dir.as_path().join(flotilla_core::log_file::DAEMON_LOG_DIRECTORY).join(flotilla_core::log_file::DAEMON_LOG_FILE);
    let daemon_panic_log_path = resolved_config_dir.join("daemon-panic.log");
    let initial_socket_path = socket_path.clone();
    let initial_config_dir = resolved_config_dir.clone();
    let initial_state_dir = resolved_state_dir.clone();
    let daemon_task = tokio::spawn(async move {
        connect_cli_socket(&initial_socket_path, &initial_config_dir, initial_state_dir.as_path(), require_host_daemon)
            .await
            .map(|d| d as Arc<dyn DaemonHandle>)
    });

    show_startup_splash(scoped_view.as_ref(), || flotilla_tui::splash::show_splash(&mut terminal)).await?;
    let daemon = match daemon_task.await {
        Ok(Ok(daemon)) => {
            std::env::remove_var(flotilla_tui::socket::reconnect::REEXEC_BUILD_ENV);
            info!(elapsed = ?startup.elapsed(), "daemon ready");
            daemon
        }
        Ok(Err(e)) => {
            flotilla_tui::terminal::restore_terminal();
            eprintln!("  Check daemon log at {}", daemon_log_path.display());
            eprintln!("  Check panic log at {}", daemon_panic_log_path.display());
            return Err(color_eyre::eyre::eyre!(e));
        }
        Err(e) => {
            flotilla_tui::terminal::restore_terminal();
            return Err(color_eyre::eyre::eyre!("daemon initialization panicked: {e}"));
        }
    };

    for root in &startup_repo_roots {
        if let Err(e) = daemon
            .execute(flotilla_protocol::Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::TrackRepoPath { path: root.clone() },
            })
            .await
        {
            info!(repo = %root.display(), err = %e, "failed to add repo");
        }
    }

    let theme_name = cli_theme.or_else(|| config.load_config().ui.theme.clone()).unwrap_or_else(|| "catppuccin-mocha".to_string());
    let initial_theme = theme::theme_by_name(&theme_name);
    if !initial_theme.name.eq_ignore_ascii_case(&theme_name) {
        tracing::warn!(requested = %theme_name, using = %initial_theme.name, "unknown theme, falling back");
    }

    let pm_connector = flotilla_tui::pm_open::detect_connector();
    let repos_info = daemon.list_repos().await.unwrap_or_default();
    let default_landing = if scoped_view.is_none() && config.load_open_views().is_none() && !startup_repo_roots.is_empty() {
        match daemon
            .execute_query(
                Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryProjectList {} },
                uuid::Uuid::new_v4(),
            )
            .await
        {
            Ok(CommandValue::ProjectList(projects)) => default_project_landing(&repos_info, &startup_repo_roots, &projects),
            Ok(value) => {
                info!(?value, "default project query returned an unexpected result; using repo page");
                None
            }
            Err(error) => {
                info!(%error, "could not resolve default project landing; using repo page");
                None
            }
        }
    } else {
        None
    };
    let mut app = match scoped_view.clone() {
        Some(address) => app::App::new_scoped(daemon.clone(), repos_info, Arc::clone(&config), initial_theme.clone(), address),
        None => app::App::new_with_default_landing(daemon.clone(), repos_info, Arc::clone(&config), initial_theme.clone(), default_landing),
    }
    .with_pm_connector(pm_connector.clone());
    restore_tui_handoff(&mut app);

    loop {
        match flotilla_tui::run::run_event_loop(terminal, app).await? {
            flotilla_tui::run::EventLoopExit::Quit => return Ok(()),
            flotilla_tui::run::EventLoopExit::DaemonDisconnected(disconnected_app) => {
                info!("daemon disconnected; reconnecting TUI");
                app = *disconnected_app;
            }
        }

        terminal = ratatui::init();
        let connected = match flotilla_tui::socket::reconnect::connect_with_retry(
            || connect_cli_socket(&socket_path, &resolved_config_dir, resolved_state_dir.as_path(), require_host_daemon),
            |notice| {
                let (attempt, detail) = match notice {
                    flotilla_tui::socket::reconnect::ReconnectNotice::Attempt { attempt } => (attempt, None),
                    flotilla_tui::socket::reconnect::ReconnectNotice::Retry { attempt, error, delay } => {
                        (attempt, Some(format!("{error} — retrying in {:.1}s", delay.as_secs_f64())))
                    }
                };
                if let Err(error) = flotilla_tui::run::render_reconnect_frame(&mut terminal, attempt, detail.as_deref(), &initial_theme) {
                    tracing::warn!(%error, "failed to render daemon reconnect status");
                }
            },
        )
        .await
        {
            Ok(connected) => connected,
            Err(error) => {
                if let Err(handoff_error) = persist_tui_handoff(&app, resolved_state_dir.as_path()) {
                    tracing::warn!(error = %handoff_error, "failed to persist TUI state for re-exec");
                }
                flotilla_tui::terminal::restore_terminal();
                return Err(color_eyre::eyre::eyre!(error));
            }
        };
        info!("TUI reconnected to daemon");
        let repos_info = connected.list_repos().await.unwrap_or_default();
        app.reconnect_daemon(connected as Arc<dyn DaemonHandle>, repos_info);
    }
}

const TUI_HANDOFF_ENV: &str = "FLOTILLA_TUI_HANDOFF";

fn persist_tui_handoff(app: &app::App, state_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join(format!("tui-handoff-{}-{}.json", std::process::id(), uuid::Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(&app.handoff())?)?;
    std::env::set_var(TUI_HANDOFF_ENV, &path);
    Ok(())
}

fn restore_tui_handoff(app: &mut app::App) {
    let Some(path) = std::env::var_os(TUI_HANDOFF_ENV).map(PathBuf::from) else { return };
    std::env::remove_var(TUI_HANDOFF_ENV);
    let handoff = std::fs::read(&path)
        .map_err(|error| error.to_string())
        .and_then(|contents| serde_json::from_slice(&contents).map_err(|error| error.to_string()));
    let _ = std::fs::remove_file(&path);
    match handoff {
        Ok(handoff) => {
            app.restore_handoff(handoff);
            app.ui.command_echo = Some("Re-executed and reconnected to daemon".to_string());
        }
        Err(error) => tracing::warn!(%error, path = %path.display(), "could not restore TUI re-exec handoff"),
    }
}

async fn run_daemon(cli: &Cli, timeout_secs: u64) -> Result<()> {
    let daemon_binary = resolve_flotillad_binary()?;
    let CliPaths { config_dir, state_dir, socket_path } = cli.daemon_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    flotilla_core::path_policy::ensure_daemon_socket_belongs_to_config(&socket_path, &config_dir)
        .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let mut command = tokio::process::Command::new(&daemon_binary);
    command.arg("--timeout").arg(timeout_secs.to_string());
    command.arg("--config-dir").arg(config_dir);
    command.arg("--state-dir").arg(state_dir);
    command.arg("--socket").arg(socket_path);
    let status = command.status().await?;
    if status.success() {
        Ok(())
    } else {
        Err(color_eyre::eyre::eyre!(
            "flotillad exited with status {}",
            status.code().map(|code| code.to_string()).unwrap_or_else(|| "signal".to_string())
        ))
    }
}

async fn run_daemon_stop(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    if !socket_path.exists() {
        println!("Daemon is not running.");
        return Ok(());
    }
    flotilla_tui::socket::shutdown_existing(&socket_path)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("could not stop daemon at {}: {error}", socket_path.display()))?;
    wait_for_socket_removal(&socket_path).await?;
    println!("Daemon stopped.");
    Ok(())
}

async fn wait_for_socket_removal(socket_path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while socket_path.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| color_eyre::eyre::eyre!("daemon accepted shutdown but did not exit within 30s"))?;
    Ok(())
}

async fn run_daemon_dev_mode(cli: &Cli, command: DevModeSubCommand) -> Result<()> {
    match command {
        DevModeSubCommand::Enable => {
            // Disable first. Even if graceful shutdown fails, launchd cannot
            // resurrect the fleet daemon while we finish unloading the job.
            flotilla_tui::socket::launchd::set_agent_enabled(false).map_err(|error| color_eyre::eyre::eyre!(error))?;
            let socket_path = cli.socket_path();
            let shutdown_error =
                if socket_path.exists() { flotilla_tui::socket::shutdown_existing(&socket_path).await.err() } else { None };
            flotilla_tui::socket::launchd::bootout_agent().map_err(|error| color_eyre::eyre::eyre!(error))?;
            if socket_path.exists() {
                wait_for_socket_removal(&socket_path).await?;
            }
            if let Some(error) = shutdown_error {
                tracing::debug!(%error, "fleet daemon did not accept graceful shutdown before launchd bootout");
            }
            println!("Daemon dev mode enabled; the fleet launchd agent is disabled and stopped.");
            Ok(())
        }
        DevModeSubCommand::Disable => {
            flotilla_tui::socket::launchd::set_agent_enabled(true).map_err(|error| color_eyre::eyre::eyre!(error))?;
            flotilla_tui::socket::launchd::bootstrap_agent().map_err(|error| color_eyre::eyre::eyre!(error))?;
            flotilla_tui::socket::launchd::kickstart_agent().map_err(|error| color_eyre::eyre::eyre!(error))?;
            println!("Daemon dev mode disabled; the fleet launchd agent is enabled and started.");
            Ok(())
        }
    }
}

fn resolve_flotillad_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("FLOTILLAD_BIN") {
        return Ok(PathBuf::from(path));
    }

    let current = std::env::current_exe()?;
    let parent = current.parent().ok_or_else(|| color_eyre::eyre::eyre!("current executable has no parent directory"))?;
    let mut candidates = vec![parent.join("flotillad")];
    if parent.file_name().is_some_and(|name| name == "deps") {
        if let Some(grandparent) = parent.parent() {
            candidates.push(grandparent.join("flotillad"));
        }
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| color_eyre::eyre::eyre!("failed to locate flotillad next to {}", current.display()))
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

async fn run_pm_command(cli: &Cli, command: PmSubCommand) -> Result<()> {
    match command {
        PmSubCommand::Connect { zellij_bin, plugin_url, wheelhouse_socket, flotilla_bin } => {
            let options = flotilla_tui::pm_connect::PmConnectOptions::builder()
                .maybe_zellij_bin(zellij_bin)
                .maybe_plugin_url(plugin_url)
                .maybe_wheelhouse_socket(wheelhouse_socket)
                .flotilla_bin(flotilla_bin)
                .build();
            let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
            flotilla_tui::pm_connect::run(
                &socket_path,
                &config_dir,
                &state_dir,
                host_daemon_socket_required(
                    std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref(),
                ),
                options,
            )
            .await
            .map_err(|e| color_eyre::eyre::eyre!(e))
        }
    }
}

async fn run_watch(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    flotilla_tui::cli::run_watch(&cli.socket_path(), format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_wait(
    cli: &Cli,
    leaves: Vec<flotilla_protocol::Leaf>,
    namespace: String,
    freshness_demand: Option<chrono::DateTime<chrono::Utc>>,
    timeout_seconds: Option<u64>,
    format: OutputFormat,
) -> Result<()> {
    reset_sigpipe();
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let daemon = connect_cli_socket(
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref()),
    )
    .await
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let request = flotilla_protocol::WaitSubscriptionRequest { namespace, leaves, freshness_demand };
    let (subscription_id, mut events) = daemon.subscribe_wait(request).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
    let wait = async move {
        loop {
            match events.recv().await {
                Ok(fire) if fire.subscription_id == subscription_id => break Ok(fire),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    break Err(color_eyre::eyre::eyre!("wait event stream lagged by {skipped} event(s); condition may have fired"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break Err(color_eyre::eyre::eyre!("daemon restarted"));
                }
            }
        }
    };
    let fire = match timeout_seconds {
        Some(seconds) => tokio::time::timeout(Duration::from_secs(seconds), wait)
            .await
            .map_err(|_| color_eyre::eyre::eyre!("timed out waiting for condition after {seconds}s"))??,
        None => wait.await?,
    };
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(&fire)?),
        OutputFormat::Human => println!("condition fired: {} (value: {})", fire.leaf, fire.value),
    }
    Ok(())
}

async fn connect_daemon(cli: &Cli) -> Result<Arc<dyn DaemonHandle>> {
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let daemon = connect_cli_socket(
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref()),
    )
    .await
    .map_err(|e| color_eyre::eyre::eyre!(e))?;
    Ok(daemon as Arc<dyn DaemonHandle>)
}

async fn run_control_command(cli: &Cli, command: Command, format: OutputFormat) -> Result<()> {
    use std::io::IsTerminal;

    reset_sigpipe();
    let convoy_auto_attach = match &command.action {
        CommandAction::ConvoyStart { intent } => intent.auto_attach,
        _ => flotilla_protocol::ConvoyAutoAttach::Never,
    };
    let daemon = connect_daemon(cli).await?;
    let result = match flotilla_tui::cli::run_command(&*daemon, command, format).await {
        Ok(result) => result,
        Err(message) => exit_command_error(message, format),
    };
    if let CommandValue::Error { .. } = result {
        std::process::exit(1);
    }
    if let CommandValue::ConvoyStarted { name, attach_plan: Some(plan), binding } = result {
        if should_exec_convoy_attach(format, std::io::stdin().is_terminal(), convoy_auto_attach) {
            stamp_pane_identity(&name, binding.as_ref()).await;
            return run_attach_plan(&plan);
        }
    }
    Ok(())
}

fn should_exec_convoy_attach(format: OutputFormat, stdin_is_terminal: bool, auto_attach: flotilla_protocol::ConvoyAutoAttach) -> bool {
    matches!(format, OutputFormat::Human)
        && match auto_attach {
            flotilla_protocol::ConvoyAutoAttach::Default => stdin_is_terminal,
            flotilla_protocol::ConvoyAutoAttach::Always => true,
            flotilla_protocol::ConvoyAutoAttach::Never => false,
        }
}

fn exit_command_error(message: String, format: OutputFormat) -> ! {
    match format {
        OutputFormat::Human => eprintln!("error: {message}"),
        OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::Error { message })),
    }
    std::process::exit(1);
}

fn attach_mode(watch: bool, strict: bool, take: bool) -> flotilla_protocol::commands::AttachMode {
    match (watch, strict, take) {
        (true, false, false) => flotilla_protocol::commands::AttachMode::Default,
        (false, true, false) => flotilla_protocol::commands::AttachMode::Strict,
        (false, false, true) => flotilla_protocol::commands::AttachMode::Take,
        (false, false, false) => flotilla_protocol::commands::AttachMode::PreferTake,
        _ => unreachable!("clap rejects conflicting attach seat options"),
    }
}

async fn run_attach(
    cli: &Cli,
    reference: &str,
    mode: flotilla_protocol::commands::AttachMode,
    transient: bool,
    host: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    reset_sigpipe();
    let daemon = connect_daemon(cli).await?;
    let context_repo = match resolve_repo_from_env(cli) {
        Some(repo) => Some(repo),
        None => startup_repo_roots(&[]).await.into_iter().next().map(RepoSelector::Path),
    };
    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo,
                action: if transient {
                    CommandAction::AttachTransient {
                        reference: reference.to_string(),
                        host: host.map(flotilla_protocol::HostName::new),
                        mode,
                    }
                } else {
                    CommandAction::Attach { reference: reference.to_string(), host: host.map(flotilla_protocol::HostName::new), mode }
                },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    match result {
        CommandValue::AttachCommandResolved { plan, binding } => match format {
            OutputFormat::Json => {
                println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::AttachCommandResolved { plan, binding }));
                Ok(())
            }
            OutputFormat::Human => {
                if !transient {
                    stamp_pane_identity(reference, binding.as_ref()).await;
                }
                run_attach_plan(&plan)
            }
        },
        CommandValue::Error { message } => match format {
            OutputFormat::Json => {
                println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::Error { message: message.clone() }));
                Err(color_eyre::eyre::eyre!(message))
            }
            OutputFormat::Human => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        },
        other => Err(color_eyre::eyre::eyre!("unexpected attach response: {other:?}")),
    }
}

/// Publish pane ≙ identity into the enclosing PM's metadata plane before
/// launching the attach command — the one moment a process knows the binding
/// (flotilla-org/flotilla#708, half 1). Best-effort: a PM-less or failed
/// stamp never blocks the attach.
async fn stamp_pane_identity(reference: &str, binding: Option<&flotilla_protocol::AttachBinding>) {
    use flotilla_manifest::{pm::PmInstance, stamp::pane_stamp};
    let Some(pm) = PmInstance::detect(&|key| std::env::var(key).ok()) else {
        return;
    };
    let Some(pane) = pm.current_pane() else {
        return;
    };
    if let Err(error) = pm.sink().send(&pane_stamp(pane, reference, binding)).await {
        eprintln!("warning: could not stamp pane identity: {error}");
    }
}

async fn run_fleet_list(cli: &Cli, format: OutputFormat) -> Result<()> {
    run_control_command(
        cli,
        Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetList {} },
        format,
    )
    .await
}

async fn run_fleet_health(cli: &Cli, format: OutputFormat) -> Result<()> {
    run_control_command(
        cli,
        Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetHealth {} },
        format,
    )
    .await
}

async fn run_replica_snapshot(cli: &Cli) -> Result<()> {
    reset_sigpipe();
    let socket_path = cli.socket_path();
    let daemon = flotilla_tui::socket::SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon at {}: {e}", socket_path.display()))?;
    let result = daemon
        .execute_query(
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetReplicaSnapshot {} },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;
    match result {
        CommandValue::FleetReplicaSnapshot(snapshot) => {
            println!("{}", flotilla_protocol::output::json_pretty(&*snapshot));
            Ok(())
        }
        CommandValue::Error { message } => Err(color_eyre::eyre::eyre!(message)),
        other => Err(color_eyre::eyre::eyre!("unexpected replica snapshot response: {other:?}")),
    }
}

async fn run_resource_command(cli: &Cli, command: ResourceSubCommand, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    match command {
        ResourceSubCommand::List(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let daemon = connect_daemon(cli).await?;
            let response = flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
                .list(
                    flotilla_client::resource::ResourceListRequest::builder()
                        .kind(args.kind)
                        .namespace(args.namespace)
                        .maybe_node_id(node_id.clone())
                        .include_replicas(args.include_replicas || !args.local_only)
                        .build(),
                )
                .await
                .map_err(|e| color_eyre::eyre::eyre!(e))?;
            print_resource_read(daemon.as_ref(), node_id, response, format).await
        }
        ResourceSubCommand::Get(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let daemon = connect_daemon(cli).await?;
            let response = flotilla_client::resource::ResourceClient::new(Arc::clone(&daemon))
                .get(
                    flotilla_client::resource::ResourceGetRequest::builder()
                        .kind(args.kind)
                        .name(args.name)
                        .namespace(args.namespace)
                        .maybe_node_id(node_id.clone())
                        .build(),
                )
                .await
                .map_err(|e| color_eyre::eyre::eyre!(e))?;
            print_resource_read(daemon.as_ref(), node_id, response, format).await
        }
        ResourceSubCommand::Apply(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let raw = std::fs::read_to_string(&args.file)
                .map_err(|error| color_eyre::eyre::eyre!("read resource document {}: {error}", args.file.display()))?;
            let document: serde_json::Value = serde_yml::from_str(&raw)
                .map_err(|error| color_eyre::eyre::eyre!("parse resource document {}: {error}", args.file.display()))?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceApply { namespace: args.namespace, document },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::Sync(args) => run_manifest_resolution(cli, args, flotilla_protocol::ManifestResolution::Sync, format).await,
        ResourceSubCommand::Adopt(args) => run_manifest_resolution(cli, args, flotilla_protocol::ManifestResolution::Adopt, format).await,
        ResourceSubCommand::PatchStatus(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            let raw = std::fs::read_to_string(&args.file)
                .map_err(|error| color_eyre::eyre::eyre!("read status document {}: {error}", args.file.display()))?;
            let status: serde_json::Value = serde_yml::from_str(&raw)
                .map_err(|error| color_eyre::eyre::eyre!("parse status document {}: {error}", args.file.display()))?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceStatusPatch { namespace: args.namespace, kind: args.kind, name: args.name, status },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::Delete(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceDelete {
                        namespace: args.namespace,
                        kind: args.kind,
                        name: args.name,
                        replica_origin: args.replica.map(flotilla_protocol::NodeId::new),
                    },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::RemoveRemote(args) => {
            let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
            run_control_command(
                cli,
                Command {
                    node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::RepositoryRemoteRemove { namespace: args.namespace, name: args.name, remote: args.remote },
                },
                format,
            )
            .await
        }
        ResourceSubCommand::DedupSweep(args) => {
            let daemon = connect_daemon(cli).await?;
            let report = flotilla_client::resource::ResourceClient::new(daemon)
                .dedup_single_home_records(&args.namespace)
                .await
                .map_err(color_eyre::eyre::Report::msg)?;
            match format {
                OutputFormat::Json => println!(
                    "{}",
                    flotilla_protocol::output::json_pretty(&serde_json::json!({
                        "inspected_roots": report.inspected_roots,
                        "duplicate_records": report.duplicate_records,
                        "deletions": report.deletions.iter().map(|deletion| serde_json::json!({
                            "kind": deletion.kind,
                            "name": deletion.name,
                            "deleted_root": deletion.deleted_root,
                            "home_root": deletion.home_root,
                        })).collect::<Vec<_>>(),
                    }))
                ),
                OutputFormat::Human => {
                    println!(
                        "inspected {} roots; found {} duplicated records; deleted {} non-home copies",
                        report.inspected_roots,
                        report.duplicate_records,
                        report.deletions.len()
                    );
                    for deletion in report.deletions {
                        println!(
                            "deleted {}/{} from root {} (home: {})",
                            deletion.kind, deletion.name, deletion.deleted_root, deletion.home_root
                        );
                    }
                }
            }
            Ok(())
        }
        ResourceSubCommand::Watch(args) => run_resource_watch(cli, args, format).await,
    }
}

async fn run_manifest_resolution(
    cli: &Cli,
    args: ResourceManifestResolutionArgs,
    resolution: flotilla_protocol::ManifestResolution,
    format: OutputFormat,
) -> Result<()> {
    let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
    run_control_command(
        cli,
        Command {
            node_id,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ResourceManifestResolve { namespace: args.namespace, kind: args.kind, name: args.name, resolution },
        },
        format,
    )
    .await
}

async fn resolve_optional_host_node(cli: &Cli, host: Option<&str>) -> Result<Option<flotilla_protocol::NodeId>> {
    match host {
        Some(host) => {
            let (_environment_id, node_id) = resolve_host_target(cli, &HostName::new(host)).await?;
            Ok(Some(node_id))
        }
        None => Ok(None),
    }
}

async fn print_resource_read(
    daemon: &dyn DaemonHandle,
    node_id: Option<flotilla_protocol::NodeId>,
    response: flotilla_protocol::ResourceReadEnvelope,
    format: OutputFormat,
) -> Result<()> {
    let value = serde_json::to_value(response).map_err(|error| color_eyre::eyre::eyre!("encode resource read: {error}"))?;
    println!("{}", format_resource_value(daemon, node_id, value, format).await?);
    Ok(())
}

async fn format_resource_value(
    daemon: &dyn DaemonHandle,
    node_id: Option<flotilla_protocol::NodeId>,
    value: serde_json::Value,
    format: OutputFormat,
) -> Result<String> {
    if format == OutputFormat::Json {
        return Ok(flotilla_protocol::output::json_pretty(&value));
    }
    let hosts = daemon
        .execute_query(
            Command { node_id, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            uuid::Uuid::new_v4(),
        )
        .await;
    Ok(format_human_resource_value(value, hosts))
}

fn format_human_resource_value<E>(mut value: serde_json::Value, hosts: std::result::Result<CommandValue, E>) -> String {
    let Ok(CommandValue::HostList(hosts)) = hosts else {
        return flotilla_protocol::output::json_pretty(&value);
    };
    let names = hosts
        .hosts
        .iter()
        .filter_map(|host| {
            let host_id = host.environment_id.as_ref()?.host_id()?.to_string();
            let display_name = host.node.as_ref().map(|node| node.display_name.clone()).unwrap_or_else(|| host.host_name.to_string());
            Some((host_id, display_name))
        })
        .collect::<std::collections::HashMap<_, _>>();
    replace_host_ids(&mut value, &names);
    flotilla_protocol::output::json_pretty(&value)
}

fn replace_host_ids(value: &mut serde_json::Value, names: &std::collections::HashMap<String, String>) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(display_name) = names.get(text) {
                *text = display_name.clone();
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                replace_host_ids(value, names);
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values_mut() {
                replace_host_ids(value, names);
            }
        }
        _ => {}
    }
}

async fn run_resource_watch(cli: &Cli, args: ResourceWatchArgs, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let node_id = resolve_optional_host_node(cli, args.host.as_deref()).await?;
    let daemon = connect_daemon(cli).await?;
    let client = flotilla_client::resource::ResourceClient::new(daemon);
    let mut watch = client
        .watch(
            flotilla_client::resource::ResourceWatchRequest::builder()
                .kind(args.kind)
                .namespace(args.namespace)
                .maybe_name(args.name)
                .maybe_node_id(node_id)
                .include_replicas(args.include_replicas)
                .maybe_cursor(args.from_cursor)
                .build(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                watch.cancel().await.map_err(|e| color_eyre::eyre::eyre!(e))?;
                return Ok(());
            }
            event = watch.next() => {
                match event.map_err(|e| color_eyre::eyre::eyre!(e))? {
                    Some(response) => print_resource_watch_event(&response, format),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn print_resource_watch_event(response: &flotilla_protocol::ResourceReadEnvelope, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", flotilla_protocol::output::json_line(response)),
        OutputFormat::Human => {
            println!("{}", flotilla_protocol::output::json_pretty(response));
        }
    }
}

fn run_attach_plan(plan: &flotilla_protocol::ResolvedAttachPlan) -> Result<()> {
    flotilla_tui::terminal::exec_attach_plan(plan).map(|never| match never {}).map_err(color_eyre::eyre::Report::msg)
}

async fn resolve_host_target(cli: &Cli, subject: &HostName) -> Result<(EnvironmentId, flotilla_protocol::NodeId)> {
    let daemon = connect_daemon(cli).await?;
    let result = daemon
        .execute_query(
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let CommandValue::HostList(response) = result else {
        return Err(color_eyre::eyre::eyre!("unexpected response while resolving host"));
    };

    select_host_target(&response.hosts, subject)
}

fn select_host_target(
    hosts: &[flotilla_protocol::HostListEntry],
    subject: &HostName,
) -> Result<(EnvironmentId, flotilla_protocol::NodeId)> {
    let mut matches: Vec<_> = hosts.iter().filter(|entry| entry.host_name == *subject).collect();
    match matches.len() {
        0 => Err(color_eyre::eyre::eyre!("unknown host: {subject}")),
        1 => {
            let entry = matches.pop().expect("single host match");
            let environment_id = entry
                .environment_id
                .clone()
                .ok_or_else(|| color_eyre::eyre::eyre!("host {subject} is configured but has no known environment identity"))?;
            let node_id = entry
                .node
                .as_ref()
                .map(|node| node.node_id.clone())
                .ok_or_else(|| color_eyre::eyre::eyre!("host {subject} is configured but has no known node identity"))?;
            Ok((environment_id, node_id))
        }
        _ => {
            let mut ids: Vec<_> = matches
                .iter()
                .map(|entry| entry.environment_id.as_ref().map(EnvironmentId::canonical_string).unwrap_or_else(|| "configured".into()))
                .collect();
            ids.sort();
            Err(color_eyre::eyre::eyre!("ambiguous host: {subject} ({})", ids.join(", ")))
        }
    }
}

async fn resolve_environment_target(
    cli: &Cli,
    target_environment_id: &EnvironmentId,
) -> Result<(flotilla_protocol::ProvisioningTarget, flotilla_protocol::NodeId)> {
    let daemon = connect_daemon(cli).await?;
    let result = daemon
        .execute_query(
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let CommandValue::HostList(response) = result else {
        return Err(color_eyre::eyre::eyre!("unexpected response while resolving environment"));
    };

    for entry in response.hosts {
        let (Some(environment_id), Some(node)) = (entry.environment_id, entry.node) else {
            continue;
        };
        if environment_id == *target_environment_id {
            return Ok((provisioning_target_for_environment(&entry.host_name, &environment_id), node.node_id));
        }

        let status = daemon
            .execute_query(
                Command {
                    node_id: Some(node.node_id.clone()),
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryHostStatus { target_environment_id: environment_id.clone() },
                },
                uuid::Uuid::new_v4(),
            )
            .await
            .map_err(|e| color_eyre::eyre::eyre!(e));

        let status = match status {
            Ok(status) => status,
            Err(err) => {
                info!(
                    host = %entry.host_name,
                    %environment_id,
                    node_id = %node.node_id,
                    error = %err,
                    "skipping host while resolving environment target"
                );
                continue;
            }
        };

        let CommandValue::HostStatus(response) = status else {
            return Err(color_eyre::eyre::eyre!("unexpected host status response while resolving environment"));
        };

        if response.visible_environments.iter().any(|environment| environment.environment_id() == target_environment_id) {
            return Ok((
                flotilla_protocol::ProvisioningTarget::ExistingEnvironment { host: entry.host_name, env_id: target_environment_id.clone() },
                node.node_id,
            ));
        }
    }

    Err(color_eyre::eyre::eyre!("unknown environment: {target_environment_id}"))
}

fn provisioning_target_for_environment(host: &HostName, environment_id: &EnvironmentId) -> flotilla_protocol::ProvisioningTarget {
    if environment_id.is_host() {
        flotilla_protocol::ProvisioningTarget::Host { host: host.clone() }
    } else {
        flotilla_protocol::ProvisioningTarget::ExistingEnvironment { host: host.clone(), env_id: environment_id.clone() }
    }
}

fn resolve_repo_from_env(cli: &Cli) -> Option<RepoSelector> {
    match (&cli.repo, std::env::var("FLOTILLA_REPO").ok()) {
        (Some(repo), _) => Some(RepoSelector::Query(repo.clone())),
        (None, Some(repo)) if !repo.is_empty() => Some(RepoSelector::Query(repo)),
        _ => None,
    }
}

fn set_context_repo(cmd: &mut Command, cli: &Cli) {
    if cmd.context_repo.is_some() {
        return;
    }
    cmd.context_repo = resolve_repo_from_env(cli);
}

fn inject_repo_context(cmd: &mut Command, cli: &Cli) -> Result<()> {
    let repo_selector = resolve_repo_from_env(cli);

    match &mut cmd.action {
        CommandAction::Checkout { repo, .. } if *repo == RepoSelector::Query(String::new()) => {
            *repo = repo_selector.ok_or_else(|| color_eyre::eyre::eyre!("checkout create requires --repo or FLOTILLA_REPO"))?;
        }
        CommandAction::QueryIssues { repo, .. } if *repo == RepoSelector::Query(String::new()) => {
            *repo = repo_selector.clone().ok_or_else(|| color_eyre::eyre::eyre!("issue search requires --repo or FLOTILLA_REPO"))?;
            cmd.context_repo = repo_selector;
        }
        _ => {
            if cmd.context_repo.is_none() {
                cmd.context_repo = repo_selector;
            }
        }
    }
    Ok(())
}

fn confirm_command(
    command: &mut Command,
    interactive: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<bool, String> {
    let CommandAction::MergeChangeRequest { id, confirmed } = &mut command.action else {
        return Ok(true);
    };
    if *confirmed {
        return Ok(true);
    }
    if !interactive {
        return Err("merging a change request non-interactively requires --yes".to_string());
    }

    write!(output, "Merge change request {id} using squash? [y/N] ").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut response = String::new();
    input.read_line(&mut response).map_err(|error| error.to_string())?;
    if matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        *confirmed = true;
        Ok(true)
    } else {
        writeln!(output, "Merge cancelled.").map_err(|error| error.to_string())?;
        Ok(false)
    }
}

async fn run_confirmed_control_command(cli: &Cli, mut command: Command, format: OutputFormat) -> Result<()> {
    use std::io::IsTerminal;

    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut output = std::io::stderr().lock();
    if !confirm_command(&mut command, interactive, &mut input, &mut output).map_err(color_eyre::eyre::Report::msg)? {
        return Ok(());
    }
    run_control_command(cli, command, format).await
}

async fn dispatch(resolved: flotilla_commands::Resolved, cli: &Cli, format: OutputFormat) -> Result<()> {
    use flotilla_commands::{resolved::HostQueryKind, RepoContext, Resolved};
    reset_sigpipe();
    match resolved {
        Resolved::HostQuery { subject, kind } => {
            let (environment_id, node_id) = resolve_host_target(cli, &subject).await?;
            let action = match kind {
                HostQueryKind::Status => CommandAction::QueryHostStatus { target_environment_id: environment_id },
                HostQueryKind::Providers => CommandAction::QueryHostProviders { target_environment_id: environment_id },
            };
            run_confirmed_control_command(
                cli,
                Command { node_id: Some(node_id), provisioning_target: None, context_repo: None, action },
                format,
            )
            .await
        }
        Resolved::Ready(cmd) => run_confirmed_control_command(cli, cmd, format).await,
        Resolved::NeedsContext { mut command, repo, host } => {
            match repo {
                RepoContext::None => {}
                RepoContext::Required => inject_repo_context(&mut command, cli)?,
                RepoContext::Inferred => set_context_repo(&mut command, cli),
            }
            if command.node_id.is_none() {
                match host {
                    flotilla_commands::HostResolution::Explicit(subject) => {
                        let (_environment_id, node_id) = resolve_host_target(cli, &subject).await?;
                        command.node_id = Some(node_id);
                        command.provisioning_target = Some(flotilla_protocol::ProvisioningTarget::Host { host: subject });
                    }
                    flotilla_commands::HostResolution::ExplicitEnvironment(environment_id) => {
                        let (target, node_id) = resolve_environment_target(cli, &environment_id).await?;
                        command.node_id = Some(node_id);
                        command.provisioning_target = Some(target);
                    }
                    _ => {}
                }
            }
            run_confirmed_control_command(cli, command, format).await
        }
    }
}

async fn run_topology_command(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let daemon = connect_daemon(cli).await?;
    flotilla_tui::cli::run_topology(&*daemon, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

fn parse_log_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

async fn run_logs(cli: &Cli, host: Option<&str>, since: Option<Duration>, level: Option<String>, target: Option<String>) -> Result<()> {
    reset_sigpipe();
    let node_id = resolve_optional_host_node(cli, host).await?;
    let daemon = connect_daemon(cli).await?;
    let result = daemon
        .execute_query(
            Command {
                node_id,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryDaemonLogs {
                    query: flotilla_protocol::commands::DaemonLogQuery {
                        since_seconds: since.map(|duration| duration.as_secs()),
                        level,
                        target,
                    },
                },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|error| color_eyre::eyre::eyre!(error))?;
    match result {
        CommandValue::DaemonLogs { lines } => {
            for line in lines {
                println!("{line}");
            }
            Ok(())
        }
        CommandValue::Error { message } => Err(color_eyre::eyre::eyre!(message)),
        other => Err(color_eyre::eyre::eyre!("unexpected daemon logs response: {other:?}")),
    }
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
    let terminal = std::env::var("FLOTILLA_NAMESPACE")
        .ok()
        .filter(|value| !value.is_empty())
        .zip(std::env::var("FLOTILLA_TERMINAL_SESSION").ok().filter(|value| !value.is_empty()))
        .map(|(namespace, session_name)| flotilla_protocol::AgentHookTerminalRef { namespace, session_name });
    let event = AgentHookEvent::builder()
        .attachable_id(attachable_id)
        .harness(harness_enum)
        .event_type(parsed.event_type)
        .maybe_session_id(parsed.session_id)
        .maybe_model(parsed.model)
        .maybe_cwd(parsed.cwd)
        .maybe_terminal(terminal)
        .build();

    // 6. Send to daemon via socket. The daemon owns agent state as a single
    // actor — no file-level races between concurrent hook processes.
    send_hook_event(&cli.socket_path(), event).await
}

/// One-shot client: connect to daemon, send an AgentHook request, read one response, exit.
async fn send_hook_event(socket_path: &std::path::Path, event: AgentHookEvent) -> Result<()> {
    let daemon = flotilla_tui::socket::SocketDaemon::connect(socket_path)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("failed to connect to daemon at {}: {error}", socket_path.display()))?;
    daemon.send_agent_hook(event).await.map_err(|error| color_eyre::eyre::eyre!("daemon error: {error}"))
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

fn run_complete(line: &str, cursor_pos: usize) {
    use clap::CommandFactory;
    let mut root = Cli::command();
    root.build();
    let completions = flotilla_commands::complete::complete(&root, line, cursor_pos);
    for item in completions {
        if let Some(desc) = &item.description {
            println!("{}\t{desc}", item.value);
        } else {
            println!("{}", item.value);
        }
    }
}

fn run_completions(shell: CompletionShell) {
    match shell {
        CompletionShell::Bash => {
            print!(
                r#"_flotilla() {{
    local completions
    completions="$(flotilla complete "${{COMP_LINE}}" "${{COMP_POINT}}" 2>/dev/null)"
    COMPREPLY=()
    while IFS=$'\t' read -r val _desc; do
        [ -n "$val" ] && COMPREPLY+=("$val")
    done <<< "$completions"
}}
complete -F _flotilla flotilla
"#
            );
        }
        CompletionShell::Zsh => {
            // Pass the full command line and absolute cursor position.
            // words[*] is the full line split by words; CURSOR is the absolute byte offset.
            print!(
                r#"#compdef flotilla
_flotilla() {{
    local -a completions
    local line="${{words[*]}}"
    while IFS=$'\t' read -r val desc; do
        [ -n "$val" ] && completions+=("$val:$desc")
    done < <(flotilla complete "$line" "${{CURSOR}}" 2>/dev/null)
    _describe 'flotilla' completions
}}
compdef _flotilla flotilla
"#
            );
        }
        CompletionShell::Fish => {
            println!(
                r#"complete -c flotilla -f -a '(flotilla complete (commandline -cp) (commandline -C) 2>/dev/null | string replace \t \t)'"#
            );
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

fn install_claude_code_hooks(path: &std::path::Path) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path).map_err(|e| color_eyre::eyre::eyre!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&content).map_err(|e| color_eyre::eyre::eyre!("failed to parse {}: {e}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let hooks = settings.as_object_mut().expect("settings is object").entry("hooks").or_insert_with(|| serde_json::json!({}));
    let new_entries = agents::claude_code_hook_entries();
    for (event, matchers) in new_entries.as_object().expect("entries is object") {
        let event_hooks = hooks.as_object_mut().expect("hooks is object").entry(event).or_insert_with(|| serde_json::json!([]));
        let existing_arr = event_hooks.as_array().expect("event hooks is array");
        // Check if flotilla hooks are already present
        let already_installed = existing_arr.iter().any(|m| m.to_string().contains(agents::CLAUDE_CODE_HOOK_COMMAND_PREFIX));
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
                arr.retain(|m| !m.to_string().contains(agents::CLAUDE_CODE_HOOK_COMMAND_PREFIX));
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
    use std::path::{Path, PathBuf};

    use clap::Parser;
    use flotilla_protocol::{
        qualified_path::HostId, EnvironmentId, HostListEntry, HostName, NodeId, NodeInfo, PeerConnectionState, ProjectListEntry,
        ProjectListRepository, ProjectListResponse, ProvisioningTarget, RepoIdentity, RepoInfo, RepoLabels, RepositoryKey, ViewAddress,
    };

    use super::{
        attach_mode, cli_surface_from, client_dirs_from, confirm_command, daemon_paths_from, default_project_landing,
        format_human_resource_value, host_daemon_socket_required, incompatible_daemon_reexec_failure, provisioning_target_for_environment,
        replace_host_ids, run_replica_snapshot, select_host_target, select_startup_repo_roots, should_exec_convoy_attach,
        should_reexec_for_incompatible_daemon, show_startup_splash, socket_path_from, Cli, CliPaths, CommandValue, DaemonSubCommand,
        DevModeSubCommand, ResourceApplyArgs, ResourceDeleteArgs, ResourceGetArgs, ResourceListArgs, ResourceManifestResolutionArgs,
        ResourceStatusPatchArgs, ResourceSubCommand, ResourceWatchArgs, SubCommand,
    };

    #[test]
    fn crew_cli_surface_identifies_the_agent_role() {
        let surface = cli_surface_from(Some("governor"), Some("fleet"));

        assert_eq!(surface.principal_ref.namespace, "fleet");
        assert_eq!(surface.principal_ref.name, "governor agent");
    }

    #[test]
    fn human_cli_surface_uses_the_implicit_principal() {
        let surface = cli_surface_from(None, Some("fleet"));

        assert_eq!(surface.principal_ref, flotilla_protocol::PrincipalRef::implicit_for_namespace("fleet"));
    }

    #[test]
    fn default_convoy_start_does_not_auto_attach_non_interactively() {
        assert!(!should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Human,
            false,
            flotilla_protocol::ConvoyAutoAttach::Default,
        ));
        assert!(should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Human,
            true,
            flotilla_protocol::ConvoyAutoAttach::Default,
        ));
        assert!(should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Human,
            false,
            flotilla_protocol::ConvoyAutoAttach::Always,
        ));
        assert!(!should_exec_convoy_attach(
            flotilla_protocol::output::OutputFormat::Json,
            true,
            flotilla_protocol::ConvoyAutoAttach::Always,
        ));
    }

    fn landing_repo(path: &str, name: &str, key: Option<&str>) -> RepoInfo {
        RepoInfo {
            identity: RepoIdentity { authority: "github.com".into(), path: format!("org/{name}") },
            repository_key: key.map(|key| RepositoryKey(key.into())),
            path: Some(PathBuf::from(path)),
            name: name.into(),
            labels: RepoLabels::default(),
            provider_names: Default::default(),
            provider_health: Default::default(),
            loading: false,
        }
    }

    #[test]
    fn wait_cli_accepts_an_or_set_and_timeout() {
        let cli = Cli::try_parse_from([
            "flotilla",
            "wait",
            "--for",
            "convoy/demo .status.phase == Landed",
            "--for",
            "work/demo/implement .latest-claim.disposition == changes-pushed",
            "--timeout",
            "30",
        ])
        .expect("parse wait command");
        let Some(SubCommand::Wait { leaves, timeout, .. }) = cli.command else { panic!("expected wait command") };
        assert_eq!(leaves.len(), 2);
        assert_eq!(timeout, Some(30));
    }

    fn landing_project(name: &str, repositories: &[(&str, Option<&str>)]) -> ProjectListEntry {
        ProjectListEntry::builder()
            .namespace("flotilla".to_string())
            .name(name.to_string())
            .display_name(name.to_string())
            .address(ViewAddress::Project { namespace: "flotilla".into(), name: name.into() })
            .repositories(
                repositories
                    .iter()
                    .map(|(key, subpath)| ProjectListRepository {
                        key: RepositoryKey((*key).into()),
                        slug: None,
                        subpaths: subpath.iter().map(|subpath| (*subpath).to_string()).collect(),
                    })
                    .collect(),
            )
            .default_workflow_ref("single-agent-contained".to_string())
            .build()
    }

    #[test]
    fn incompatible_daemon_reexecs_once_per_client_build() {
        let mismatch = "daemon protocol version mismatch: daemon has 18, client has 17";
        assert!(should_reexec_for_incompatible_daemon(mismatch, None));
        assert!(!should_reexec_for_incompatible_daemon(mismatch, Some(flotilla_tui::socket::BUILD_ID)));
        assert!(!should_reexec_for_incompatible_daemon("daemon unavailable", None));
    }

    #[test]
    fn failed_incompatible_daemon_reexec_names_both_builds_and_protocols() {
        let mismatch = "daemon protocol version mismatch: client built cli-old speaks proto 19; daemon built daemon-new speaks proto 20";

        let message = incompatible_daemon_reexec_failure(mismatch, &std::io::Error::from_raw_os_error(libc::ENOENT));

        assert!(message.contains("client built cli-old speaks proto 19"), "{message}");
        assert!(message.contains("daemon built daemon-new speaks proto 20"), "{message}");
        assert!(message.contains("re-exec could not reach a matching build"), "{message}");
    }

    #[test]
    fn explicit_repo_roots_take_precedence_over_cwd_detection() {
        let explicit = vec![PathBuf::from("/repos/one"), PathBuf::from("/repos/two")];

        let roots = select_startup_repo_roots(&explicit, Some(PathBuf::from("/repos/current")));

        assert_eq!(roots, explicit);
    }

    #[test]
    fn explicit_repo_roots_are_deduplicated_in_argument_order() {
        let explicit = vec![PathBuf::from("/repos/one"), PathBuf::from("/repos/two"), PathBuf::from("/repos/one")];

        let roots = select_startup_repo_roots(&explicit, None);

        assert_eq!(roots, vec![PathBuf::from("/repos/one"), PathBuf::from("/repos/two")]);
    }

    #[test]
    fn fresh_landing_resolves_the_detected_repos_whole_repo_project() {
        let repos = vec![
            landing_repo("/repos/other", "other", Some("repo-other")),
            landing_repo("/repos/flotilla", "flotilla", Some("repo-flotilla")),
        ];
        let projects = ProjectListResponse {
            projects: vec![
                landing_project("presentation", &[("repo-flotilla", None), ("repo-other", None)]),
                landing_project("flotilla", &[("repo-flotilla", None)]),
            ],
        };

        let landing = default_project_landing(&repos, &[PathBuf::from("/repos/flotilla")], &projects);

        assert_eq!(
            landing,
            Some((repos[1].identity.clone(), ViewAddress::Project { namespace: "flotilla".into(), name: "flotilla".into() },))
        );
    }

    #[test]
    fn fresh_landing_falls_back_when_detected_repo_has_no_project() {
        let repos = vec![landing_repo("/repos/plain", "plain", Some("repo-plain"))];
        let projects = ProjectListResponse { projects: vec![landing_project("other", &[("repo-other", None)])] };

        assert_eq!(default_project_landing(&repos, &[PathBuf::from("/repos/plain")], &projects), None);
    }

    #[test]
    fn fresh_landing_does_not_confuse_a_subpath_project_for_the_whole_repo_project() {
        let repos = vec![landing_repo("/repos/shared", "shared", Some("repo-shared"))];
        let projects = ProjectListResponse {
            projects: vec![
                landing_project("docs", &[("repo-shared", Some("docs"))]),
                landing_project("shared-a1b2c3d4", &[("repo-shared", None)]),
            ],
        };

        assert_eq!(
            default_project_landing(&repos, &[PathBuf::from("/repos/shared")], &projects),
            Some((repos[0].identity.clone(), ViewAddress::Project { namespace: "flotilla".into(), name: "shared-a1b2c3d4".into() },))
        );
    }

    #[test]
    fn cwd_repo_is_used_when_no_explicit_root_is_given() {
        let roots = select_startup_repo_roots(&[], Some(PathBuf::from("/repos/current")));

        assert_eq!(roots, vec![PathBuf::from("/repos/current")]);
    }

    #[tokio::test]
    async fn scoped_tui_skips_splash_while_full_tui_shows_it() {
        use std::cell::Cell;

        let scoped_view = "convoys/flotilla".parse().expect("valid scoped view");
        let scoped_splash_shown = Cell::new(false);
        show_startup_splash(Some(&scoped_view), || async {
            scoped_splash_shown.set(true);
            Ok(())
        })
        .await
        .expect("scoped startup");

        let full_splash_shown = Cell::new(false);
        show_startup_splash(None, || async {
            full_splash_shown.set(true);
            Ok(())
        })
        .await
        .expect("full startup");

        assert!(!scoped_splash_shown.get());
        assert!(full_splash_shown.get());
    }

    #[test]
    fn cli_parses_topology_subcommand() {
        let cli = Cli::try_parse_from(["flotilla", "topology"]).expect("topology cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Topology)));
        assert!(!cli.json);
    }

    #[test]
    fn cli_parses_topology_with_global_json() {
        let cli = Cli::try_parse_from(["flotilla", "topology", "--json"]).expect("topology json should parse");
        assert!(matches!(cli.command, Some(SubCommand::Topology)));
        assert!(cli.json);
    }

    #[test]
    fn cli_parses_remote_filtered_logs() {
        let cli = Cli::try_parse_from([
            "flotilla",
            "logs",
            "--host",
            "feta",
            "--since",
            "2h",
            "--level",
            "warn",
            "--target",
            "flotilla_daemon::peer",
        ])
        .expect("logs cli should parse");

        assert!(matches!(
            cli.command,
            Some(SubCommand::Logs {
                host: Some(host),
                since: Some(since),
                level: Some(level),
                target: Some(target),
            }) if host == "feta"
                && since == std::time::Duration::from_secs(7200)
                && level == "warn"
                && target == "flotilla_daemon::peer"
        ));
    }

    #[test]
    fn cli_parses_ls_subcommand() {
        let cli = Cli::try_parse_from(["flotilla", "ls"]).expect("ls cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Ls)));
    }

    #[test]
    fn cli_parses_resource_subcommands() {
        let list = Cli::try_parse_from(["flotilla", "resource", "list", "convoys", "--host", "feta"]).expect("resource list should parse");
        assert!(matches!(
            list.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::List(ResourceListArgs {
                    kind,
                    namespace,
                    host: Some(host),
                    local_only: false,
                    include_replicas: false,
                })
            }) if kind == "convoys" && namespace == "flotilla" && host == "feta"
        ));

        let include_replicas = Cli::try_parse_from(["flotilla", "resource", "list", "hosts", "--include-replicas"])
            .expect("legacy explicit replica selection should still parse");
        assert!(matches!(
            include_replicas.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::List(ResourceListArgs {
                    kind,
                    namespace,
                    host: None,
                    local_only: false,
                    include_replicas: true,
                })
            }) if kind == "hosts" && namespace == "flotilla"
        ));

        let get = Cli::try_parse_from(["flotilla", "resource", "get", "convoys", "demo", "--namespace", "ops"])
            .expect("resource get should parse");
        assert!(matches!(
            get.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Get(ResourceGetArgs { kind, name, namespace, host: None })
            }) if kind == "convoys" && name == "demo" && namespace == "ops"
        ));

        let sync = Cli::try_parse_from(["flotilla", "resource", "sync", "projects", "cleat", "--namespace", "ops"])
            .expect("manifest sync should parse");
        assert!(matches!(
            sync.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Sync(ResourceManifestResolutionArgs { kind, name, namespace, host: None })
            }) if kind == "projects" && name == "cleat" && namespace == "ops"
        ));
        let adopt = Cli::try_parse_from(["flotilla", "resource", "adopt", "projects", "cleat", "--namespace", "ops"])
            .expect("manifest adopt should parse");
        assert!(matches!(
            adopt.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Adopt(ResourceManifestResolutionArgs { kind, name, namespace, host: None })
            }) if kind == "projects" && name == "cleat" && namespace == "ops"
        ));

        let delete =
            Cli::try_parse_from(["flotilla", "resource", "delete", "workflowtemplates", "scratch", "--namespace", "ops", "--host", "feta"])
                .expect("resource delete should parse");
        assert!(matches!(
            delete.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Delete(ResourceDeleteArgs { kind, name, namespace, replica: None, host: Some(host) })
            }) if kind == "workflowtemplates" && name == "scratch" && namespace == "ops" && host == "feta"
        ));

        let collect =
            Cli::try_parse_from(["flotilla", "resource", "delete", "convoys", "orphan", "--replica", "retired-root", "--host", "feta"])
                .expect("replica collection should parse");
        assert!(matches!(
            collect.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Delete(ResourceDeleteArgs {
                    replica: Some(origin),
                    host: Some(host),
                    ..
                })
            }) if origin == "retired-root" && host == "feta"
        ));

        let apply = Cli::try_parse_from(["flotilla", "resource", "apply", "-f", "demand.yaml", "--namespace", "ops", "--host", "feta"])
            .expect("resource apply should parse");
        assert!(matches!(
            apply.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Apply(ResourceApplyArgs { file, namespace, host: Some(host) })
            }) if file.as_path() == Path::new("demand.yaml") && namespace == "ops" && host == "feta"
        ));

        let patch_status =
            Cli::try_parse_from(["flotilla", "resource", "patch-status", "usages", "usage-ada", "-f", "status.json", "--namespace", "ops"])
                .expect("resource patch-status should parse");
        assert!(matches!(
            patch_status.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::PatchStatus(ResourceStatusPatchArgs { kind, name, file, namespace, host: None })
            }) if kind == "usages"
                && name == "usage-ada"
                && file.as_path() == Path::new("status.json")
                && namespace == "ops"
        ));

        let watch = Cli::try_parse_from([
            "flotilla",
            "resource",
            "watch",
            "terminalsessions",
            "session-a",
            "--from-cursor",
            &flotilla_protocol::ResourceCursor::from_position("7", None).to_string(),
            "--json",
        ])
        .expect("resource watch should parse");
        assert!(watch.json);
        assert!(matches!(
            watch.command,
            Some(SubCommand::Resource {
                command: ResourceSubCommand::Watch(ResourceWatchArgs {
                    kind,
                    name: Some(name),
                    namespace,
                    host: None,
                    include_replicas: false,
                    from_cursor: Some(_),
                })
            }) if kind == "terminalsessions" && name == "session-a" && namespace == "flotilla"
        ));
    }

    #[test]
    fn human_resource_rendering_replaces_exact_host_ids_but_not_embedded_object_names() {
        let mut value = serde_json::json!({
            "spec": {"host_ref": "01HXYZ"},
            "metadata": {"name": "host-direct-01HXYZ"},
            "status": {"placement_decision": {"target_host": {"ref": "01HXYZ", "display_name": "kiwi"}}}
        });
        replace_host_ids(&mut value, &std::collections::HashMap::from([("01HXYZ".to_string(), "kiwi".to_string())]));

        assert_eq!(value["spec"]["host_ref"], "kiwi");
        assert_eq!(value["status"]["placement_decision"]["target_host"]["ref"], "kiwi");
        assert_eq!(value["metadata"]["name"], "host-direct-01HXYZ");
    }

    #[test]
    fn human_resource_rendering_falls_back_to_raw_json_when_host_lookup_fails() {
        let value = serde_json::json!({
            "spec": {"host_ref": "01HXYZ"},
            "metadata": {"name": "demo"}
        });

        let rendered = format_human_resource_value(value.clone(), Err::<CommandValue, _>("transient host lookup failure"));

        assert_eq!(rendered, flotilla_protocol::output::json_pretty(&value));
    }

    #[tokio::test]
    async fn replica_snapshot_does_not_spawn_daemon_when_socket_is_missing() {
        let test_dir = std::env::temp_dir().join(format!(
            "flotilla-replica-snapshot-no-spawn-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock after epoch").as_nanos()
        ));
        std::fs::create_dir_all(&test_dir).expect("create test dir");
        let config_dir = test_dir.join("config");
        let socket = test_dir.join("missing.sock");
        let cli = Cli::try_parse_from([
            "flotilla",
            "--config-dir",
            config_dir.to_str().expect("config dir utf8"),
            "--socket",
            socket.to_str().expect("socket utf8"),
            "replica-snapshot",
        ])
        .expect("replica snapshot cli should parse");

        // FLOTILLA_DAEMON_SOCKET takes precedence over --socket (contained crew
        // delivery relies on that), so a live ambient value would otherwise route
        // this test at the real host daemon instead of the intentionally-missing
        // socket under test. Isolate it for the duration of the assertion.
        let ambient_socket = std::env::var_os("FLOTILLA_DAEMON_SOCKET");
        std::env::remove_var("FLOTILLA_DAEMON_SOCKET");

        let err = run_replica_snapshot(&cli).await.expect_err("missing socket should fail");

        if let Some(value) = ambient_socket {
            std::env::set_var("FLOTILLA_DAEMON_SOCKET", value);
        }

        assert!(err.to_string().contains("cannot connect to daemon"), "unexpected error: {err}");
        std::fs::remove_dir_all(&test_dir).expect("remove test dir");
    }

    #[test]
    fn cli_parses_attach_subcommand() {
        let cli = Cli::try_parse_from(["flotilla", "attach", "convoy-a/implement/coder"]).expect("attach cli should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Attach { reference, watch: false, strict: false, take: false, transient: false, host: None })
                if reference == "convoy-a/implement/coder"
        ));
        assert_eq!(attach_mode(false, false, false), flotilla_protocol::commands::AttachMode::PreferTake);
    }

    #[test]
    fn cli_parses_controller_seat_flags() {
        let strict =
            Cli::try_parse_from(["flotilla", "attach", "--strict", "convoy-a/implement/coder"]).expect("strict attach cli should parse");
        assert!(matches!(
            strict.command,
            Some(SubCommand::Attach { reference, watch: false, strict: true, take: false, transient: false, host: None })
                if reference == "convoy-a/implement/coder"
        ));
        let take = Cli::try_parse_from(["flotilla", "attach", "--take", "convoy-a/implement/coder"]).expect("take attach cli should parse");
        assert!(matches!(
            take.command,
            Some(SubCommand::Attach { reference, watch: false, strict: false, take: true, transient: false, host: None })
                if reference == "convoy-a/implement/coder"
        ));
        let watch =
            Cli::try_parse_from(["flotilla", "attach", "--watch", "convoy-a/implement/coder"]).expect("watch attach cli should parse");
        assert!(matches!(
            watch.command,
            Some(SubCommand::Attach { reference, watch: true, strict: false, take: false, transient: false, host: None })
                if reference == "convoy-a/implement/coder"
        ));
        assert_eq!(attach_mode(true, false, false), flotilla_protocol::commands::AttachMode::Default);
        assert!(Cli::try_parse_from(["flotilla", "attach", "--strict", "--take", "convoy-a/implement/coder"]).is_err());
        assert!(Cli::try_parse_from(["flotilla", "attach", "--watch", "--take", "convoy-a/implement/coder"]).is_err());
    }

    #[test]
    fn contained_cli_socket_environment_takes_precedence() {
        assert_eq!(
            socket_path_from(
                Some(Path::new("/explicit/flotilla.sock")),
                Path::new("/config"),
                Some(std::ffi::OsStr::new("/run/flotilla.sock")),
            ),
            PathBuf::from("/run/flotilla.sock"),
        );
    }

    #[test]
    fn default_socket_uses_a_dedicated_runtime_directory() {
        assert_eq!(socket_path_from(None, Path::new("/config"), None), PathBuf::from("/config/run/flotilla.sock"));
    }

    #[test]
    fn daemon_paths_use_only_explicit_values_and_policy_defaults() {
        assert_eq!(
            daemon_paths_from(
                None,
                None,
                None,
                Some(Path::new("/explicit/flotilla.sock")),
                Path::new("/default/config"),
                Path::new("/default/state"),
            )
            .expect("policy defaults form a complete identity"),
            CliPaths {
                config_dir: PathBuf::from("/default/config"),
                state_dir: PathBuf::from("/default/state"),
                socket_path: PathBuf::from("/explicit/flotilla.sock"),
            },
        );
    }

    #[test]
    fn ambient_scoped_socket_supplies_client_config_and_state_dirs() {
        assert_eq!(
            client_dirs_from(
                None,
                None,
                None,
                Path::new("/home/test/.config/flotilla"),
                Path::new("/home/test/.local/state/flotilla"),
                Some(std::ffi::OsStr::new("/work/live-session/config/run/flotilla.sock")),
            )
            .expect("ambient scoped socket forms a complete identity"),
            (PathBuf::from("/work/live-session/config"), PathBuf::from("/work/live-session/state")),
        );
    }

    #[test]
    fn explicit_config_is_not_replaced_by_a_conflicting_scoped_socket() {
        assert_eq!(
            client_dirs_from(
                None,
                Some(Path::new("/work/root-b/config")),
                Some(Path::new("/work/root-b/state")),
                Path::new("/home/test/.config/flotilla"),
                Path::new("/home/test/.local/state/flotilla"),
                Some(std::ffi::OsStr::new("/work/root-a/config/run/flotilla.sock")),
            )
            .expect("paired category overrides form a complete identity"),
            (PathBuf::from("/work/root-b/config"), PathBuf::from("/work/root-b/state")),
        );
    }

    #[test]
    fn daemon_starting_paths_require_config_and_state_overrides_together() {
        let config_only =
            Cli::try_parse_from(["flotilla", "--config-dir", "/work/a/config"]).expect("socket-only commands may parse a config override");
        let state_only =
            Cli::try_parse_from(["flotilla", "--state-dir", "/work/a/state"]).expect("socket-only commands may parse a state override");
        let config_error = config_only.client_paths().expect_err("config-only startup identity must be rejected");
        let state_error = state_only.client_paths().expect_err("state-only startup identity must be rejected");
        assert!(config_error.contains("may start a daemon"), "unexpected error: {config_error}");
        assert!(state_error.contains("may start a daemon"), "unexpected error: {state_error}");
        assert!(Cli::try_parse_from(["flotilla", "--config-dir", "/work/a/config", "--state-dir", "/work/a/state", "status",])
            .expect("paired overrides should parse")
            .client_paths()
            .is_ok());
    }

    #[test]
    fn daemon_stop_is_a_nested_daemon_command() {
        let cli = Cli::try_parse_from(["flotilla", "daemon", "stop"]).expect("daemon stop should parse");
        assert!(matches!(cli.command, Some(SubCommand::Daemon { command: Some(DaemonSubCommand::Stop), .. })));

        let foreground = Cli::try_parse_from(["flotilla", "daemon", "--timeout", "0"]).expect("foreground daemon should still parse");
        assert!(matches!(foreground.command, Some(SubCommand::Daemon { command: None, timeout: 0 })));

        let dev_mode = Cli::try_parse_from(["flotilla", "daemon", "dev-mode", "enable"]).expect("daemon dev-mode enable should parse");
        assert!(matches!(
            dev_mode.command,
            Some(SubCommand::Daemon { command: Some(DaemonSubCommand::DevMode { command: DevModeSubCommand::Enable }), .. })
        ));

        let fleet_mode = Cli::try_parse_from(["flotilla", "daemon", "dev-mode", "disable"]).expect("daemon dev-mode disable should parse");
        assert!(matches!(
            fleet_mode.command,
            Some(SubCommand::Daemon { command: Some(DaemonSubCommand::DevMode { command: DevModeSubCommand::Disable }), .. })
        ));
    }

    #[test]
    fn custom_roots_select_distinct_daemon_stores_and_lifecycle_locks() {
        let paths_a =
            daemon_paths_from(Some(Path::new("/work/a")), None, None, None, Path::new("/default/config"), Path::new("/default/state"))
                .expect("root A forms a complete identity");
        let paths_b =
            daemon_paths_from(Some(Path::new("/work/b")), None, None, None, Path::new("/default/config"), Path::new("/default/state"))
                .expect("root B forms a complete identity");

        assert_eq!(paths_a.config_dir, Path::new("/work/a/config"));
        assert_eq!(paths_a.state_dir, Path::new("/work/a/state"));
        assert_eq!(paths_a.socket_path, Path::new("/work/a/config/run/flotilla.sock"));
        assert_ne!(paths_a.state_dir.join("resources.sqlite"), paths_b.state_dir.join("resources.sqlite"));
        assert_ne!(
            paths_a.state_dir.join(flotilla_core::DAEMON_LIFECYCLE_LOCK_FILE),
            paths_b.state_dir.join(flotilla_core::DAEMON_LIFECYCLE_LOCK_FILE),
        );
    }

    #[test]
    fn root_selector_conflicts_with_category_overrides() {
        assert!(Cli::try_parse_from([
            "flotilla",
            "--root",
            "/work/a",
            "--config-dir",
            "/work/b/config",
            "--state-dir",
            "/work/b/state",
            "status",
        ])
        .is_err());
    }

    #[test]
    fn contained_cli_socket_environment_requires_the_host_daemon() {
        assert!(host_daemon_socket_required(Some(std::ffi::OsStr::new("1"))));
        assert!(!host_daemon_socket_required(None));
    }

    #[test]
    fn cli_parses_internal_transient_attach_target() {
        let cli = Cli::try_parse_from(["flotilla", "attach", "--transient", "--host", "feta", "terminal-scratch"])
            .expect("transient attach cli should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Attach { reference, watch: false, strict: false, take: false, transient: true, host: Some(host) })
                if reference == "terminal-scratch" && host == "feta"
        ));
    }

    #[test]
    fn cli_parses_host_qualified_materialize_recipe() {
        let cli = Cli::try_parse_from(["flotilla", "attach", "--host", "feta", "terminal-scratch"])
            .expect("host-qualified attach cli should parse");
        assert!(matches!(
            cli.command,
            Some(SubCommand::Attach { reference, watch: false, strict: false, take: false, transient: false, host: Some(host) })
                if reference == "terminal-scratch" && host == "feta"
        ));
    }

    #[test]
    fn cli_parses_repo_noun() {
        let cli = Cli::try_parse_from(["flotilla", "repo", "owner/repo"]).expect("repo cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Repo(_))));
    }

    #[test]
    fn cli_parses_checkout_noun() {
        let cli = Cli::try_parse_from(["flotilla", "checkout", "my-feature", "remove"]).expect("checkout cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Checkout(_))));
    }

    #[test]
    fn cli_parses_convoy_noun() {
        let cli =
            Cli::try_parse_from(["flotilla", "convoy", "convoy-a", "work", "implement", "complete"]).expect("convoy cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Convoy(_))));
    }

    #[test]
    fn cli_parses_crew_list_and_handoff_grammar() {
        let list = Cli::try_parse_from(["flotilla", "crew", "list"]).expect("crew list should parse");
        assert!(matches!(list.command, Some(SubCommand::Crew(_))));

        let handoff = Cli::try_parse_from(["flotilla", "crew", "reviewer", "handoff", "--message", "Review commit abc123"])
            .expect("crew handoff should parse");
        assert!(matches!(handoff.command, Some(SubCommand::Crew(_))));

        let marked = Cli::try_parse_from(["flotilla", "crew", "@list", "handoff", "--message", "Review commit abc123"])
            .expect("marked crew role should parse");
        assert!(matches!(marked.command, Some(SubCommand::Crew(_))));

        let literal = Cli::try_parse_from(["flotilla", "crew", "--subject", "@reviewer", "handoff", "--message", "Review commit abc123"])
            .expect("literal crew role should parse");
        assert!(matches!(literal.command, Some(SubCommand::Crew(_))));
    }

    #[test]
    fn cli_parses_cr_noun() {
        let cli = Cli::try_parse_from(["flotilla", "cr", "42", "open"]).expect("cr cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Cr(_))));
    }

    #[test]
    fn cli_parses_pr_alias() {
        let cli = Cli::try_parse_from(["flotilla", "pr", "42", "open"]).expect("pr alias should parse");
        assert!(matches!(cli.command, Some(SubCommand::Cr(_))));
    }

    #[test]
    fn cli_parses_issue_noun() {
        let cli = Cli::try_parse_from(["flotilla", "issue", "1", "open"]).expect("issue cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Issue(_))));
    }

    #[test]
    fn cli_parses_agent_noun() {
        let cli = Cli::try_parse_from(["flotilla", "agent", "claude-1", "teleport"]).expect("agent cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Agent(_))));
    }

    #[test]
    fn cli_parses_workspace_noun() {
        let cli = Cli::try_parse_from(["flotilla", "workspace", "feat-ws", "select"]).expect("workspace cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Workspace(_))));
    }

    #[test]
    fn cli_parses_workflow_template_noun() {
        let cli = Cli::try_parse_from(["flotilla", "workflow-template", "scratch", "apply", "--file", "/tmp/x.yaml"])
            .expect("workflow-template cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::WorkflowTemplate(_))));
    }

    #[test]
    fn cli_parses_project_noun() {
        let cli = Cli::try_parse_from(["flotilla", "project", "add", "https://example.com/repo.git", "--name", "my-project"])
            .expect("project cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Project(_))));
    }

    #[test]
    fn cli_parses_convoy_create() {
        let cli = Cli::try_parse_from(["flotilla", "convoy", "my-convoy", "create", "--template", "scratch", "--input", "topic=hi"])
            .expect("convoy create cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Convoy(_))));
    }

    #[test]
    fn cli_parses_dispatch_queue_with_json() {
        let cli = Cli::try_parse_from(["flotilla", "--json", "dispatch", "queue"]).expect("dispatch queue should parse");
        assert!(cli.json);
        assert!(matches!(cli.command, Some(SubCommand::Dispatch(_))));
    }

    #[test]
    fn cli_parses_host_noun() {
        let cli = Cli::try_parse_from(["flotilla", "host", "list"]).expect("host list should parse");
        assert!(matches!(cli.command, Some(SubCommand::Host(_))));
    }

    #[test]
    fn cli_parses_fleet_health_dashboard() {
        let cli = Cli::try_parse_from(["flotilla", "fleet"]).expect("fleet should parse");
        assert!(matches!(cli.command, Some(SubCommand::Fleet)));
    }

    #[test]
    fn cli_parses_environment_noun() {
        let cli = Cli::try_parse_from(["flotilla", "environment", "host:alpha", "refresh"]).expect("environment cli should parse");
        assert!(matches!(cli.command, Some(SubCommand::Environment(_))));
    }

    #[test]
    fn cli_parses_env_alias() {
        let cli = Cli::try_parse_from(["flotilla", "env", "prov:builder-1", "refresh"]).expect("env alias should parse");
        assert!(matches!(cli.command, Some(SubCommand::Environment(_))));
    }

    #[test]
    fn cli_parses_host_status_with_json() {
        let cli = Cli::try_parse_from(["flotilla", "host", "alpha", "status", "--json"]).expect("host status json should parse");
        assert!(matches!(cli.command, Some(SubCommand::Host(_))));
        assert!(cli.json);
    }

    #[test]
    fn cli_global_json_before_subcommand() {
        let cli = Cli::try_parse_from(["flotilla", "--json", "topology"]).expect("json before subcommand should parse");
        assert!(matches!(cli.command, Some(SubCommand::Topology)));
        assert!(cli.json);
    }

    #[test]
    fn cli_repo_context_flag() {
        let cli = Cli::try_parse_from(["flotilla", "--repo", "owner/repo", "checkout", "create", "--branch", "feat-x"])
            .expect("repo context should parse");
        assert!(matches!(cli.command, Some(SubCommand::Checkout(_))));
        assert_eq!(cli.repo.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn cli_no_subcommand_is_none() {
        let cli = Cli::try_parse_from(["flotilla"]).expect("bare cli should parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn interactive_merge_confirmation_marks_command_confirmed() {
        let mut command = flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
        };
        let mut input = std::io::Cursor::new(b"yes\n");
        let mut output = Vec::new();

        let proceed = confirm_command(&mut command, true, &mut input, &mut output).expect("confirmation prompt");

        assert!(proceed);
        assert!(matches!(command.action, flotilla_protocol::CommandAction::MergeChangeRequest { confirmed: true, .. }));
        assert_eq!(String::from_utf8(output).expect("utf8 prompt"), "Merge change request 42 using squash? [y/N] ");
    }

    #[test]
    fn interactive_merge_decline_does_not_dispatch() {
        let mut command = flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
        };
        let mut input = std::io::Cursor::new(b"n\n");
        let mut output = Vec::new();

        assert!(!confirm_command(&mut command, true, &mut input, &mut output).expect("confirmation prompt"));
        assert!(matches!(command.action, flotilla_protocol::CommandAction::MergeChangeRequest { confirmed: false, .. }));
        assert_eq!(String::from_utf8(output).expect("utf8 prompt"), "Merge change request 42 using squash? [y/N] Merge cancelled.\n");
    }

    #[test]
    fn non_interactive_merge_requires_yes_flag() {
        let mut command = flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
        };

        let error = confirm_command(&mut command, false, &mut std::io::empty(), &mut std::io::sink())
            .expect_err("non-interactive merge must require explicit confirmation");

        assert_eq!(error, "merging a change request non-interactively requires --yes");
    }

    #[test]
    fn host_target_selection_uses_host_facing_name() {
        let hosts = vec![HostListEntry {
            environment_id: Some(EnvironmentId::host(HostId::new("desktop-a"))),
            host_name: HostName::new("desktop"),
            node: Some(NodeInfo::new(NodeId::new("node-a"), "Builder")),
            is_local: false,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            reconnect: None,
            has_summary: true,
            repo_count: 0,
        }];

        let (environment_id, node_id) = select_host_target(&hosts, &HostName::new("desktop")).expect("resolve host");
        assert_eq!(environment_id, EnvironmentId::host(HostId::new("desktop-a")));
        assert_eq!(node_id, NodeId::new("node-a"));
    }

    #[test]
    fn host_target_selection_reports_ambiguity_for_duplicate_host_names() {
        let hosts = vec![
            HostListEntry {
                environment_id: Some(EnvironmentId::host(HostId::new("desktop-a"))),
                host_name: HostName::new("desktop"),
                node: Some(NodeInfo::new(NodeId::new("node-a"), "Desktop")),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Connected,
                reconnect: None,
                has_summary: true,
                repo_count: 0,
            },
            HostListEntry {
                environment_id: Some(EnvironmentId::host(HostId::new("desktop-b"))),
                host_name: HostName::new("desktop"),
                node: Some(NodeInfo::new(NodeId::new("node-b"), "Desktop")),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Connected,
                reconnect: None,
                has_summary: true,
                repo_count: 0,
            },
        ];

        let err = select_host_target(&hosts, &HostName::new("desktop")).expect_err("duplicate host names should be ambiguous");
        let message = err.to_string();
        assert!(message.contains("ambiguous host: desktop"), "unexpected error: {message}");
        assert!(message.contains("desktop-a"), "unexpected error: {message}");
        assert!(message.contains("desktop-b"), "unexpected error: {message}");
    }

    #[test]
    fn provisioning_target_for_host_environment_uses_host_target() {
        let host = HostName::new("desktop");
        let environment_id = EnvironmentId::host(HostId::new("desktop-a"));

        let target = provisioning_target_for_environment(&host, &environment_id);
        assert_eq!(target, ProvisioningTarget::Host { host });
    }

    #[test]
    fn provisioning_target_for_non_host_environment_preserves_environment_identity() {
        let host = HostName::new("desktop");
        let environment_id = EnvironmentId::new("builder-1");

        let target = provisioning_target_for_environment(&host, &environment_id);
        assert_eq!(target, ProvisioningTarget::ExistingEnvironment { host, env_id: environment_id });
    }
}
