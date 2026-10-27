use std::{collections::HashMap, os::unix::fs::PermissionsExt};

use flotilla_protocol::{DaemonHostPath, EnvironmentId};
use sha2::{Digest, Sha256};
use tokio::{net::UnixListener, task::JoinHandle};
use tracing::info;

pub(crate) fn directory_name(id: &EnvironmentId) -> String {
    let digest = Sha256::digest(id.as_str().as_bytes());
    digest[..16].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One socket's identity: an environment's own, or one of its vessels' on a
/// host whose vessels share the environment (#3792). A vessel agent claims its
/// host endpoint exclusively, so each vessel there needs a socket of its own;
/// every one serves its environment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SocketKey {
    environment: EnvironmentId,
    vessel: Option<String>,
}

impl SocketKey {
    fn directory_name(&self) -> String {
        match &self.vessel {
            None => directory_name(&self.environment),
            Some(vessel) => {
                let mut digest = Sha256::new();
                for part in [self.environment.as_str(), vessel] {
                    digest.update((part.len() as u64).to_le_bytes());
                    digest.update(part.as_bytes());
                }
                digest.finalize()[..16].iter().map(|byte| format!("{byte:02x}")).collect()
            }
        }
    }
}

// Darwin's 104-byte sun_path includes the trailing NUL. Use the same bound
// everywhere so a Linux-tested path is also safe on Darwin.
const MAX_SOCKET_PATH_BYTES: usize = 103;

fn check_socket_path(path: &std::path::Path) -> Result<(), String> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > MAX_SOCKET_PATH_BYTES {
        return Err(format!(
            "environment socket path {} is {length} bytes; SUN_LEN permits at most {MAX_SOCKET_PATH_BYTES} pathname bytes (excluding NUL)",
            path.display()
        ));
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    unsafe { libc::geteuid() }
}

fn alias_path(state_dir: &std::path::Path, uid: u32) -> std::path::PathBuf {
    let digest = Sha256::digest(state_dir.as_os_str().as_encoded_bytes());
    let name: String = digest[..16].iter().map(|byte| format!("{byte:02x}")).collect();
    // Deliberately ignore TMPDIR: it can be as long as the state directory.
    // This fixed short prefix makes the pathname bound independent of both.
    std::path::Path::new("/tmp").join(format!("flt-e-{uid}-{name}"))
}

/// A short spelling of the existing state directory, not a relocated mount.
/// Keeping the actual directories in place preserves bind mounts made before
/// this change as well as across later daemon restarts. Never canonicalize the
/// returned alias before bind: the kernel limits the supplied pathname.
async fn socket_root(state_dir: &DaemonHostPath) -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::MetadataExt;

    tokio::fs::create_dir_all(state_dir.as_path())
        .await
        .map_err(|error| format!("create environment socket state directory {state_dir}: {error}"))?;
    let target = tokio::fs::canonicalize(state_dir.as_path())
        .await
        .map_err(|error| format!("canonicalize environment socket state directory {state_dir}: {error}"))?;
    let uid = effective_uid();
    let alias = alias_path(&target, uid);
    match tokio::fs::symlink(&target, &alias).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create environment socket alias {}: {error}", alias.display())),
    }
    let metadata = tokio::fs::symlink_metadata(&alias)
        .await
        .map_err(|error| format!("inspect environment socket alias {}: {error}", alias.display()))?;
    if !metadata.file_type().is_symlink() || metadata.uid() != uid || tokio::fs::read_link(&alias).await.ok().as_ref() != Some(&target) {
        return Err(format!(
            "refuse environment socket alias {}: expected a symlink owned by uid {uid} to {}",
            alias.display(),
            target.display()
        ));
    }
    // Retain this tiny alias across shutdown; other environments and running
    // containers may still use it. /tmp cleanup can remove it while the daemon
    // is stopped: the next preparation recreates it without replacing a mount.
    Ok(alias)
}

pub struct EnvironmentSocketRegistry {
    sockets: HashMap<SocketKey, (Option<JoinHandle<()>>, DaemonHostPath)>,
}

