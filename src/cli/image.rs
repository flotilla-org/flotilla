use color_eyre::Result;
use flotilla_commands::commands::image::{ImageNoun, ImageVerb};
use flotilla_protocol::{output::OutputFormat, Command, CommandAction};
use flotilla_tui::cli::args::Cli;

use super::{
    daemon::{connect_daemon, reset_sigpipe},
    resource::exit_command_error,
};

/// `flotilla image check`: the daemon resolves and get-or-creates the build;
/// this side waits for it and exits non-zero on a refusal, a failed build or
/// a timeout.
pub(crate) async fn run_image_command(cli: &Cli, noun: ImageNoun, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let ImageVerb::Check(args) = noun.verb;
    let daemon = connect_daemon(cli).await?;
    let command = Command::builder().action(CommandAction::ImageCheck { request: args.request() }).build();
    match flotilla_tui::cli::image_check::run_image_check(&*daemon, command, args.wait_for(), format).await {
        Ok(outcome) if outcome.failed() => std::process::exit(1),
        Ok(_) => Ok(()),
        Err(message) => exit_command_error(message, format),
    }
}
