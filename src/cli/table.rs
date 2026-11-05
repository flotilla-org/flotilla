//! `flotilla table`: hands off to the flotilla-table sibling beside this binary (#4249), passing
//! the global flags this CLI resolved ahead of the user's arguments.

use std::sync::OnceLock;

use color_eyre::{eyre::eyre, Result};
use flotilla_cli::{
    cli::args::{Cli, TableArgs},
    frontend::table::{asks_for_help, run_table},
};
use flotilla_client::build_id;

use super::daemon::connect_daemon;

/// The status a sibling that ran as a child (not exec'd: Windows) exited with, for this process
/// to exit with once the runtime has shut down.
static EXIT_STATUS: OnceLock<i32> = OnceLock::new();

pub(crate) fn handoff_exit_status() -> Option<i32> {
    EXIT_STATUS.get().copied()
}

pub(crate) async fn run_table_command(cli: &Cli, args: TableArgs) -> Result<()> {
    // Start the daemon if none is running, as other commands do; the sibling only reads its
    // socket. Not for a remote daemon or help, nor where there is no socket to read (the sibling
    // says so).
    let remote = cli.remote_daemon().map_err(|error| eyre!(error))?;
    if remote.is_none() && cfg!(unix) && !asks_for_help(&args.args) {
        drop(connect_daemon(cli).await?);
    }
    let status = run_table(cli, args, build_id())?;
    let _ = EXIT_STATUS.set(status);
    Ok(())
}
