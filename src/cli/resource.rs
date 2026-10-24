use crate::resource_validate;
use color_eyre::Result;
use flotilla_cli::cli::args::{Cli, ResourceSubCommand};
pub(crate) use flotilla_cli::frontend::resource::{dispatch, exit_command_error, run_artifact_command, run_control_command};
use flotilla_config::ConfigStore;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::output::OutputFormat;
#[cfg(unix)]
use std::path::Path;

pub(crate) async fn run_resource_command(cli: &Cli, command: ResourceSubCommand, format: OutputFormat) -> Result<()> {
    match command {
        ResourceSubCommand::Validate { path, from_daemon, host, skill_catalog, skill_sources, skill_probe_tokens } => {
            if from_daemon {
                let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
                let local_roots = if host.is_none() {
                    let config = ConfigStore::new(DaemonHostPath::new(&paths.config_dir), DaemonHostPath::new(&paths.state_dir));
                    Some(
                        config
                            .load_observation_roots()
                            .map_err(|error| color_eyre::eyre::eyre!(error))?
                            .into_iter()
                            .map(|path| path.into_path_buf())
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                };
                let socket = if let Some(host) = &host {
                    if host.is_empty() || host.contains(['/', '\\', '\0']) || host == "." || host == ".." {
                        return Err(color_eyre::eyre::eyre!("invalid peer host name: {host}"));
                    }
                    paths.state_dir.join("peers").join(format!("{host}.sock"))
                } else {
                    paths.socket_path
                };
                #[cfg(unix)]
                let result = resource_validate::validate_daemon_with_options(
                    &socket,
                    local_roots.as_deref(),
                    skill_catalog.as_deref(),
                    &resource_validate::ValidationOptions {
                        require_import: ConfigStore::new(DaemonHostPath::new(&paths.config_dir), DaemonHostPath::new(&paths.state_dir))
                            .load_daemon_config()
                            .map_err(|error| color_eyre::eyre::eyre!(error))?
                            .primitives
                            .is_some(),
                        sources: skill_sources.or_else(|| skill_catalog.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))),
                        credential_tokens: resource_validate::frozen::load_tokens(skill_probe_tokens.as_deref())?,
                    },
                )
                .await;
                #[cfg(not(unix))]
                let result = {
                    if skill_sources.is_some() || skill_probe_tokens.is_some() {
                        return Err(color_eyre::eyre::eyre!("frozen-reference daemon validation requires Unix"));
                    }
                    resource_validate::validate_daemon(&socket, local_roots.as_deref(), skill_catalog.as_deref()).await
                };
                result.map(|_| ()).map_err(|error| {
                    color_eyre::eyre::eyre!("resource validation on {}: {error:#}", host.as_deref().unwrap_or("local host"))
                })
            } else {
                resource_validate::validate_path(&path.expect("clap requires path without --from-daemon"), skill_catalog.as_deref())
            }
        }
        command => flotilla_cli::frontend::resource::run_resource_command(cli, command, format).await,
    }
}
