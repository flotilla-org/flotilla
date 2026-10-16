//! `flotilla-fake-forge serve --config <path>`: run the shared fake Forgejo
//! service for a rehearsal fleet. See the crate README.

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

#[derive(Parser)]
#[command(name = "flotilla-fake-forge", version, about = "A fake Forgejo service shared by a rehearsal fleet")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the Forgejo API and the admin API until SIGTERM or SIGINT.
    Serve {
        /// The service's TOML configuration.
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    match cli.command {
        Command::Serve { config } => match flotilla_fake_forge::serve::run(&config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "fake forge failed to start");
                ExitCode::FAILURE
            }
        },
    }
}
