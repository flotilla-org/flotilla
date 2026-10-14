use std::collections::HashMap;

use flotilla_protocol::{DaemonHostPath, EnvironmentId};
use sha2::{Digest, Sha256};
use tokio::{net::UnixListener, task::JoinHandle};
use tracing::info;

pub struct EnvironmentSocketRegistry {
    sockets: HashMap<EnvironmentId, (JoinHandle<()>, DaemonHostPath)>,
}

impl Drop for EnvironmentSocketRegistry {
    fn drop(&mut self) {
        for (_id, (handle, path)) in self.sockets.drain() {
            handle.abort();
            let _ = std::fs::remove_file(path.as_path());
            // Keep the directory inode across daemon restarts: running
            // containers bind this directory and must see the replacement socket.
        }
    }
}

impl Default for EnvironmentSocketRegistry {
    fn default() -> Self {
        Self::new()
    }
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
        let digest = Sha256::digest(id.as_str().as_bytes());
        let directory_name = digest[..16].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let directory = state_dir.as_path().join(directory_name);
        tokio::fs::create_dir_all(&directory).await.map_err(|error| error.to_string())?;
        let socket_path = DaemonHostPath::new(directory.join("flotilla.sock"));

        if let Some((_, path)) = self.sockets.get(&id) {
            return Ok(path.clone());
        }
        // Remove stale socket file if it exists
        let _ = tokio::fs::remove_file(socket_path.as_path()).await;

        let listener = UnixListener::bind(socket_path.as_path()).map_err(|e| format!("failed to bind environment socket: {e}"))?;

        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(socket_path.as_path(), std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|error| format!("protect environment socket: {error}"))?;
        info!(%id, path = %socket_path, "environment socket listening");

        let handle = spawn_accept_loop(listener, id.clone());
        self.sockets.insert(id, (handle, socket_path.clone()));

        Ok(socket_path)
    }

    /// Remove an environment socket and clean up.
    pub async fn remove(&mut self, id: &EnvironmentId) -> Result<(), String> {
        if let Some((handle, path)) = self.sockets.get(id) {
            handle.abort();
            match tokio::fs::remove_file(path.as_path()).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("remove environment socket {path}: {error}")),
            }
            if let Some(parent) = path.as_path().parent() {
                if let Err(error) = tokio::fs::remove_dir(parent).await {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(path = %parent.display(), %error, "environment socket directory not empty at teardown");
                    }
                }
            }
            self.sockets.remove(id);
            info!(%id, "environment socket removed");
        }
        Ok(())
    }

    /// Remove all environment sockets.
    pub async fn remove_all(&mut self) {
        for (id, (handle, path)) in self.sockets.drain() {
            handle.abort();
            let _ = tokio::fs::remove_file(path.as_path()).await;
            if let Some(parent) = path.as_path().parent() {
                let _ = tokio::fs::remove_dir(parent).await;
            }
            info!(%id, "environment socket removed");
        }
    }
}

/// Host-owned provisioning seam. The server supplies the accept loop; the
/// controller owns admission and retirement, without knowing the transport.
#[derive(Clone)]
pub struct EnvironmentSocketService {
    registry: std::sync::Arc<tokio::sync::Mutex<EnvironmentSocketRegistry>>,
    directory: DaemonHostPath,
    accept: std::sync::Arc<dyn Fn(UnixListener, EnvironmentId) -> JoinHandle<()> + Send + Sync>,
}

impl std::fmt::Debug for EnvironmentSocketService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentSocketService").field("directory", &self.directory).finish_non_exhaustive()
    }
}

impl EnvironmentSocketService {
    pub(crate) fn new(
        registry: std::sync::Arc<tokio::sync::Mutex<EnvironmentSocketRegistry>>,
        directory: DaemonHostPath,
        accept: impl Fn(UnixListener, EnvironmentId) -> JoinHandle<()> + Send + Sync + 'static,
    ) -> Self {
        Self { registry, directory, accept: std::sync::Arc::new(accept) }
    }

    pub(crate) async fn prepare(&self, id: EnvironmentId) -> Result<DaemonHostPath, String> {
        tokio::fs::create_dir_all(self.directory.as_path()).await.map_err(|error| error.to_string())?;
        let accept = self.accept.clone();
        self.registry.lock().await.add(id, &self.directory, move |listener, id| accept(listener, id)).await
    }

    pub(crate) async fn remove(&self, id: &EnvironmentId) -> Result<(), String> {
        self.registry.lock().await.remove(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3270: provisioning is idempotent, restart preserves the mounted directory,
    // and teardown retires the accept loop and removes the socket.
    #[tokio::test]
    async fn socket_lifecycle_preserves_mount_inode_until_teardown() {
        use std::os::unix::fs::MetadataExt;
        let directory = flotilla_test_support::TestSocketDir::new();
        let root = DaemonHostPath::new(directory.socket_path("env"));
        let id = EnvironmentId::new("env-convoy-0123456789abcdef0123456789abcdef-work");
        let accept = |listener, _| {
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await
            })
        };
        let mut registry = EnvironmentSocketRegistry::new();
        let path = registry.add(id.clone(), &root, accept).await.expect("provision socket");
        let parent = path.as_path().parent().expect("socket directory");
        let inode = std::fs::metadata(parent).expect("directory").ino();
        assert_eq!(std::fs::read_dir(parent).expect("mount contents").count(), 1);
        assert_eq!(registry.add(id.clone(), &root, |_, _| panic!("duplicate accept loop")).await.expect("idempotent"), path);
        drop(registry);
        assert!(!path.as_path().exists());
        assert_eq!(std::fs::metadata(parent).expect("restart directory").ino(), inode);
        let mut restarted = EnvironmentSocketRegistry::new();
        assert_eq!(restarted.add(id.clone(), &root, accept).await.expect("restart socket"), path);
        assert_eq!(std::fs::metadata(parent).expect("mounted directory").ino(), inode);
        restarted.remove(&id).await.expect("teardown");
        assert!(!path.as_path().exists());
        assert!(!parent.exists());
        restarted.remove(&id).await.expect("duplicate teardown");
    }
}
