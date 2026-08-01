use std::{path::PathBuf, sync::OnceLock};

use clap::Parser;
use flotilla_core::path_policy::PathPolicy;

/// Flotilla daemon
#[derive(Parser)]
#[command(version, long_version = binary_version())]
struct Cli {
    /// Config directory
    #[arg(long)]
    config_dir: Option<PathBuf>,

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
        self.socket.clone().unwrap_or_else(|| self.config_dir().join("run/flotilla.sock"))
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    flotilla_core::tls::install_default_provider();
    let cli = Cli::parse();
    let paths = PathPolicy::from_process_env();
    flotilla_daemon::cli::run(&cli.socket_path(), &cli.config_dir(), paths.state_dir.as_path(), cli.timeout).await
}