impl Drop for EnvironmentSocketRegistry {
    fn drop(&mut self) {
        for (_id, (handle, path)) in self.sockets.drain() {
            if let Some(handle) = handle {
                handle.abort();
            }
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
        self.add_keyed(SocketKey { environment: id, vessel: None }, state_dir, spawn_accept_loop).await
    }

    async fn add_keyed(
        &mut self,
        key: SocketKey,
        state_dir: &DaemonHostPath,
        spawn_accept_loop: impl FnOnce(UnixListener, EnvironmentId) -> JoinHandle<()>,
    ) -> Result<DaemonHostPath, String> {
        if let Some((Some(handle), path)) = self.sockets.get(&key) {
            if !handle.is_finished() {
                return Ok(path.clone());
            }
        }
        let directory_name = key.directory_name();
        let directory = socket_root(state_dir).await?.join(directory_name);
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .await
            .map_err(|error| format!("create environment socket directory {}: {error}", directory.display()))?;
        tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|error| format!("protect environment socket directory {}: {error}", directory.display()))?;
        let socket_path = DaemonHostPath::new(directory.join("flotilla.sock"));
        check_socket_path(socket_path.as_path())?;

        // Remove stale socket file if it exists
        let _ = tokio::fs::remove_file(socket_path.as_path()).await;

        let listener = UnixListener::bind(socket_path.as_path()).map_err(|e| format!("failed to bind environment socket: {e}"))?;

        tokio::fs::set_permissions(socket_path.as_path(), std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|error| format!("protect environment socket: {error}"))?;
        info!(id = %key.environment, vessel = ?key.vessel, path = %socket_path, "environment socket listening");

        let handle = spawn_accept_loop(listener, key.environment.clone());
        self.sockets.insert(key, (Some(handle), socket_path.clone()));

        Ok(socket_path)
    }

    /// Remove an environment socket, and its vessels' sockets, and clean up.
    pub async fn remove(&mut self, id: &EnvironmentId) -> Result<(), String> {
        let vessels = self.sockets.keys().filter(|key| key.environment == *id && key.vessel.is_some()).cloned().collect::<Vec<_>>();
        for key in vessels {
            self.remove_keyed(&key).await?;
        }
        self.remove_keyed(&SocketKey { environment: id.clone(), vessel: None }).await
    }

    async fn remove_keyed(&mut self, key: &SocketKey) -> Result<(), String> {
        let id = &key.environment;
        if let Some((handle, path)) = self.sockets.get_mut(key) {
            // A failed unlink retains the path for retry, but never a serving
            // registration: add must rebind rather than return an aborted loop.
            if let Some(handle) = handle.take() {
                handle.abort();
            }
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
            self.sockets.remove(key);
            info!(%id, vessel = ?key.vessel, "environment socket removed");
        }
        Ok(())
    }

    /// Best-effort bulk cleanup for shutdown: abort every accept task even if
    /// unlink/rmdir fails. Resource teardown uses `remove` to report and retry
    /// cleanup failures rather than treating them as successful deletion.
    pub async fn remove_all(&mut self) {
        for (key, (handle, path)) in self.sockets.drain() {
            if let Some(handle) = handle {
                handle.abort();
            }
            let _ = tokio::fs::remove_file(path.as_path()).await;
            if let Some(parent) = path.as_path().parent() {
                let _ = tokio::fs::remove_dir(parent).await;
            }
            info!(id = %key.environment, vessel = ?key.vessel, "environment socket removed");
        }
    }
}

/// How crews reach an environment socket, which decides what the kernel peer proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketReach {
    /// Bind-mounted into a container: the kernel peer is the crew's own process.
    Mounted,
    /// Carried by a vessel agent's guest forward on an SSH host (#3792):
    /// the kernel peer is the controller's own agent client, so its
    /// environment is no crew evidence.
    Forwarded,
}

