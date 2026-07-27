use std::{io::IsTerminal, path::Path, sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore,
    log_file::{rotating_log_writer, DAEMON_LOG_DIRECTORY, DAEMON_LOG_FILE},
    path_context::DaemonHostPath,
    providers::discovery::DiscoveryRuntime,
};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{runtime::DaemonRuntime, server::DaemonServer};

pub async fn run(socket_path: &Path, config_dir: &Path, state_dir: &Path, timeout_secs: u64) -> Result<(), String> {
    let config = Arc::new(ConfigStore::new(DaemonHostPath::new(config_dir), DaemonHostPath::new(state_dir)));
    let daemon_config = config.load_daemon_config()?;

    // Hardcoded directives are appended after RUST_LOG and take precedence,
    // so these noisy crates stay at INFO even if RUST_LOG sets them to DEBUG.
    let filter_builder =
        tracing_subscriber::EnvFilter::builder().with_default_directive(tracing_subscriber::filter::LevelFilter::DEBUG.into());
    let filter = match daemon_config.logging.filter.as_deref() {
        Some(directives) => filter_builder.parse(directives).map_err(|error| format!("invalid daemon logging filter: {error}"))?,
        None => filter_builder.from_env_lossy(),
    };
    let filter = ["h2=info", "hyper=info", "reqwest=info", "rustls=info"]
        .into_iter()
        .fold(filter, |f, d| f.add_directive(d.parse().expect("valid directive")));
    let log_dir = state_dir.join(DAEMON_LOG_DIRECTORY);
    let file_appender = rotating_log_writer(&log_dir, DAEMON_LOG_FILE, daemon_config.logging.max_bytes, daemon_config.logging.generations)
        .map_err(|err| format!("open rotating daemon log: {err}"))?;
    // Detached daemons redirect stderr to an unrotated panic log. Keep the
    // human-readable layer for interactive runs without duplicating the
    // structured stream into that file.
    let stderr_layer = std::io::stderr().is_terminal().then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    let file_layer = tracing_subscriber::fmt::layer().json().with_ansi(false).with_writer(file_appender);
    tracing_subscriber::registry().with(filter).with(stderr_layer).with(file_layer).try_init().ok();

    let timeout = if timeout_secs == 0 { Duration::from_secs(u64::MAX) } else { Duration::from_secs(timeout_secs) };

    let repo_roots = config.load_and_migrate_repos();
    info!(repo_count = repo_roots.len(), "starting daemon");

    let discovery = DiscoveryRuntime::for_process(daemon_config.follower);
    let repo_root_paths = repo_roots.into_iter().map(|p| p.into_path_buf()).collect();
    let server = DaemonServer::new(repo_root_paths, Arc::clone(&config), discovery, socket_path.to_path_buf(), timeout).await?;
    let daemon = server.daemon();
    let runtime = DaemonRuntime::start(daemon, Arc::clone(&config), Some(socket_path.to_path_buf())).await?;

    let result = server.run().await;
    // Every path out of `run` here is an intended stop (SIGTERM, SIGINT, idle
    // timeout, explicit shutdown); say so, so the runtime's Drop ERROR keeps
    // meaning "this went away when it should not have".
    runtime.shutdown();
    result
}
