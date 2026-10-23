//! The pushed flotilla CLI. Its dependency graph has neither the daemon nor TUI.
use color_eyre::{eyre::eyre, Result};
use flotilla_cli::{
    cli::args::{Cli, SubCommand},
    frontend,
};
use flotilla_protocol::output::OutputFormat;

fn main() -> Result<()> {
    std::env::set_var("FLOTILLA_CONTAINED_HOST_DAEMON", "1");
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> Result<()> {
    flotilla_daemon_api::build_info::initialize_build_id(option_env!("FLOTILLA_BUILD_ID").unwrap_or(env!("CARGO_PKG_VERSION")));
    color_eyre::install()?;
    // Every target/repository helper connects to the vessel's host endpoint.
    // A missing host socket is an error, never a request to spawn flotillad.
    let mut cli = Cli::try_parse_with_version(env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_VERSION")).unwrap_or_else(|error| error.exit());
    let format = OutputFormat::from_json_flag(cli.json);
    match cli.command.take() {
        Some(SubCommand::Domain(command)) => {
            frontend::dispatch(command.resolve().map_err(color_eyre::eyre::Report::msg)?, &cli, format).await
        }
        Some(SubCommand::Attach(args)) => {
            frontend::attach::run_attach(
                &cli,
                &args.reference,
                flotilla_cli::cli::args::attach_mode(args.watch, args.strict, args.take),
                args.transient,
                args.host.as_deref(),
                format,
            )
            .await
        }
        Some(SubCommand::Resource { command }) => frontend::resource::run_resource_command(&cli, command, format).await,
        Some(SubCommand::Manifest { command }) => frontend::manifest::run_manifest_command(&cli, command, format).await,
        Some(SubCommand::Ensure { command }) => frontend::manifest::run_ensure_command(&cli, command, format).await,
        Some(SubCommand::Status) => frontend::logs::run_status(&cli, format).await,
        Some(SubCommand::Watch) => frontend::logs::run_watch(&cli, format).await,
        Some(SubCommand::Wait(args)) => {
            frontend::logs::run_wait(&cli, args.leaves, args.namespace, args.fresher_than, args.timeout, format).await
        }
        Some(SubCommand::Logs(args)) => frontend::logs::run_logs(&cli, args.host.as_deref(), args.since, args.level, args.target).await,
        Some(SubCommand::Topology(args)) => frontend::logs::run_topology_command(&cli, format, args.dot).await,
        Some(SubCommand::Hook(args)) => frontend::hooks::run_hook(&cli, &args.harness, &args.event_type, args.payload.as_deref()).await,
        Some(SubCommand::Hooks { command }) => frontend::hooks::run_hooks_command(&command).await,
        Some(SubCommand::Artifact { command }) => frontend::artifact::run_artifact_command(&cli, command, format).await,
        Some(SubCommand::Complete(args)) => {
            frontend::hooks::run_complete(&args.line, args.cursor_pos);
            Ok(())
        }
        Some(SubCommand::Completions(args)) => {
            frontend::hooks::run_completions(args.shell);
            Ok(())
        }
        _ => Err(eyre!("this vessel CLI provides domain commands, artifacts and hooks; use the host CLI for operator surfaces")),
    }
}
