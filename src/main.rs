use color_eyre::Result;
use flotilla_cli::cli::args::{
    attach_mode, AttachArgs, Cli, CompleteArgs, CompletionsArgs, DaemonArgs, DaemonSubCommand, DomainCommand, EventsArgs, FleetSubCommand,
    HookArgs, LogsArgs, LsArgs, ResourceListArgs, ResourceSubCommand, SubCommand, TopologyArgs, ViewArgs, WaitArgs,
};
use flotilla_client::{
    binary_version, build_id,
    reconnect::{incompatible_daemon_reexec_failure, reexec_current_process, should_reexec_for_incompatible_daemon, REEXEC_BUILD_ENV},
};
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};
use flotilla_provider_runtime::ProcessCommandRunner;

mod cli;
mod fleet_prune;
mod resource_validate;
use cli::{
    dispatch, query_installer_health, run_artifact_command, run_attach, run_complete, run_completions, run_control_command, run_daemon,
    run_daemon_bridge, run_daemon_dev_mode, run_daemon_stop, run_ensure_command, run_fleet_health, run_fleet_list, run_hook,
    run_hooks_command, run_image_command, run_logs, run_manifest_command, run_pm_command, run_resource_command, run_status,
    run_table_command, run_topology_command, run_tui, run_wait, run_watch, run_web_command,
};

// Parsing and dispatch run in sequence rather than nested: clap's derived
// builders and the dispatch future both need large frames in unoptimised
// builds, and stacking them overflowed Windows' 1 MiB main thread (#2588).
fn allows_daemon_reexec(command: &Option<SubCommand>) -> bool {
    !matches!(
        command,
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Check | FleetSubCommand::Spread | FleetSubCommand::Status) })
    )
}

fn main() -> Result<()> {
    flotilla_daemon_api::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    rigging_tls::install_default_provider();
    color_eyre::install()?;
    let mut cli = Cli::try_parse_with_version(env!("CARGO_PKG_VERSION"), binary_version(env!("CARGO_PKG_VERSION")))
        .unwrap_or_else(|error| exit_cli_parse_error(error));
    let format = OutputFormat::from_json_flag(cli.json);
    let remote_daemon_selected = cli.remote_daemon().is_ok_and(|endpoint| endpoint.is_some());
    let command = cli.command.take();
    let allow_daemon_reexec = allows_daemon_reexec(&command);

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(Box::pin(run_command(cli, command, format)));
    // The runtime shuts down before this exits with the status of a sibling it ran as a child.
    drop(runtime);
    if let Some(status) = cli::handoff_exit_status() {
        std::process::exit(status);
    }

    if let Err(error) = &result {
        let message = format!("{error:?}");
        let already_reexecuted = std::env::var(REEXEC_BUILD_ENV).ok();
        if allow_daemon_reexec && should_reexec_for_incompatible_daemon(&message, already_reexecuted.as_deref(), remote_daemon_selected) {
            std::env::set_var(REEXEC_BUILD_ENV, build_id());
            if let Err(reexec_error) = reexec_current_process() {
                return Err(color_eyre::eyre::eyre!(incompatible_daemon_reexec_failure(&message, &reexec_error)));
            }
        }
    }
    result
}

