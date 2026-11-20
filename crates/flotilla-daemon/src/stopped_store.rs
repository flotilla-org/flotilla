//! A stopped daemon's engine store, served as its first start serves it, for pre-roll validation.
//!
//! `flotilla resource validate --from-store` runs the checks `--from-daemon` runs. They read
//! through a daemon's resource socket, so a stopped host's store is opened by a daemon composed
//! as the candidate composes one, over a copy of the store, with none of the controllers that
//! make a running daemon act on the host. The copy keeps the real store as the roll finds it:
//! opening a store migrates it, and the first start does that to the real one.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_config::ConfigStore;
use flotilla_core::{
    host_identity::resolve_local_environment_state_dir,
    in_process::{decided, InProcessDaemon},
};
use flotilla_discovery::DiscoveryRuntime;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::HostName;
use tokio::{net::UnixListener, task::JoinHandle};

use crate::{peer::HostPeerOverlay, server::serve_local_http};

/// A copy of a stopped host's engine store, opened by a daemon that serves its resource API on
/// [`StoppedStore::socket`] until this is dropped.
pub struct StoppedStore {
    socket: PathBuf,
    _daemon: Arc<InProcessDaemon>,
    serving: JoinHandle<()>,
    followers: Vec<JoinHandle<()>>,
    // Removes the copy when dropped, after the daemon that has it open.
    _copy: tempfile::TempDir,
}

impl StoppedStore {
    /// Opens the engine store in `config`'s state directory, with `observation_roots` as the
    /// daemon's start would track them. `None` when the state directory holds no store, so the
    /// first start begins empty.
    pub async fn open(config: &ConfigStore, observation_roots: Vec<PathBuf>) -> Result<Option<Self>, String> {
        let state_dir = config.state_dir().as_path();
        let engine = state_dir.join(decided::ENGINE_STORE);
        if !engine.exists() {
            return Ok(None);
        }
        let copy =
            tempfile::Builder::new().prefix("flotilla-validate-").tempdir().map_err(|error| format!("create the store copy: {error}"))?;
        let copied_state = copy.path().join("state");
        std::fs::create_dir_all(&copied_state).map_err(|error| format!("create {}: {error}", copied_state.display()))?;
        for suffix in ["", "-wal"] {
            let from = PathBuf::from(format!("{}{suffix}", engine.display()));
            if suffix.is_empty() || from.exists() {
                let to = copied_state.join(format!("{}{suffix}", decided::ENGINE_STORE));
                std::fs::copy(&from, &to).map_err(|error| format!("copy {}: {error}", from.display()))?;
            }
        }
        let daemon_config = config.load_daemon_config()?;
        let runner = flotilla_provider_runtime::ProcessCommandRunner;
        // The host's identity, so its own records are its own.
        let machine_id = daemon_config.machine_id.as_deref();
        let host_state = resolve_local_environment_state_dir(state_dir, machine_id, &runner).await;
        let copied_host_state = resolve_local_environment_state_dir(&copied_state, machine_id, &runner).await;
        let host_id = host_state.join("host-id");
        if host_id.exists() {
            std::fs::create_dir_all(&copied_host_state).map_err(|error| format!("create {}: {error}", copied_host_state.display()))?;
            std::fs::copy(&host_id, copied_host_state.join("host-id")).map_err(|error| format!("copy {}: {error}", host_id.display()))?;
        }

        let copied = Arc::new(ConfigStore::new(config.base_path().clone(), DaemonHostPath::new(copied_state.clone())));
        let mut discovery = DiscoveryRuntime::for_process();
        discovery.factories.link_vessel_agents(flotilla_drydock::agent::link());
        let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
        let daemon = InProcessDaemon::new_composed(
            observation_roots,
            copied,
            discovery,
            host_name,
            flotilla_store_memory::engine_home_backend(),
            Arc::new(flotilla_resources::SystemClock),
            Arc::new(HostPeerOverlay::default()),
            decided::opener(&copied_state),
        )
        .await
        .map_err(|error| format!("open a copy of {}: {error}", engine.display()))?;

        let followers =
            crate::runtime::open_engine_reads(&daemon).await.map_err(|error| format!("read a copy of {}: {error}", engine.display()))?;

        let socket = copy.path().join("resources.sock");
        let listener = UnixListener::bind(&socket).map_err(|error| format!("bind {}: {error}", socket.display()))?;
        let serving = tokio::spawn(serve_resource_socket(listener, Arc::clone(&daemon)));
        Ok(Some(Self { socket, _daemon: daemon, serving, followers, _copy: copy }))
    }

    /// The socket the copy's resource API is served on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for StoppedStore {
    fn drop(&mut self) {
        self.serving.abort();
        for follower in &self.followers {
            follower.abort();
        }
    }
}

/// Serves `daemon`'s resource API to every connection `listener` accepts, as the main socket
/// serves the operator's process.
pub async fn serve_resource_socket(listener: UnixListener, daemon: Arc<InProcessDaemon>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve_local_http(stream, Arc::clone(&daemon)));
    }
}
