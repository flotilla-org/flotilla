//! `flotilla table`: hands off to the flotilla-table sibling beside this binary (#4249), passing
//! the global flags this CLI resolved ahead of the user's arguments.

use std::{ffi::OsString, sync::OnceLock};

use color_eyre::{eyre::eyre, Result};
use flotilla_cli::cli::args::{Cli, CliPaths, TableArgs};
use flotilla_client::{
    build_id,
    endpoint::SshEndpoint,
    sibling::{hand_off, Sibling},
};

use super::daemon::connect_daemon;

/// The status a sibling that ran as a child (not exec'd: Windows) exited with, for this process
/// to exit with once the runtime has shut down.
static EXIT_STATUS: OnceLock<i32> = OnceLock::new();

pub(crate) fn handoff_exit_status() -> Option<i32> {
    EXIT_STATUS.get().copied()
}

pub(crate) async fn run_table_command(cli: &Cli, args: TableArgs) -> Result<()> {
    let remote = cli.remote_daemon().map_err(|error| eyre!(error))?;
    let paths = match remote {
        Some(_) => None,
        None => {
            // Start the daemon if none is running, as other commands do; the sibling only reads
            // its socket. Not for help, nor where there is no socket to read (the sibling says so).
            if cfg!(unix) && !asks_for_help(&args.args) {
                drop(connect_daemon(cli).await?);
            }
            Some(cli.client_paths().map_err(|error| eyre!(error))?)
        }
    };
    let handed = handoff_args(remote.as_ref(), paths.as_ref(), cli.json, args.args);
    let status = hand_off(Sibling::Table, handed, build_id()).map_err(|error| eyre!(error))?;
    let _ = EXIT_STATUS.set(status);
    Ok(())
}

fn asks_for_help(args: &[OsString]) -> bool {
    args.iter().any(|arg| ["-h", "--help", "-V", "--version"].iter().any(|flag| arg == flag))
}

/// flotilla-table's arguments: the daemon this CLI resolved (a remote one, which the sibling
/// refuses until #4241, or this host's socket and config directory), `--json`, then `user`'s.
fn handoff_args(remote: Option<&SshEndpoint>, paths: Option<&CliPaths>, json: bool, user: Vec<OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(remote) = remote {
        args.extend(["--daemon".into(), remote.to_string().into()]);
    }
    if let Some(paths) = paths {
        args.extend(["--socket".into(), paths.socket_path.clone().into(), "--config-dir".into(), paths.config_dir.clone().into()]);
    }
    if json {
        args.push("--json".into());
    }
    args.extend(user);
    args
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use flotilla_cli::cli::args::SubCommand;

    use super::*;

    fn table_args(cli: &Cli) -> Vec<OsString> {
        match &cli.command {
            Some(SubCommand::Table(table)) => table.args.clone(),
            _ => panic!("not the table command"),
        }
    }

    #[test]
    fn table_passes_its_arguments_through_after_the_resolved_globals() {
        let cli = Cli::try_parse_from(["flotilla", "--json", "table", "convoys", "--where", "status.phase==Landed", "--help"])
            .expect("table parses");
        let user = table_args(&cli);
        assert_eq!(user, ["convoys", "--where", "status.phase==Landed", "--help"], "flotilla-table's flags, its --help included");
        assert!(asks_for_help(&user));

        let paths = CliPaths {
            config_dir: PathBuf::from("/c"),
            state_dir: PathBuf::from("/s"),
            socket_path: PathBuf::from("/c/run/flotilla.sock"),
        };
        assert_eq!(
            handoff_args(None, Some(&paths), cli.json, user.clone()),
            ["--socket", "/c/run/flotilla.sock", "--config-dir", "/c", "--json", "convoys", "--where", "status.phase==Landed", "--help"]
        );
        let remote = SshEndpoint::parse("ssh://udder").expect("remote");
        assert_eq!(handoff_args(Some(&remote), None, false, vec!["convoys".into()]), ["--daemon", "ssh://udder", "convoys"]);

        let cli = Cli::try_parse_from(["flotilla", "table"]).expect("bare table lists");
        assert!(table_args(&cli).is_empty());
    }
}
