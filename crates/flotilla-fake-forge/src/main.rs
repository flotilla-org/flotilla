//! `flotilla-fake-forge serve --config <path>`: run the shared fake Forgejo
//! service for a rehearsal fleet. `flotilla-fake-forge seed` generates its
//! seed from the copied stores' Repository records. See the crate README.

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
    /// Write a seed from Repository records, as `flotilla resource list
    /// repositories` prints them on each rehearsal host.
    Seed {
        /// A `flotilla resource list repositories` output file; repeat for each host.
        #[arg(long = "repositories", required = true)]
        repositories: Vec<PathBuf>,
        /// `SERVICE_URL=MOUNT`: serve the repositories of the forge Repository
        /// records name by SERVICE_URL under MOUNT, e.g. `https://github.com=github`.
        #[arg(long = "forge", required = true, value_parser = parse_mount)]
        forges: Vec<(String, String)>,
        /// Where to write the seed (standard output by default).
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

fn parse_mount(value: &str) -> Result<(String, String), String> {
    match value.rsplit_once('=') {
        Some((service, mount)) if !service.is_empty() && !mount.is_empty() => Ok((service.to_string(), mount.to_string())),
        _ => Err(format!("`{value}` is not SERVICE_URL=MOUNT")),
    }
}

fn write_seed(repositories: &[PathBuf], forges: Vec<(String, String)>, output: Option<PathBuf>) -> Result<(), String> {
    let mut inputs = Vec::new();
    for path in repositories {
        let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        inputs.push(serde_json::from_str(&text).map_err(|error| format!("{} is not JSON: {error}", path.display()))?);
    }
    let generated = flotilla_fake_forge::seed::seed_from_repository_records(&inputs, &forges)?;
    for line in &generated.skipped {
        eprintln!("skipped {line}");
    }
    let count: usize = generated.seed.forges.values().map(|forge| forge.repositories.len()).sum();
    eprintln!("seed holds {count} repositories across {} forges", generated.seed.forges.len());
    let text = serde_json::to_string_pretty(&generated.seed).map_err(|error| error.to_string())? + "\n";
    match output {
        Some(path) => std::fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display())),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    match cli.command {
        Command::Seed { repositories, forges, output } => match write_seed(&repositories, forges, output) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("flotilla-fake-forge seed: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Serve { config } => match flotilla_fake_forge::serve::run(&config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "fake forge failed to start");
                ExitCode::FAILURE
            }
        },
    }
}
