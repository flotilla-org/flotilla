use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_core::{
    config::ConfigStore,
    log_file::{rotating_log_writer, DAEMON_LOG_DIRECTORY, DAEMON_LOG_FILE},
    path_context::DaemonHostPath,
    path_policy::PathPolicy,
    providers::discovery::DiscoveryRuntime,
};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    resource_limits::raise_file_descriptor_limit, restart_history::DaemonLifecycle, runtime::DaemonRuntime, server::DaemonServer,
    DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH,
};

pub async fn run(socket_path: &Path, config_dir: &Path, state_dir: &Path, timeout_secs: u64) -> Result<(), String> {
    let lifecycle = DaemonLifecycle::begin(state_dir)?;
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
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|error| format!("initialize daemon logging: {error}"))?;
    raise_file_descriptor_limit();

    let timeout = if timeout_secs == 0 { Duration::from_secs(u64::MAX) } else { Duration::from_secs(timeout_secs) };

    let repo_roots = config.load_and_migrate_repos();
    info!(repo_count = repo_roots.len(), "starting daemon");

    let discovery = DiscoveryRuntime::for_process(daemon_config.follower);
    let repo_root_paths = repo_roots.into_iter().map(|p| p.into_path_buf()).collect();
    let server = DaemonServer::new_with_socket_discovery_path(
        repo_root_paths,
        Arc::clone(&config),
        discovery,
        socket_path.to_path_buf(),
        stable_socket_discovery_path(),
        timeout,
    )
    .await?;
    let daemon = server.daemon();
    let runtime = DaemonRuntime::start(daemon, Arc::clone(&config), Some(socket_path.to_path_buf())).await?;

    let result = server.run().await;
    // Stop background tasks after the accept loop ends. SIGTERM, SIGINT, idle
    // timeout, and explicit shutdown return Ok and clear the lifecycle marker;
    // a server error deliberately leaves it behind as an abnormal exit.
    runtime.shutdown();
    if result.is_ok() {
        lifecycle.finish()?;
    }
    result
}

fn stable_socket_discovery_path() -> PathBuf {
    // This path is a dialer-facing contract derived from the remote user's
    // home, so deliberately do not inherit config-directory overrides.
    let home_policy = PathPolicy::from_env(|key| if key == "HOME" { std::env::var_os(key) } else { None });
    home_policy.config_dir.into_path_buf().join(DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::fd::AsRawFd};

    use flotilla_core::log_file::{rotating_log_writer, DAEMON_LOG_FILE};
    use tempfile::{tempdir, TempDir};
    use tracing_subscriber::{layer::SubscriberExt, Registry};

    use super::*;

    #[tokio::test]
    async fn lifecycle_loser_never_removes_live_daemon_socket() {
        let temp = TempDir::new().expect("tempdir");
        let state_dir = temp.path().join("state");
        let config_dir = temp.path().join("config");
        let socket_path = config_dir.join("run/flotilla.sock");
        std::fs::create_dir_all(socket_path.parent().expect("socket parent")).expect("create socket parent");
        std::fs::write(&socket_path, "owned by live daemon").expect("create live socket stand-in");

        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let lock_path = state_dir.join(flotilla_core::DAEMON_LIFECYCLE_LOCK_FILE);
        let lifecycle =
            std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock_path).expect("open lifecycle lock");
        // SAFETY: the file owns this descriptor for the rest of the test.
        assert_eq!(unsafe { libc::flock(lifecycle.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0, "hold lifecycle lock");

        let error = run(&socket_path, &config_dir, &state_dir, 0).await.expect_err("competing daemon must lose lifecycle lock");

        assert!(error.contains("another flotillad process holds the lifecycle lock"), "unexpected error: {error}");
        assert_eq!(std::fs::read_to_string(&socket_path).expect("live socket stand-in survives"), "owned by live daemon");
    }

    #[test]
    fn structured_file_layer_writes_boot_record() {
        let log_dir = tempdir().expect("log directory");
        let writer = rotating_log_writer(log_dir.path(), DAEMON_LOG_FILE, 1024 * 1024, 1).expect("rotating log writer");
        let file_layer = tracing_subscriber::fmt::layer().json().with_ansi(false).with_writer(writer);
        let subscriber = Registry::default().with(file_layer);

        tracing::subscriber::with_default(subscriber, || tracing::info!(repo_count = 0, "starting daemon"));

        let contents = fs::read_to_string(log_dir.path().join(DAEMON_LOG_FILE)).expect("structured daemon log");
        let record: serde_json::Value = serde_json::from_str(contents.trim()).expect("JSON log record");
        assert_eq!(record["level"], "INFO");
        assert_eq!(record["fields"]["message"], "starting daemon");
        assert_eq!(record["fields"]["repo_count"], 0);
    }
}