/// Host-owned provisioning seam. The server supplies the accept loop; the
/// controller owns admission and retirement, without knowing the transport.
#[derive(Clone)]
pub struct EnvironmentSocketService {
    registry: std::sync::Arc<tokio::sync::Mutex<EnvironmentSocketRegistry>>,
    directory: DaemonHostPath,
    accept: std::sync::Arc<dyn Fn(UnixListener, EnvironmentId, SocketReach) -> JoinHandle<()> + Send + Sync>,
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
        accept: impl Fn(UnixListener, EnvironmentId, SocketReach) -> JoinHandle<()> + Send + Sync + 'static,
    ) -> Self {
        Self { registry, directory, accept: std::sync::Arc::new(accept) }
    }

    pub(crate) async fn prepare(&self, id: EnvironmentId) -> Result<DaemonHostPath, String> {
        self.prepare_reached(SocketKey { environment: id, vessel: None }, SocketReach::Mounted).await
    }

    /// The socket a vessel's agent forwards its crews to, where the vessel is
    /// the whole environment (#3981).
    pub(crate) async fn prepare_forwarded(&self, id: EnvironmentId) -> Result<DaemonHostPath, String> {
        self.prepare_reached(SocketKey { environment: id, vessel: None }, SocketReach::Forwarded).await
    }

    /// The socket one vessel's agent forwards its crews to, on an SSH
    /// host where vessels share their environment (#3792). It serves that
    /// environment.
    pub(crate) async fn prepare_vessel_forwarded(&self, id: EnvironmentId, vessel: &str) -> Result<DaemonHostPath, String> {
        self.prepare_reached(SocketKey { environment: id, vessel: Some(vessel.to_string()) }, SocketReach::Forwarded).await
    }

    async fn prepare_reached(&self, key: SocketKey, reach: SocketReach) -> Result<DaemonHostPath, String> {
        tokio::fs::create_dir_all(self.directory.as_path()).await.map_err(|error| error.to_string())?;
        let accept = &self.accept;
        self.registry.lock().await.add_keyed(key, &self.directory, |listener, id| accept(listener, id, reach)).await
    }

    pub(crate) async fn remove(&self, id: &EnvironmentId) -> Result<(), String> {
        self.registry.lock().await.remove(id).await
    }

    pub(crate) async fn remove_vessel(&self, id: &EnvironmentId, vessel: &str) -> Result<(), String> {
        self.registry.lock().await.remove_keyed(&SocketKey { environment: id.clone(), vessel: Some(vessel.to_string()) }).await
    }
}

/// Production aliases survive daemon shutdown. Tests own unique state roots
/// and retire their aliases even when an assertion unwinds.
#[cfg(test)]
pub(crate) struct SocketAliasGuard(pub(crate) std::path::PathBuf);

#[cfg(test)]
impl SocketAliasGuard {
    pub(crate) fn new(state_dir: &DaemonHostPath) -> Self {
        std::fs::create_dir_all(state_dir.as_path()).expect("test state directory");
        let target = std::fs::canonicalize(state_dir.as_path()).expect("canonical test state directory");
        Self(alias_path(&target, effective_uid()))
    }
}

