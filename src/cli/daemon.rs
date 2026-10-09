use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use color_eyre::Result;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        vcs::{git::GitVcs, VcsInspection},
        ProcessCommandRunner,
    },
};
use flotilla_protocol::{commands::CommandValue, Command, CommandAction, ProjectListResponse, RepoIdentity, RepoInfo, ViewAddress};
use flotilla_tui::{
    app, event_log,
    socket::{DaemonEndpoint, SshEndpoint},
    theme,
};
use tracing::info;

use flotilla_tui::cli::args::{Cli, CliPaths, DevModeSubCommand};

pub(super) fn host_daemon_socket_required(contained_marker: Option<&std::ffi::OsStr>) -> bool {
    contained_marker.is_some()
}

pub(super) async fn connect_cli_socket(
    remote: Option<&SshEndpoint>,
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    require_host_daemon: bool,
) -> Result<Arc<flotilla_tui::socket::SocketDaemon>, String> {
    let surface =
        cli_surface_from(std::env::var("FLOTILLA_CREW_ROLE").ok().as_deref(), std::env::var("FLOTILLA_NAMESPACE").ok().as_deref());
    let endpoint = remote.cloned().map(DaemonEndpoint::Ssh).unwrap_or_else(|| DaemonEndpoint::Local(socket_path.to_path_buf()));
    flotilla_tui::socket::connect_endpoint_or_spawn_with_surface(&endpoint, config_dir, state_dir, require_host_daemon, surface).await
}

pub(super) fn cli_surface_from(crew_role: Option<&str>, namespace: Option<&str>) -> flotilla_protocol::SurfaceDeclaration {
    let namespace = namespace.unwrap_or("flotilla");
    let principal_ref = match crew_role.filter(|role| !role.trim().is_empty()) {
        Some(role) => flotilla_protocol::PrincipalRef { namespace: namespace.to_string(), name: format!("{role} agent") },
        None => flotilla_protocol::PrincipalRef::implicit_for_namespace(namespace),
    };
    flotilla_protocol::SurfaceDeclaration { principal_ref, character: flotilla_protocol::SurfaceCharacter::Focal }
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
pub(crate) async fn run_tui(cli: Cli, scoped_view: Option<flotilla_protocol::ViewAddress>) -> Result<()> {
    let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let resolved_state_dir = DaemonHostPath::new(paths.state_dir);
    event_log::init_with_dir(resolved_state_dir.as_path());
    let startup = std::time::Instant::now();
    let resolved_config_dir = paths.config_dir;
    let config = Arc::new(ConfigStore::new(DaemonHostPath::new(&resolved_config_dir), resolved_state_dir.clone()));

    let startup_repo_roots = startup_repo_roots(&cli.repo_root).await;
    let cli_theme = cli.theme.clone();
    let socket_path = paths.socket_path;
    let require_host_daemon =
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref());

    // Connect to the daemon concurrently with terminal initialization.
    let daemon_log_path =
        resolved_state_dir.as_path().join(flotilla_core::log_file::DAEMON_LOG_DIRECTORY).join(flotilla_core::log_file::DAEMON_LOG_FILE);
    let daemon_panic_log_path = resolved_config_dir.join("daemon-panic.log");
    let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let initial_remote = remote.clone();
    let initial_socket_path = socket_path.clone();
    let initial_config_dir = resolved_config_dir.clone();
    let initial_state_dir = resolved_state_dir.clone();
    let daemon_task = tokio::spawn(async move {
        connect_cli_socket(
            initial_remote.as_ref(),
            &initial_socket_path,
            &initial_config_dir,
            initial_state_dir.as_path(),
            require_host_daemon,
        )
        .await
        .map(|d| d as Arc<dyn DaemonHandle>)
    });

    let mut terminal = ratatui::init();
    flotilla_tui::terminal::install_panic_hook();
    #[cfg(unix)]
    flotilla_tui::terminal::install_sigterm_handler();
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

    let theme_name = cli_theme.or_else(|| config.load_config().ui.theme.clone()).unwrap_or_else(|| "catppuccin-mocha".to_string());
    let initial_theme = theme::theme_by_name(&theme_name);
    if !initial_theme.name.eq_ignore_ascii_case(&theme_name) {
        tracing::warn!(requested = %theme_name, using = %initial_theme.name, "unknown theme, falling back");
    }

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
    };
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
            || connect_cli_socket(remote.as_ref(), &socket_path, &resolved_config_dir, resolved_state_dir.as_path(), require_host_daemon),
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
            app.ui.notifications.push(app::ui_state::NotificationKind::Info, "Re-executed and reconnected to daemon".to_string());
        }
        Err(error) => tracing::warn!(%error, path = %path.display(), "could not restore TUI re-exec handoff"),
    }
}

