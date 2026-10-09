use std::sync::OnceLock;

use color_eyre::Result;
use flotilla_client::{
    build_id,
    reconnect::{should_reexec_for_incompatible_daemon, REEXEC_BUILD_ENV},
};
use flotilla_core::providers::ProcessCommandRunner;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};
use flotilla_tui::cli::args::{
    attach_mode, AttachArgs, Cli, CompleteArgs, CompletionsArgs, DaemonArgs, DaemonSubCommand, EventsArgs, FleetSubCommand, HookArgs,
    LogsArgs, LsArgs, ResourceListArgs, ResourceSubCommand, SubCommand, TopologyArgs, ViewArgs, WaitArgs,
};

mod cli;
mod fleet_prune;
mod resource_validate;
use cli::{
    dispatch, query_installer_health, run_artifact_command, run_attach, run_complete, run_completions, run_control_command, run_daemon,
    run_daemon_bridge, run_daemon_dev_mode, run_daemon_stop, run_ensure_command, run_fleet_health, run_fleet_list, run_hook,
    run_hooks_command, run_logs, run_manifest_command, run_pm_command, run_resource_command, run_status, run_topology_command, run_tui,
    run_wait, run_watch,
};

fn binary_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| format!("{} (wire={}, proto={})", env!("CARGO_PKG_VERSION"), build_id(), flotilla_protocol::PROTOCOL_VERSION))
}

// Parsing and dispatch run in sequence rather than nested: clap's derived
// builders and the dispatch future both need large frames in unoptimised
// builds, and stacking them overflowed Windows' 1 MiB main thread (#2588).
fn allows_daemon_reexec(command: &Option<SubCommand>) -> bool {
    !matches!(command, Some(SubCommand::Fleet { command: Some(FleetSubCommand::Check | FleetSubCommand::Spread) }))
}

fn main() -> Result<()> {
    flotilla_core::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    flotilla_core::tls::install_default_provider();
    color_eyre::install()?;
    let mut cli =
        Cli::try_parse_with_version(env!("CARGO_PKG_VERSION"), binary_version()).unwrap_or_else(|error| exit_cli_parse_error(error));
    let format = OutputFormat::from_json_flag(cli.json);
    let remote_daemon_selected = cli.remote_daemon().is_ok_and(|endpoint| endpoint.is_some());
    let command = cli.command.take();
    let allow_daemon_reexec = allows_daemon_reexec(&command);

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(Box::pin(run_command(cli, command, format)));

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
        Some(SubCommand::Daemon(DaemonArgs { command: Some(DaemonSubCommand::Stop), .. })) => run_daemon_stop(&cli).await,
        Some(SubCommand::Daemon(DaemonArgs { command: Some(DaemonSubCommand::DevMode { command }), .. })) => {
            run_daemon_dev_mode(&cli, command).await
        }
        Some(SubCommand::Daemon(DaemonArgs { command: None, timeout })) => run_daemon(&cli, timeout).await,
        Some(SubCommand::Status) => run_status(&cli, format).await,
        Some(SubCommand::Watch) => run_watch(&cli, format).await,
        Some(SubCommand::Wait(WaitArgs { leaves, namespace, fresher_than, timeout })) => {
            run_wait(&cli, leaves, namespace, fresher_than, timeout, format).await
        }
        Some(SubCommand::Topology(TopologyArgs { dot })) => run_topology_command(&cli, format, dot).await,
        Some(SubCommand::Logs(LogsArgs { host, since, level, target })) => run_logs(&cli, host.as_deref(), since, level, target).await,
        Some(SubCommand::Fleet { command: None }) => run_fleet_health(&cli, format).await,
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Check) }) => {
            flotilla_core::fleet_health::check(query_installer_health(&cli).await).map_err(|error| color_eyre::eyre::eyre!(error))
        }
        Some(SubCommand::Fleet { command: Some(FleetSubCommand::Spread) }) => {
            let result = query_installer_health(&cli).await;
            match &result {
                Err(error) | Ok(CommandValue::Error { message: error }) => eprintln!("fleet status: {error}"),
                _ => {}
            }
            print!("{}", flotilla_core::fleet_health::spread(result));
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
        Some(SubCommand::Resource { command }) => run_resource_command(&cli, command, format).await,
        Some(SubCommand::Manifest { command }) => run_manifest_command(&cli, command, format).await,
        Some(SubCommand::Artifact { command }) => run_artifact_command(&cli, command, format).await,
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

        None => run_tui(cli, None).await,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn failed_incompatible_daemon_reexec_names_both_builds_and_protocols() {
        let mismatch = "daemon protocol version mismatch: client built cli-old speaks proto 19; daemon built daemon-new speaks proto 20";

        let message = incompatible_daemon_reexec_failure(mismatch, &std::io::Error::from_raw_os_error(libc::ENOENT));

        assert!(message.contains("client built cli-old speaks proto 19"), "{message}");
        assert!(message.contains("daemon built daemon-new speaks proto 20"), "{message}");
        assert!(message.contains("re-exec could not reach a matching build"), "{message}");
    }
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
