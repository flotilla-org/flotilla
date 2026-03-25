use std::collections::HashMap;

use flotilla_protocol::{DaemonHostPath, EnvironmentId};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tracing::info;

pub struct EnvironmentSocketRegistry {
    sockets: HashMap<EnvironmentId, (JoinHandle<()>, DaemonHostPath)>,
}

impl EnvironmentSocketRegistry {
    pub fn new() -> Self {
        Self { sockets: HashMap::new() }
    }

    /// Create a new per-environment socket. Returns the socket path for mounting into the container.
    /// The `spawn_accept_loop` closure is called with the listener and env_id to spawn the accept task.
    pub async fn add(
        &mut self,
        id: EnvironmentId,
        state_dir: &DaemonHostPath,
        spawn_accept_loop: impl FnOnce(UnixListener, EnvironmentId) -> JoinHandle<()>,
    ) -> Result<DaemonHostPath, String> {
        let socket_path = DaemonHostPath::new(state_dir.as_path().join(format!("env-{}.sock", id)));

        // Remove stale socket file if it exists
        let _ = tokio::fs::remove_file(socket_path.as_path()).await;

        let listener = UnixListener::bind(socket_path.as_path())
            .map_err(|e| format!("failed to bind environment socket: {e}"))?;

        info!(%id, path = %socket_path, "environment socket listening");

        let handle = spawn_accept_loop(listener, id.clone());
        self.sockets.insert(id, (handle, socket_path.clone()));

        Ok(socket_path)
    }

    /// Remove an environment socket and clean up.
    pub async fn remove(&mut self, id: &EnvironmentId) -> Result<(), String> {
        if let Some((handle, path)) = self.sockets.remove(id) {
            handle.abort();
            let _ = tokio::fs::remove_file(path.as_path()).await;
            info!(%id, "environment socket removed");
        }
        Ok(())
    }

    /// Remove all environment sockets.
    pub async fn remove_all(&mut self) {
        for (id, (handle, path)) in self.sockets.drain() {
            handle.abort();
            let _ = tokio::fs::remove_file(path.as_path()).await;
            info!(%id, "environment socket removed");
        }
    }
}
