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
        ResourceSubCommand::Validate {
            path,
            from_daemon,
            from_store,
            host,
            skill_catalog,
            skill_sources,
            skill_probe_tokens,
            busy_retry_secs,
        } => {
            if from_store {
                #[cfg(unix)]
                {
                    let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
                    let config = ConfigStore::new(DaemonHostPath::new(&paths.config_dir), DaemonHostPath::new(&paths.state_dir));
                    let roots = config
                        .load_observation_roots()
                        .map_err(|error| color_eyre::eyre::eyre!(error))?
                        .into_iter()
                        .map(|path| path.into_path_buf())
                        .collect::<Vec<_>>();
                    let identity = store_identity(&config).await?;
                    return resource_validate::validate_store_with_options(
                        &paths.state_dir,
                        identity,
                        &roots,
                        skill_catalog.as_deref(),
                        &resource_validate::ValidationOptions {
                            require_import: true,
                            sources: skill_sources.or_else(|| skill_catalog.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))),
                            credential_tokens: resource_validate::frozen::load_tokens(skill_probe_tokens.as_deref())?,
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| color_eyre::eyre::eyre!("stopped-store validation of {}: {error:#}", paths.state_dir.display()));
                }
                #[cfg(not(unix))]
                return Err(color_eyre::eyre::eyre!("stopped-store validation requires Unix"));
            }
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
                        // Every daemon runs on the engine, so every validation rehearses the import.
                        require_import: true,
                        sources: skill_sources.or_else(|| skill_catalog.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))),
                        credential_tokens: resource_validate::frozen::load_tokens(skill_probe_tokens.as_deref())?,
                        busy_retry: busy_retry_secs.map(std::time::Duration::from_secs).unwrap_or(resource_validate::DEFAULT_BUSY_RETRY),
                    },
                )
                .await;
                #[cfg(not(unix))]
                let result = {
                    let _ = busy_retry_secs;
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

/// The home and host a stopped daemon's first start imports as, resolved as that start resolves
/// them (`InProcessDaemon::new_composed`). On a host whose daemon has run they are read; on one
/// that never ran a daemon they are created, as its first start would create them.
#[cfg(unix)]
async fn store_identity(config: &ConfigStore) -> Result<resource_validate::StoreIdentity> {
    use flotilla_core::host_identity::{resolve_local_host_id, resolve_local_node_id};
    let machine_id = config.load_daemon_config().map_err(|error| color_eyre::eyre::eyre!(error))?.machine_id;
    let runner = flotilla_provider_runtime::ProcessCommandRunner;
    let home = resolve_local_node_id(config.base_path().as_path(), machine_id.as_deref(), &runner)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("resolve the daemon's node id: {error}"))?;
    let host = resolve_local_host_id(config.state_dir().as_path(), machine_id.as_deref(), &runner)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("resolve the daemon's host id: {error}"))?;
    Ok(resource_validate::StoreIdentity { home: home.as_str().to_owned(), own_host: Some(host.to_string()) })
}