#[cfg(test)]
impl Drop for SocketAliasGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3285: the length guard counts bytes, accepting the last safe pathname
    // and refusing the next byte with the actual path and limit in the error.
    #[hegel::test]
    fn socket_path_length_contract(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Empty through oversized paths, including multibyte names whose
        // character count fits while their byte count exceeds SUN_LEN.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(120));
        let multibyte = tc.draw(gs::booleans());
        for count in [count, 103, 104] {
            let name = if multibyte { "é" } else { "x" }.repeat(count);
            let path = std::path::Path::new(&name);
            let bytes = name.len();
            let result = check_socket_path(path);
            assert_eq!(result.is_ok(), bytes <= 103);
            if let Err(error) = result {
                assert!(error.contains(&name));
                assert!(error.contains("103"));
                assert!(error.contains(&format!("{bytes} bytes")));
            }
        }
    }

    // #3285: aliases stay bounded even for maximum-width UIDs and arbitrary
    // state roots. State namespaces must not collide for the same environment.
    #[hegel::test]
    fn alias_paths_are_bounded_and_namespaced(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(512));
        let uid = tc.draw(gs::integers::<u32>());
        let state = std::path::PathBuf::from("/state").join("é".repeat(count));
        for uid in [uid, u32::MAX] {
            let alias = alias_path(&state, uid);
            let socket = alias.join(directory_name(&EnvironmentId::new("example"))).join("flotilla.sock");
            assert!(socket.as_os_str().as_encoded_bytes().len() <= 103);
            assert_eq!(alias, alias_path(&state, uid));
            assert_ne!(alias, alias_path(&state.join("other"), uid));
        }
    }

    // Refuse an unexpected alias without replacing it or modifying its target.
    #[tokio::test]
    async fn socket_alias_refuses_unexpected_target() {
        let state = tempfile::tempdir().expect("state");
        let other = tempfile::tempdir().expect("other state");
        let cleanup = SocketAliasGuard::new(&DaemonHostPath::new(state.path()));
        let alias = &cleanup.0;
        std::os::unix::fs::symlink(other.path(), alias).expect("unexpected alias");
        let error = socket_root(&DaemonHostPath::new(state.path())).await.expect_err("refuse wrong target");
        assert!(error.contains("refuse environment socket alias"));
        assert!(error.contains(&alias.display().to_string()));
        assert_eq!(std::fs::read_link(alias).expect("alias retained"), other.path());
    }

    // #3285: state paths beyond Linux's 108-byte limit must still bind.
    #[tokio::test]
    async fn long_state_directory_still_binds() {
        let directory = tempfile::tempdir().expect("state root");
        let root = DaemonHostPath::new(directory.path().join("x".repeat(120)));
        let _cleanup = SocketAliasGuard::new(&root);
        let mut registry = EnvironmentSocketRegistry::new();
        let id = EnvironmentId::new("long-state");
        let path = registry
            .add(id.clone(), &root, |listener, _| {
                tokio::spawn(async move {
                    let _listener = listener;
                    std::future::pending::<()>().await
                })
            })
            .await
            .expect("long state directory must bind");
        assert!(path.as_path().as_os_str().as_encoded_bytes().len() <= 103);
        tokio::net::UnixStream::connect(path.as_path()).await.expect("connect");
        registry.remove(&id).await.expect("cleanup");
    }

    // #3270: provisioning is idempotent, restart preserves the mounted directory,
    // and teardown retires the accept loop and removes the socket.
    #[tokio::test]
    async fn socket_lifecycle_preserves_mount_inode_until_teardown() {
        use std::os::unix::fs::MetadataExt;
        let directory = flotilla_test_support::TestSocketDir::new();
        let root = DaemonHostPath::new(directory.socket_path("env"));
        let _cleanup = SocketAliasGuard::new(&root);
        let id = EnvironmentId::new("env-convoy-0123456789abcdef0123456789abcdef-work");
        let accept = |listener, _| {
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await
            })
        };
        // Simulate the directory already mounted by a vessel before this roll.
        let legacy_parent = root.as_path().join(directory_name(&id));
        std::fs::create_dir_all(&legacy_parent).expect("existing mount");
        let legacy_inode = std::fs::metadata(&legacy_parent).expect("existing inode").ino();
        let mut registry = EnvironmentSocketRegistry::new();
        let path = registry.add(id.clone(), &root, accept).await.expect("provision socket");
        let parent = path.as_path().parent().expect("socket directory");
        let inode = std::fs::metadata(parent).expect("directory").ino();
        assert_eq!(legacy_inode, inode, "vessels running across the roll retain their mount");
        assert_eq!(std::fs::read_dir(parent).expect("mount contents").count(), 1);
        assert_eq!(registry.add(id.clone(), &root, |_, _| panic!("duplicate accept loop")).await.expect("idempotent"), path);
        drop(registry);
        assert!(!path.as_path().exists());
        assert_eq!(std::fs::metadata(parent).expect("restart directory").ino(), inode);
        std::fs::remove_file(parent.parent().expect("short alias")).expect("simulate tmp cleanup");
        let mut restarted = EnvironmentSocketRegistry::new();
        assert_eq!(restarted.add(id.clone(), &root, accept).await.expect("restart socket"), path);
        assert_eq!(std::fs::metadata(parent).expect("mounted directory").ino(), inode);
        restarted.remove(&id).await.expect("teardown");
        assert!(!path.as_path().exists());
        assert!(!parent.exists());
        restarted.remove(&id).await.expect("duplicate teardown");
    }
    // Unlink failure cannot leave a dead listener masquerading as an idempotent
    // registration. Once the filesystem obstruction is removed, add must rebind.
    #[tokio::test]
    async fn failed_socket_removal_can_rebind_and_retry() {
        let directory = flotilla_test_support::TestSocketDir::new();
        let root = DaemonHostPath::new(directory.socket_path("env"));
        let _cleanup = SocketAliasGuard::new(&root);
        let id = EnvironmentId::new("retry");
        let accept = |listener, _| {
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await
            })
        };
        let mut registry = EnvironmentSocketRegistry::new();
        let path = registry.add(id.clone(), &root, accept).await.expect("first listener");
        let parent = path.as_path().parent().expect("socket directory");
        assert_eq!(std::fs::metadata(parent).expect("private directory").permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(path.as_path()).expect("private socket").permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(path.as_path()).expect("replace socket with obstruction");
        std::fs::create_dir(path.as_path()).expect("obstruction");
        assert!(registry.remove(&id).await.is_err());
        assert!(registry.add(id.clone(), &root, accept).await.is_err(), "obstructed path must never be returned as live");
        std::fs::remove_dir(path.as_path()).expect("remove obstruction");
        let path = registry.add(id.clone(), &root, accept).await.expect("replacement listener");
        tokio::net::UnixStream::connect(path.as_path()).await.expect("replacement socket accepts connections");
        registry.remove(&id).await.expect("retry teardown");
        assert!(!path.as_path().exists());
    }
}
