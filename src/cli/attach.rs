use flotilla_tui::terminal::client_attach_plan;

use color_eyre::Result;
use flotilla_core::config::ConfigStore;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction, RepoSelector};

use flotilla_tui::cli::args::Cli;

use super::daemon::{connect_daemon, reset_sigpipe};
use super::targets::{resolve_cli_repository, resolve_optional_cwd_repository, resolve_repo_from_env};

pub(crate) async fn run_attach(
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
        None => std::env::current_dir().ok().map(RepoSelector::Path),
    };
    let context_repo = match context_repo {
        Some(RepoSelector::Path(path)) => resolve_optional_cwd_repository(&*daemon, path).await.map_err(color_eyre::eyre::Report::msg)?,
        Some(selector) => Some(resolve_cli_repository(&*daemon, selector).await.map_err(color_eyre::eyre::Report::msg)?),
        None => None,
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
        CommandValue::AttachCommandResolved { plan, binding } => {
            let plan = client_attach_plan(
                &cli.daemon_endpoint().map_err(color_eyre::eyre::Report::msg)?,
                plan,
                binding.as_ref(),
                reference,
                mode,
                || {
                    let paths = cli.client_paths()?;
                    ConfigStore::with_base(&paths.config_dir).load_hosts()
                },
            )
            .map_err(color_eyre::eyre::Report::msg)?;
            match format {
                OutputFormat::Json => {
                    println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::AttachCommandResolved { plan, binding }));
                    Ok(())
                }
                OutputFormat::Human => {
                    if let Some(phase) = binding.as_ref().and_then(|binding| binding.convoy_phase) {
                        eprintln!("Convoy phase: {phase}");
                    }
                    if !transient {
                        stamp_pane_identity(reference, binding.as_ref()).await;
                    }
                    run_attach_plan(&plan)
                }
            }
        }
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
pub(super) async fn stamp_pane_identity(reference: &str, binding: Option<&flotilla_protocol::AttachBinding>) {
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

pub(super) fn run_attach_plan(plan: &flotilla_protocol::ResolvedAttachPlan) -> Result<()> {
    flotilla_tui::terminal::exec_attach_plan(plan).map(|never| match never {}).map_err(color_eyre::eyre::Report::msg)
}
