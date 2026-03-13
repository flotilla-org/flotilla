use std::{path::Path, sync::Arc, time::Duration};

use flotilla_core::config::ConfigStore;
use tracing::info;

use crate::server::DaemonServer;

pub async fn run(socket_path: &Path, timeout_secs: u64) -> Result<(), String> {
    // Hardcoded directives are appended after RUST_LOG and take precedence,
    // so these noisy crates stay at INFO even if RUST_LOG sets them to DEBUG.
    let filter = ["h2=info", "hyper=info", "reqwest=info", "rustls=info"].into_iter().fold(
        tracing_subscriber::EnvFilter::builder()
            .with_default_directive(tracing_subscriber::filter::LevelFilter::DEBUG.into())
            .from_env_lossy(),
        |f, d| f.add_directive(d.parse().expect("valid directive")),
    );
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(filter).try_init().ok();

    let timeout = if timeout_secs == 0 { Duration::from_secs(u64::MAX) } else { Duration::from_secs(timeout_secs) };

    let config = Arc::new(ConfigStore::new());
    let repo_roots = config.load_repos();
    info!(repo_count = repo_roots.len(), "starting daemon");

    let server = DaemonServer::new(repo_roots, config, socket_path.to_path_buf(), timeout).await?;

    server.run().await
}
