use std::{path::PathBuf, sync::OnceLock};

use clap::Parser;
use flotilla_core::path_policy::{daemon_dirs_for_socket, daemon_socket_path, PathPolicy};

/// Flotilla daemon
#[derive(Parser)]
#[command(version, long_version = binary_version())]
struct Cli {
    /// Config directory
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// State directory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Socket path (default: ${config_dir}/run/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Idle timeout in seconds (0 = no timeout)
    #[arg(long, default_value = "300")]
    timeout: u64,
}

fn binary_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| format!("{} (wire={})", env!("CARGO_PKG_VERSION"), flotilla_client::BUILD_ID))
}

impl Cli {
    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone().unwrap_or_else(|| PathPolicy::from_process_env().config_dir.into_path_buf())
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(|| daemon_socket_path(&self.config_dir()))
    }

    fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| PathPolicy::from_process_env().state_dir.into_path_buf())
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    flotilla_core::tls::install_default_provider();
    let cli = Cli::parse();
    let config_dir = cli.config_dir();
    let state_dir = cli.state_dir();
    let socket_path = cli.socket_path();
    let (config_dir, state_dir) = daemon_dirs_for_socket(&socket_path, &config_dir, &state_dir)?;
    flotilla_daemon::cli::run(&socket_path, &config_dir, &state_dir, cli.timeout).await
}
