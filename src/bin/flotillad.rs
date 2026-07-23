use std::path::PathBuf;

use clap::Parser;
use flotilla_core::path_policy::PathPolicy;

/// Flotilla daemon
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config directory
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Socket path (default: ${config_dir}/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Idle timeout in seconds (0 = no timeout)
    #[arg(long, default_value = "300")]
    timeout: u64,
}

impl Cli {
    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone().unwrap_or_else(|| PathPolicy::from_process_env().config_dir.into_path_buf())
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(|| self.config_dir().join("flotilla.sock"))
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    flotilla_core::tls::install_default_provider();
    let cli = Cli::parse();
    let paths = PathPolicy::from_process_env();
    flotilla_daemon::cli::run(&cli.socket_path(), &cli.config_dir(), paths.state_dir.as_path(), cli.timeout).await
}
