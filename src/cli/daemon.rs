use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use color_eyre::Result;
use flotilla_client::{DaemonEndpoint, SshEndpoint};
use flotilla_daemon_api::daemon::DaemonHandle;
#[cfg(not(feature = "tui"))]
use flotilla_protocol::ViewAddress;

use flotilla_cli::cli::args::{Cli, CliPaths, DevModeSubCommand};

#[cfg(feature = "tui")]
mod dashboard;
#[cfg(feature = "tui")]
pub(crate) use dashboard::run_tui;

pub(super) fn host_daemon_socket_required(contained_marker: Option<&std::ffi::OsStr>) -> bool {
    contained_marker.is_some()
}

pub(super) async fn connect_cli_socket(
    remote: Option<&SshEndpoint>,
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    require_host_daemon: bool,
) -> Result<Arc<flotilla_client::SocketDaemon>, String> {
    let surface =
        cli_surface_from(std::env::var("FLOTILLA_CREW_ROLE").ok().as_deref(), std::env::var("FLOTILLA_NAMESPACE").ok().as_deref());
    let endpoint = remote.cloned().map(DaemonEndpoint::Ssh).unwrap_or_else(|| DaemonEndpoint::Local(socket_path.to_path_buf()));
    flotilla_client::connect_endpoint_or_spawn_with_surface(&endpoint, config_dir, state_dir, require_host_daemon, surface).await
}

pub(super) fn cli_surface_from(crew_role: Option<&str>, namespace: Option<&str>) -> flotilla_protocol::SurfaceDeclaration {
    let namespace = namespace.unwrap_or("flotilla");
    let principal_ref = match crew_role.filter(|role| !role.trim().is_empty()) {
        Some(role) => flotilla_protocol::PrincipalRef { namespace: namespace.to_string(), name: format!("{role} agent") },
        None => flotilla_protocol::PrincipalRef::implicit_for_namespace(namespace),
    };
    flotilla_protocol::SurfaceDeclaration { principal_ref, character: flotilla_protocol::SurfaceCharacter::Focal }
}

/// The remote half of `--daemon ssh://...`: copy stdio to this host's daemon
/// socket. It never spawns a daemon; a missing one is reported to the client.
pub(crate) async fn run_daemon_bridge(cli: &Cli) -> Result<()> {
    cli.require_local_daemon("daemon-bridge")?;
    #[cfg(unix)]
    {
        flotilla_client::bridge::bridge_stdio(&cli.socket_path()).await.map_err(|error| color_eyre::eyre::eyre!(error))
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
    flotilla_paths::path_policy::ensure_daemon_socket_belongs_to_config(&socket_path, &config_dir)
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
    let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let outcome = flotilla_client::shutdown_existing_or_absent(&socket_path, &paths.state_dir)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("could not stop daemon at {}: {error}", socket_path.display()))?;
    if outcome == flotilla_client::ShutdownOutcome::NotRunning {
        println!("Daemon is not running.");
        return Ok(());
    }
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
            let shutdown_error = if socket_path.exists() { flotilla_client::shutdown_existing(&socket_path).await.err() } else { None };
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
    return flotilla_client::launchd::set_agent_enabled(enabled).map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(target_os = "linux")]
    return flotilla_client::systemd::set_unit_enabled(enabled).map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(color_eyre::eyre::eyre!("daemon dev mode is only available on macOS and Linux fleet hosts"))
}

fn stop_fleet_daemon_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    return flotilla_client::launchd::bootout_agent().map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(target_os = "linux")]
    return flotilla_client::systemd::stop_unit().map_err(|error| color_eyre::eyre::eyre!(error));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(color_eyre::eyre::eyre!("daemon dev mode is only available on macOS and Linux fleet hosts"))
}

fn start_fleet_daemon_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        flotilla_client::launchd::bootstrap_agent().map_err(|error| color_eyre::eyre::eyre!(error))?;
        flotilla_client::launchd::kickstart_agent().map_err(|error| color_eyre::eyre::eyre!(error))
    }
    #[cfg(target_os = "linux")]
    return flotilla_client::systemd::start_unit().map_err(|error| color_eyre::eyre::eyre!(error));
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
        host_daemon_socket_required(std::env::var_os(flotilla_environment::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref()),
    )
    .await
    .map_err(|e| color_eyre::eyre::eyre!(e))?;
    Ok(daemon as Arc<dyn DaemonHandle>)
}

#[cfg(not(feature = "tui"))]
pub(crate) async fn run_tui(_cli: Cli, _scoped_view: Option<ViewAddress>) -> Result<()> {
    Err(color_eyre::eyre::eyre!("this binary was built without the TUI; use an operator build with the tui feature"))
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn contained_cli_socket_environment_requires_the_host_daemon() {
        assert!(host_daemon_socket_required(Some(std::ffi::OsStr::new("1"))));
        assert!(!host_daemon_socket_required(None));
    }
}
