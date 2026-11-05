//! `flotilla-tui`, the old dashboard beside `flotilla`; `flotilla` with no subcommand and
//! `flotilla view` hand off to it (#4263). It parses `flotilla`'s own global flags.

use color_eyre::{eyre::eyre, Result};
use flotilla_cli::cli::args::{Cli, SubCommand, TuiArgs, ViewArgs};
use flotilla_client::{
    build_id,
    reconnect::{incompatible_daemon_reexec_failure, reexec_current_process, should_reexec_for_incompatible_daemon, REEXEC_BUILD_ENV},
    sibling::{check_parent_build, PARENT_BUILD_ENV},
};

mod dashboard;

/// The version `flotilla --version` reports, naming the build, so a release can check this sibling.
fn binary_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| format!("{} (wire={}, proto={})", env!("CARGO_PKG_VERSION"), build_id(), flotilla_protocol::PROTOCOL_VERSION))
}

fn main() -> Result<()> {
    flotilla_daemon_api::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    rigging_tls::install_default_provider();
    color_eyre::install()?;
    let parent = std::env::var(PARENT_BUILD_ENV).ok();
    check_parent_build("flotilla-tui", parent.as_deref(), env!("FLOTILLA_BUILD_ID")).map_err(|error| eyre!(error))?;
    // Checked once: a re-exec into a newer build below runs it on its own.
    std::env::remove_var(PARENT_BUILD_ENV);
    let mut cli = Cli::try_parse_with_version(env!("CARGO_PKG_VERSION"), binary_version()).unwrap_or_else(|error| error.exit());
    let scoped_view = match cli.command.take() {
        Some(SubCommand::Tui(TuiArgs { args })) if asks_for_version(&args) => {
            println!("flotilla-tui {}", binary_version());
            return Ok(());
        }
        command => scoped_view(command)?,
    };
    let remote_daemon_selected = cli.remote_daemon().is_ok_and(|endpoint| endpoint.is_some());

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(Box::pin(dashboard::run_tui(cli, scoped_view)));
    drop(runtime);

    if let Err(error) = &result {
        let message = format!("{error:?}");
        let already_reexecuted = std::env::var(REEXEC_BUILD_ENV).ok();
        if should_reexec_for_incompatible_daemon(&message, already_reexecuted.as_deref(), remote_daemon_selected) {
            std::env::set_var(REEXEC_BUILD_ENV, build_id());
            if let Err(reexec_error) = reexec_current_process() {
                return Err(eyre!(incompatible_daemon_reexec_failure(&message, &reexec_error)));
            }
        }
    }
    result
}

/// The View the dashboard is scoped to: none for the full dashboard, or `view`'s address.
fn scoped_view(command: Option<SubCommand>) -> Result<Option<flotilla_protocol::ViewAddress>> {
    match command {
        None => Ok(None),
        Some(SubCommand::Tui(TuiArgs { args })) if args.is_empty() => Ok(None),
        Some(SubCommand::View(ViewArgs { address })) => address.parse().map(Some).map_err(|error| eyre!("{error}")),
        Some(_) => Err(eyre!("flotilla-tui runs the dashboard, or `view ADDRESS`; run other commands with flotilla")),
    }
}

/// Whether `flotilla tui`'s arguments ask only for the version.
fn asks_for_version(args: &[std::ffi::OsString]) -> bool {
    matches!(args, [flag] if flag == "--version" || flag == "-V")
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::Parser;

    fn command(args: &[&str]) -> Option<SubCommand> {
        Cli::try_parse_from(args).expect("parses").command
    }

    #[test]
    fn the_dashboard_runs_whole_or_scoped_to_one_view_and_refuses_other_commands() {
        assert_eq!(scoped_view(command(&["flotilla-tui", "--theme", "classic"])).expect("whole dashboard"), None);
        assert_eq!(scoped_view(command(&["flotilla-tui", "tui"])).expect("`flotilla tui`"), None);
        assert!(scoped_view(command(&["flotilla-tui", "tui", "convoys"])).is_err());
        let scoped = scoped_view(command(&["flotilla-tui", "view", "convoys/flotilla"])).expect("scoped");
        assert_eq!(scoped, Some("convoys/flotilla".parse().expect("address")));
        assert!(scoped_view(command(&["flotilla-tui", "view", "not a view"])).is_err());
        let refused = scoped_view(command(&["flotilla-tui", "status"])).unwrap_err().to_string();
        assert!(refused.contains("run other commands with flotilla"), "{refused}");
    }

    #[test]
    fn flotilla_tui_version_is_the_only_argument_tui_reads() {
        assert!(asks_for_version(&["--version".into()]));
        assert!(asks_for_version(&["-V".into()]));
        assert!(!asks_for_version(&[]));
        assert!(!asks_for_version(&["--version".into(), "x".into()]));
    }
}