/// The remote half of `--daemon ssh://...`: copy stdio to this host's daemon
/// socket. It never spawns a daemon; a missing one is reported to the client.
pub(crate) async fn run_daemon_bridge(cli: &Cli) -> Result<()> {
    cli.require_local_daemon("daemon-bridge")?;
    #[cfg(unix)]
    {
        flotilla_tui::socket::bridge::bridge_stdio(&cli.socket_path()).await.map_err(|error| color_eyre::eyre::eyre!(error))
    }
    #[cfg(not(unix))]
    {
        Err(color_eyre::eyre::eyre!("daemon-bridge serves a local Unix daemon socket, which this platform does not host"))
    }
}

pub(crate) async fn run_daemon(cli: &Cli, timeout_secs: u64) -> Result<()> {
    cli.require_local_daemon("daemon")?;
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

pub(crate) async fn run_daemon_stop(cli: &Cli) -> Result<()> {
    cli.require_local_daemon("daemon stop")?;
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

pub(crate) async fn run_daemon_dev_mode(cli: &Cli, command: DevModeSubCommand) -> Result<()> {
    cli.require_local_daemon("daemon dev-mode")?;
    match command {
        DevModeSubCommand::Enable => {
            // Disable first. Even if graceful shutdown fails, the supervisor cannot
            // resurrect the fleet daemon while we finish unloading the job.
            set_fleet_daemon_enabled(false)?;
            let socket_path = cli.socket_path();
            let shutdown_error =
                if socket_path.exists() { flotilla_tui::socket::shutdown_existing(&socket_path).await.err() } else { None };
            stop_fleet_daemon_service()?;
            if socket_path.exists() {
                wait_for_socket_removal(&socket_path).await?;
            }
            if let Some(error) = shutdown_error {
                tracing::debug!(%error, "fleet daemon did not accept graceful shutdown before supervisor stop");
            }
            println!("Daemon dev mode enabled; the fleet daemon service is disabled and stopped.");
            Ok(())
        }
        DevModeSubCommand::Disable => {
            set_fleet_daemon_enabled(true)?;
            start_fleet_daemon_service()?;
            println!("Daemon dev mode disabled; the fleet daemon service is enabled and started.");
            Ok(())
        }
    }
}

#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(unused_variables))]
fn set_fleet_daemon_enabled(enabled: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    return flotilla_tui::socket::launchd::set_agent_enabled(enabled).map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(target_os = "linux")]
    return flotilla_tui::socket::systemd::set_unit_enabled(enabled).map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(color_eyre::eyre::eyre!("daemon dev mode is only available on macOS and Linux fleet hosts"))
}

fn stop_fleet_daemon_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    return flotilla_tui::socket::launchd::bootout_agent().map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(target_os = "linux")]
    return flotilla_tui::socket::systemd::stop_unit().map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(color_eyre::eyre::eyre!("daemon dev mode is only available on macOS and Linux fleet hosts"))
}

fn start_fleet_daemon_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        flotilla_tui::socket::launchd::bootstrap_agent().map_err(|error| color_eyre::eyre::eyre!(error))?;
        flotilla_tui::socket::launchd::kickstart_agent().map_err(|error| color_eyre::eyre::eyre!(error))
    }
    #[cfg(target_os = "linux")]
    return flotilla_tui::socket::systemd::start_unit().map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(color_eyre::eyre::eyre!("daemon dev mode is only available on macOS and Linux fleet hosts"))
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
pub(super) fn reset_sigpipe() {
    // SAFETY: libc::signal is safe to call before I/O begins. Tokio does not configure SIGPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
pub(super) fn reset_sigpipe() {}

pub(super) async fn connect_daemon(cli: &Cli) -> Result<Arc<dyn DaemonHandle>> {
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let daemon = connect_cli_socket(
        remote.as_ref(),
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref()),
    )
    .await
    .map_err(|e| color_eyre::eyre::eyre!(e))?;
    Ok(daemon as Arc<dyn DaemonHandle>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use flotilla_protocol::{
        ProjectListEntry, ProjectListRepository, ProjectListResponse, RepoIdentity, RepoInfo, RepoLabels, RepositoryKey, ViewAddress,
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
            .default_workflow_ref("single-agent".to_string())
            .build()
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
    fn fresh_unadopted_cwd_lands_on_the_global_convoy_view() {
        let landing = default_project_landing(&[], &[PathBuf::from("/unadopted")], &ProjectListResponse { projects: vec![] });
        assert_eq!(landing, None);
        let views = flotilla_tui::app::open_views::OpenViews::seed_with_landing(landing);
        assert_eq!(views.active_address(), Some(&ViewAddress::Convoys { namespace: "flotilla".into(), scope: None }));
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

    #[test]
    fn contained_cli_socket_environment_requires_the_host_daemon() {
        assert!(host_daemon_socket_required(Some(std::ffi::OsStr::new("1"))));
        assert!(!host_daemon_socket_required(None));
    }
}
