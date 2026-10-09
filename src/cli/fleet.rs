use std::path::PathBuf;

use color_eyre::Result;
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};

use flotilla_tui::cli::args::{Cli, CliPaths, PmSubCommand};

use super::daemon::{connect_cli_socket, host_daemon_socket_required};
use super::resource::run_control_command;

pub(crate) async fn run_pm_command(cli: &Cli, command: PmSubCommand) -> Result<()> {
    match command {
        PmSubCommand::Connect { zellij_bin, plugin_url, wheelhouse_socket, flotilla_bin } => {
            let flotilla_bin = resolve_pm_flotilla_bin(flotilla_bin, std::env::current_exe)?;
            let options = flotilla_tui::pm_connect::PmConnectOptions::builder()
                .maybe_zellij_bin(zellij_bin)
                .maybe_plugin_url(plugin_url)
                .maybe_wheelhouse_socket(wheelhouse_socket)
                .flotilla_bin(flotilla_bin)
                .build();
            let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
            let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
            flotilla_tui::pm_connect::run(
                remote,
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

fn resolve_pm_flotilla_bin(override_bin: Option<String>, current_exe: impl FnOnce() -> std::io::Result<PathBuf>) -> Result<String> {
    if let Some(override_bin) = override_bin {
        return Ok(override_bin);
    }

    let current_exe = current_exe().map_err(|error| color_eyre::eyre::eyre!("resolve running Flotilla executable: {error}"))?;
    current_exe
        .into_os_string()
        .into_string()
        .map_err(|path| color_eyre::eyre::eyre!("running Flotilla executable path is not valid UTF-8: {}", PathBuf::from(path).display()))
}

pub(crate) async fn run_fleet_list(cli: &Cli, format: OutputFormat, project: Option<String>, all: bool) -> Result<()> {
    let (crew_id, convoy) = ls_crew_scope(project.as_deref(), all, |key| std::env::var(key).ok());
    run_control_command(
        cli,
        Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::QueryFleetList { project, crew_id, convoy },
        },
        format,
    )
    .await
}

fn ls_crew_scope(project: Option<&str>, all: bool, env: impl Fn(&str) -> Option<String>) -> (Option<String>, Option<String>) {
    if project.is_some() || all {
        return (None, None);
    }
    let crew_id = env("FLOTILLA_CREW_ID");
    let convoy = if crew_id.is_none() { env("FLOTILLA_CONVOY") } else { None };
    (crew_id, convoy)
}

pub(crate) async fn query_installer_health(cli: &Cli) -> Result<CommandValue, String> {
    cli.require_local_daemon("fleet health").map_err(|error| error.to_string())?;
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths()?;
    // Readiness belongs to this wire generation: never spawn, restart or
    // re-exec during install confirmation to bypass a fingerprint mismatch.
    let daemon = connect_cli_socket(None, &socket_path, &config_dir, &state_dir, true).await?;
    daemon
        .execute_query(
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetHealth {} },
            uuid::Uuid::new_v4(),
        )
        .await
}

pub(crate) async fn run_fleet_health(cli: &Cli, format: OutputFormat) -> Result<()> {
    run_control_command(
        cli,
        Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetHealth {} },
        format,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn pm_connector_defaults_to_running_executable_and_override_wins() {
        let discovered =
            resolve_pm_flotilla_bin(None, || Ok(PathBuf::from("/different/from/path/flotilla"))).expect("discover running executable");
        assert_eq!(discovered, "/different/from/path/flotilla");

        let overridden = resolve_pm_flotilla_bin(Some("remote-flotilla".to_owned()), || {
            panic!("explicit override must not inspect the connector executable")
        })
        .expect("use explicit override");
        assert_eq!(overridden, "remote-flotilla");
    }

    #[test]
    fn pm_connector_reports_executable_discovery_failure() {
        let error = resolve_pm_flotilla_bin(None, || Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing executable")))
            .expect_err("discovery failure should be reported");
        assert!(error.to_string().contains("resolve running Flotilla executable: missing executable"));
    }

    #[test]
    fn ls_scope_defaults_to_crew_and_explicit_flags_win() {
        let env = |key: &str| match key {
            "FLOTILLA_CREW_ID" => Some("crew-1".to_string()),
            "FLOTILLA_CONVOY" => Some("convoy-1".to_string()),
            _ => None,
        };
        assert_eq!(super::ls_crew_scope(None, false, env), (Some("crew-1".to_string()), None));
        assert_eq!(super::ls_crew_scope(Some("island"), false, env), (None, None));
        assert_eq!(super::ls_crew_scope(None, true, env), (None, None));
        assert_eq!(super::ls_crew_scope(None, false, |_| None), (None, None));
        assert_eq!(
            super::ls_crew_scope(None, false, |key| (key == "FLOTILLA_CONVOY").then(|| "convoy-1".to_string())),
            (None, Some("convoy-1".to_string()))
        );
    }
}
