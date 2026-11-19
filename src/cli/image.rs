use color_eyre::Result;
use flotilla_cli::cli::args::Cli;
use flotilla_commands::commands::image::{ImageNoun, ImageVerb};
use flotilla_protocol::{output::OutputFormat, Command, CommandAction, CommandValue};

use super::{
    daemon::{connect_daemon, reset_sigpipe},
    resource::exit_command_error,
};

/// `flotilla image check`: the daemon resolves and demands the build; this
/// side waits for it and exits non-zero on a refusal, a failed build or a
/// timeout. `flotilla image retry` launches a given-up build again.
pub(crate) async fn run_image_command(cli: &Cli, noun: ImageNoun, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let args = match noun.verb {
        ImageVerb::Check(args) => args,
        ImageVerb::Retry(args) => return run_image_retry(cli, args.name, format).await,
    };
    let daemon = connect_daemon(cli).await?;
    let command = Command::builder().action(CommandAction::ImageCheck { request: args.request() }).build();
    match flotilla_cli::cli::image_check::run_image_check(&*daemon, command, args.wait_for(), format).await {
        Ok(outcome) if outcome.failed() => std::process::exit(1),
        Ok(_) => Ok(()),
        Err(message) => exit_command_error(message, format),
    }
}

async fn run_image_retry(cli: &Cli, name: String, format: OutputFormat) -> Result<()> {
    let daemon = connect_daemon(cli).await?;
    let command = Command::builder().action(CommandAction::ImageBuildRetry { name }).build();
    match flotilla_cli::cli::run_command(&*daemon, command, format).await {
        Ok(CommandValue::Error { .. }) => std::process::exit(1),
        Ok(_) => Ok(()),
        Err(message) => exit_command_error(message, format),
    }
}
