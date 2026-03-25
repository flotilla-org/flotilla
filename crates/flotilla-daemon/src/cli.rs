use std::{io::IsTerminal, path::Path, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, path_context::DaemonHostPath, providers::discovery::DiscoveryRuntime};
use tracing::info;

use crate::server::DaemonServer;

pub async fn run(socket_path: &Path, config_dir: &Path, state_dir: &Path, timeout_secs: u64) -> Result<(), String> {
    // Hardcoded directives are appended after RUST_LOG and take precedence,
    // so these noisy crates stay at INFO even if RUST_LOG sets them to DEBUG.
    let filter = ["h2=info", "hyper=info", "reqwest=info", "rustls=info"].into_iter().fold(
        tracing_subscriber::EnvFilter::builder()
            .with_default_directive(tracing_subscriber::filter::LevelFilter::DEBUG.into())
            .from_env_lossy(),
        |f, d| f.add_directive(d.parse().expect("valid directive")),
    );
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(filter)
        .try_init()
        .ok();

    let timeout = if timeout_secs == 0 { Duration::from_secs(u64::MAX) } else { Duration::from_secs(timeout_secs) };

    let config = Arc::new(ConfigStore::new(DaemonHostPath::new(config_dir), DaemonHostPath::new(state_dir)));
    let repo_roots = config.load_repos();
    info!(repo_count = repo_roots.len(), "starting daemon");

    let daemon_config = config.load_daemon_config();
    let discovery = DiscoveryRuntime::for_process(daemon_config.follower);
    let repo_root_paths = repo_roots.into_iter().map(|p| p.into_path_buf()).collect();
    let server = DaemonServer::new(repo_root_paths, config, discovery, socket_path.to_path_buf(), timeout).await?;

    server.run().await
}