async fn run_command(cli: Cli, command: Option<SubCommand>, format: OutputFormat) -> Result<()> {
    match command {
        Some(SubCommand::View(ViewArgs { address })) => {
            // Parse before handing off so a bad address in a recipe fails
            // loudly at the shell (ADR 0013).
            if let Err(e) = address.parse::<flotilla_protocol::ViewAddress>() {
                eprintln!("Error: {e}");
                std::process::exit(2);
            }
            run_tui()
        }
        Some(SubCommand::Daemon(DaemonArgs { command: Some(DaemonSubCommand::Stop), .. })) => run_daemon_stop(&cli).await,
        Some(SubCommand::Daemon(DaemonArgs { command: Some(DaemonSubCommand::DevMode { command }), .. })) => {
            run_daemon_dev_mode(&cli, command).await
        }
        Some(SubCommand::Daemon(DaemonArgs { command: None, timeout, http_listen })) => {
            run_daemon(&cli, timeout, http_listen.as_deref()).await
        }
        Some(SubCommand::Status) => run_status(&cli, format).await,
        Some(SubCommand::Watch) => run_watch(&cli, format).await,
        Some(SubCommand::Wait(WaitArgs { leaves, namespace, fresher_than, timeout })) => {
            run_wait(&cli, leaves, namespace, fresher_than, timeout, format).await
        }
        Some(SubCommand::Topology(TopologyArgs { dot })) => run_topology_command(&cli, format, dot).await,
        Some(SubCommand::Logs(LogsArgs { host, since, level, target })) => run_logs(&cli, host.as_deref(), since, level, target).await,
        Some(SubCommand::Fleet { command: None }) => run_fleet_health(&cli, format).await,
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Check) }) => {
            let refused = flotilla_projections::fleet_health::check(query_installer_health(&cli).await)
                .map_err(|error| color_eyre::eyre::eyre!(error))?;
            for line in refused {
                eprintln!("{line}");
            }
            Ok(())
        }
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Spread) }) => {
            let result = query_installer_health(&cli).await;
            match &result {
                Err(error) | Ok(CommandValue::Error { message: error }) => eprintln!("fleet status: {error}"),
                _ => {}
            }
            print!("{}", flotilla_projections::fleet_health::spread(result));
            Ok(())
        }
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Status) }) => {
            cli.require_local_daemon("fleet status")?;
            let paths = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
            // scripts/fleet-install appends to the same file in its DAEMON_CONFIG_DIR.
            let log = match std::fs::read_to_string(paths.config_dir.join("fleet-rolls.log")) {
                Ok(log) => log,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(color_eyre::eyre::eyre!("cannot read the fleet roll log: {error}")),
            };
            let host = flotilla_protocol::HostName::local();
            print!("{}", flotilla_projections::fleet_health::rolls(host.as_str(), &log, 3));
            Ok(())
        }
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Prune { fleet_root, keep_others, dry_run }) }) => {
            if cli.daemon.is_some() {
                return Err(color_eyre::eyre::eyre!("fleet prune operates on local generations; drop --daemon"));
            }
            fleet_prune::run(&fleet_root, &keep_others, dry_run, &ProcessCommandRunner)
                .await
                .map_err(|error| color_eyre::eyre::eyre!(error))
        }
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::PostInstall { cleat_bin, generation, diagnostics_dir }) }) => {
            cli.require_local_daemon("fleet post-install")?;
            run_control_command(
                &cli,
                Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::FleetPostInstall { cleat_bin, generation, diagnostics_dir },
                },
                format,
            )
            .await
        }
        Some(SubCommand::Ls(LsArgs { project, all })) => run_fleet_list(&cli, format, project, all).await,
        Some(SubCommand::Attach(AttachArgs { reference, watch, strict, take, transient, host })) => {
            run_attach(&cli, &reference, attach_mode(watch, strict, take), transient, host.as_deref(), format).await
        }
        Some(SubCommand::Hook(HookArgs { harness, event_type, payload })) => {
            run_hook(&cli, &harness, &event_type, payload.as_deref()).await
        }
        Some(SubCommand::Hooks { command }) => run_hooks_command(&command).await,
        Some(SubCommand::Pm { command }) => run_pm_command(&cli, command).await,
        Some(SubCommand::Ensure { command }) => run_ensure_command(&cli, command, format).await,
        Some(SubCommand::Web { command }) => run_web_command(&cli, command).await,
        Some(SubCommand::Table(args)) => run_table_command(&cli, args).await,
        Some(SubCommand::Tui(_)) => run_tui(),
        Some(SubCommand::Resource { command }) => run_resource_command(&cli, command, format).await,
        Some(SubCommand::Manifest { command }) => run_manifest_command(&cli, command, format).await,
        Some(SubCommand::Artifact { command }) => run_artifact_command(&cli, command, format).await,
        Some(SubCommand::Archive { command }) => flotilla_cli::frontend::archive::run_archive_command(&cli, command, format).await,
        Some(SubCommand::Events(EventsArgs { namespace, host, local_only })) => {
            run_resource_command(
                &cli,
                ResourceSubCommand::List(ResourceListArgs {
                    kind: "events".to_string(),
                    project: None,
                    namespace,
                    host,
                    local_only,
                    include_replicas: false,
                }),
                format,
            )
            .await
        }
        Some(SubCommand::Domain(DomainCommand::Image(noun))) => run_image_command(&cli, noun, format).await,
        Some(SubCommand::Domain(command)) => dispatch(command.resolve()?, &cli, format).await,

        Some(SubCommand::DaemonBridge) => run_daemon_bridge(&cli).await,
        Some(SubCommand::Complete(CompleteArgs { line, cursor_pos })) => {
            run_complete(&line, cursor_pos);
            Ok(())
        }
        Some(SubCommand::Completions(CompletionsArgs { shell })) => {
            run_completions(shell);
            Ok(())
        }

        None => run_tui(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // Installer readiness and diagnostics must query the selected generation;
    // an incompatible wire generation must not cause either command to re-exec its CLI.
    #[test]
    fn installer_health_commands_require_exact_wire_generation() {
        for subcommand in ["check", "spread"] {
            let cli = Cli::try_parse_from(["flotilla", "--socket", "/fleet/run/daemon.sock", "fleet", subcommand]).expect("health command");
            assert!(!allows_daemon_reexec(&cli.command));
            assert_eq!(cli.remote_daemon().expect("local socket"), None);
        }
        let cli = Cli::try_parse_from(["flotilla", "fleet"]).expect("dashboard");
        assert!(allows_daemon_reexec(&cli.command));
    }
}
