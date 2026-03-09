use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use flotilla_core::config::ConfigStore;

pub async fn run(socket_path: &Path, timeout_secs: u64) -> Result<(), String> {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::DEBUG.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init()
        .ok();

    let timeout = if timeout_secs == 0 {
        Duration::from_secs(u64::MAX)
    } else {
        Duration::from_secs(timeout_secs)
    };

    let config = Arc::new(ConfigStore::new());
    let repo_roots = config.load_repos();
    info!("starting daemon with {} repo(s)", repo_roots.len());

    let server =
        crate::server::DaemonServer::new(repo_roots, config, socket_path.to_path_buf(), timeout)
            .await;

    server.run().await
}
