//! `flotilla table`: hands off to the flotilla-table sibling beside this binary (#4249), passing
//! the global flags this CLI resolved ahead of the user's arguments.

use color_eyre::Result;
use flotilla_cli::{
    cli::args::{Cli, TableArgs},
    frontend::table::{asks_for_help, run_table},
};
use flotilla_client::build_id;

use super::{daemon::connect_daemon, sibling::record_exit_status};

pub(crate) async fn run_table_command(cli: &Cli, args: TableArgs) -> Result<()> {
    // Start the daemon if none is running, as other commands do; the sibling only reads its
    // socket. Not for a remote daemon or help, nor where there is no socket to read (the sibling
    // says so).
    let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
    if remote.is_none() && cfg!(unix) && !asks_for_help(&args.args) {
        drop(connect_daemon(cli).await?);
    }
    record_exit_status(run_table(cli, args, build_id())?);
    Ok(())
}
