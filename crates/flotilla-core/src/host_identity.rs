//! Stable host identity with machine-id scoping.
//!
//! Pre-wired for the node identity spec (2026-03-28). Will be called from
//! daemon startup to resolve the local machine's `HostId` once `NodeId`
//! and the full identity lifecycle are implemented.

use std::{
    fs,
    path::{Path, PathBuf},
};

use ed25519_dalek::{SigningKey, VerifyingKey};
use flotilla_protocol::{EnvironmentId, NodeId, qualified_path::HostId};
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use tracing::warn;
use uuid::Uuid;

use crate::providers::{ChannelLabel, CommandRunner, discovery::EnvVars};

/// Resolve a machine-scoped state directory under `base_state_dir`.
///
/// Resolution order for the machine identifier:
/// 1. `config_machine_id` parameter (from daemon.toml)
/// 2. `/etc/machine-id` file (Linux)
/// 3. `IOPlatformUUID` via `ioreg` (macOS, using injected `runner`)
/// 4. Error
pub async fn machine_scoped_state_dir(
    base_state_dir: &Path,
    config_machine_id: Option<&str>,
    runner: &dyn CommandRunner,
) -> Result<PathBuf, String> {
    let machine_id = if let Some(id) = config_machine_id {
        id.to_owned()
    } else if let Some(id) = read_etc_machine_id() {
        id
    } else if let Some(id) = read_macos_platform_uuid(runner).await {
        id
    } else {
        return Err("Cannot determine machine identity. Set `machine_id` in daemon.toml.".to_owned());
    };

    Ok(base_state_dir.join(machine_id))
}

/// Read and return the trimmed contents of `/etc/machine-id`, if present.
fn read_etc_machine_id() -> Option<String> {
    let content = fs::read_to_string("/etc/machine-id").ok()?;
    let trimmed = content.trim().to_owned();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// Query macOS `IOPlatformUUID` via the injected `CommandRunner`.
async fn read_macos_platform_uuid(runner: &dyn CommandRunner) -> Option<String> {
    let output = runner.run("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"], Path::new("/"), &ChannelLabel::Noop).await.ok()?;

    for line in output.lines() {
        if line.contains("IOPlatformUUID") {
            if let Some(uuid_start) = line.rfind('"') {
                let before_last = &line[..uuid_start];
                if let Some(uuid_begin) = before_last.rfind('"') {
                    let uuid = &line[uuid_begin + 1..uuid_start];
                    if !uuid.is_empty() {
                        return Some(uuid.to_owned());
                    }
                }
            }
        }
    }
    None
}

fn machine_scoped_state_dir_or_base(base_state_dir: &Path, resolved: Result<PathBuf, String>) -> PathBuf {
    match resolved {
        Ok(path) => path,
        Err(err) => {
            warn!(base_state_dir = %base_state_dir.display(), err = %err, "machine-scoped state dir unavailable, using base state dir");
            base_state_dir.to_path_buf()
        }
    }
}

/// Resolve the state directory to use for local direct-environment identity.
///
/// This prefers a machine-scoped subdirectory, but falls back to the provided
/// base state directory if machine identity probing fails.
pub async fn resolve_local_environment_state_dir(
    base_state_dir: &Path,
    config_machine_id: Option<&str>,
    runner: &dyn CommandRunner,
) -> PathBuf {
    let resolved = machine_scoped_state_dir(base_state_dir, config_machine_id, runner).await;
    machine_scoped_state_dir_or_base(base_state_dir, resolved)
}

/// Resolve or create a persisted local host id in the machine-scoped local
/// state directory.
pub async fn resolve_local_host_id(
    base_state_dir: &Path,
    config_machine_id: Option<&str>,
    runner: &dyn CommandRunner,
) -> Result<HostId, String> {
    let state_dir = resolve_local_environment_state_dir(base_state_dir, config_machine_id, runner).await;
    resolve_or_create_host_id(&state_dir)
}

/// Resolve or create a persisted local mesh node id in the machine-scoped
/// local identity directory.
pub async fn resolve_local_node_id(
    base_config_dir: &Path,
    config_machine_id: Option<&str>,
    runner: &dyn CommandRunner,
) -> Result<NodeId, String> {
    let identity_dir = machine_scoped_state_dir(&base_config_dir.join("identity"), config_machine_id, runner).await?;
    resolve_or_create_node_id(&identity_dir)
}

/// Resolve an existing `HostId` from `<state_dir>/host-id`, or generate and
/// persist a new one atomically.
///
/// Atomic write strategy: write to a temp file, `hard_link` to the canonical
/// path, then remove the temp. If the link fails (another process won), read
/// the canonical file instead.
pub fn resolve_or_create_host_id(state_dir: &Path) -> Result<HostId, String> {
    let target = state_dir.join("host-id");

    // Fast path: file already exists.
    if let Ok(content) = fs::read_to_string(&target) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Ok(HostId::new(trimmed));
        }
    }

    fs::create_dir_all(state_dir).map_err(|e| format!("failed to create state dir {}: {e}", state_dir.display()))?;
    let new_id = Uuid::new_v4().to_string();

    for _ in 0..8 {
        let temp = state_dir.join(format!(".host-id.{}", Uuid::new_v4()));
        match fs::write(&temp, format!("{new_id}\n")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to write temp host-id: {e}")),
        }

        let link_result = fs::hard_link(&temp, &target);
        let _ = fs::remove_file(&temp);

        match link_result {
            Ok(()) => return Ok(HostId::new(new_id)),
            Err(_) if target.exists() => {
                let content = fs::read_to_string(&target).map_err(|e| format!("failed to read host-id after link race: {e}"))?;
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    return Err("host-id file exists but is empty".to_owned());
                }
                return Ok(HostId::new(trimmed));
            }
            Err(_) => continue,
        }
    }

    let content = fs::read_to_string(&target).map_err(|e| format!("failed to read host-id after retries: {e}"))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("host-id file exists but is empty".to_owned());
    }
    Ok(HostId::new(trimmed))
}

fn node_id_from_public_key(public_key: &VerifyingKey) -> NodeId {
    let fingerprint = Sha256::digest(public_key.to_bytes());
    let mut truncated = String::with_capacity(32);
    for byte in &fingerprint[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut truncated, "{byte:02x}");
    }
    NodeId::new(truncated)
}

fn read_signing_key(path: &Path) -> Result<SigningKey, String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let secret: [u8; 32] =
        bytes.try_into().map_err(|_| format!("invalid node private key length in {}: expected 32 bytes", path.display()))?;
    Ok(SigningKey::from_bytes(&secret))
}

fn read_verifying_key(path: &Path) -> Result<VerifyingKey, String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let verifying_array: [u8; 32] =
        bytes.try_into().map_err(|_| format!("invalid node public key length in {}: expected 32 bytes", path.display()))?;
    VerifyingKey::from_bytes(&verifying_array).map_err(|e| format!("invalid node public key in {}: {e}", path.display()))
}

fn persist_public_key(path: &Path, verifying: &VerifyingKey) -> Result<(), String> {
    if let Ok(existing) = read_verifying_key(path) {
        if existing != *verifying {
            return Err(format!("node keypair mismatch in {}", path.parent().unwrap_or(path).display()));
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create state dir {}: {e}", parent.display()))?;
    }

    for _ in 0..8 {
        let temp = path.parent().unwrap_or_else(|| Path::new(".")).join(format!(".node.pub.{}", Uuid::new_v4()));
        match fs::write(&temp, verifying.to_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to write temp node.pub: {e}")),
        }

        let link_result = fs::hard_link(&temp, path);
        let _ = fs::remove_file(&temp);

        match link_result {
            Ok(()) => return Ok(()),
            Err(_) if path.exists() => {
                let existing = read_verifying_key(path)?;
                if existing == *verifying {
                    return Ok(());
                }
                return Err(format!("node keypair mismatch in {}", path.parent().unwrap_or(path).display()));
            }
            Err(_) => continue,
        }
    }

    let existing = read_verifying_key(path)?;
    if existing != *verifying {
        return Err(format!("node keypair mismatch in {}", path.parent().unwrap_or(path).display()));
    }
    Ok(())
}

fn write_new_node_keypair(state_dir: &Path) -> Result<(SigningKey, VerifyingKey), String> {
    fs::create_dir_all(state_dir).map_err(|e| format!("failed to create state dir {}: {e}", state_dir.display()))?;

    let signing = SigningKey::generate(&mut OsRng);
    let verifying = signing.verifying_key();
    let key_path = state_dir.join("node.key");
    let pub_path = state_dir.join("node.pub");

    for _ in 0..8 {
        let key_temp = state_dir.join(format!(".node.key.{}", Uuid::new_v4()));
        let pub_temp = state_dir.join(format!(".node.pub.{}", Uuid::new_v4()));

        match fs::write(&key_temp, signing.to_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to write temp node.key: {e}")),
        }
        if let Err(err) = set_restrictive_permissions(&key_temp) {
            let _ = fs::remove_file(&key_temp);
            return Err(err);
        }

        match fs::write(&pub_temp, verifying.to_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&key_temp);
                continue;
            }
            Err(e) => {
                let _ = fs::remove_file(&key_temp);
                return Err(format!("failed to write temp node.pub: {e}"));
            }
        }

        let key_link = fs::hard_link(&key_temp, &key_path);
        let pub_link = fs::hard_link(&pub_temp, &pub_path);
        let _ = fs::remove_file(&key_temp);
        let _ = fs::remove_file(&pub_temp);

        if key_link.is_ok() && pub_link.is_ok() {
            return Ok((signing, verifying));
        }

        if key_path.exists() && pub_path.exists() {
            set_restrictive_permissions(&key_path)?;
            let signing = read_signing_key(&key_path)?;
            let verifying_bytes = fs::read(&pub_path).map_err(|e| format!("failed to read {}: {e}", pub_path.display()))?;
            let verifying_array: [u8; 32] = verifying_bytes
                .try_into()
                .map_err(|_| format!("invalid node public key length in {}: expected 32 bytes", pub_path.display()))?;
            let verifying = VerifyingKey::from_bytes(&verifying_array)
                .map_err(|e| format!("invalid node public key in {}: {e}", pub_path.display()))?;
            return Ok((signing, verifying));
        }
    }

    let signing = read_signing_key(&state_dir.join("node.key"))?;
    let key_path = state_dir.join("node.key");
    set_restrictive_permissions(&key_path)?;
    let verifying = signing.verifying_key();
    persist_public_key(&state_dir.join("node.pub"), &verifying)?;
    Ok((signing, verifying))
}

fn set_restrictive_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|e| format!("failed to set permissions on {}: {e}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

/// Resolve or create a persisted mesh node id in `<state_dir>/node.{key,pub}`.
pub fn resolve_or_create_node_id(state_dir: &Path) -> Result<NodeId, String> {
    let key_path = state_dir.join("node.key");
    let pub_path = state_dir.join("node.pub");

    if key_path.exists() {
        let signing = read_signing_key(&key_path)?;
        let verifying = signing.verifying_key();
        set_restrictive_permissions(&key_path)?;
        if pub_path.exists() {
            let persisted = read_verifying_key(&pub_path)?;
            if persisted != verifying {
                return Err(format!("node keypair mismatch in {}", state_dir.display()));
            }
            return Ok(node_id_from_public_key(&persisted));
        }
        persist_public_key(&pub_path, &verifying)?;
        return Ok(node_id_from_public_key(&verifying));
    }

    if pub_path.exists() {
        return Err(format!("node private key missing for existing public key in {}", state_dir.display()));
    }

    let (_signing, verifying) = write_new_node_keypair(state_dir)?;
    Ok(node_id_from_public_key(&verifying))
}

/// Resolve an existing direct-environment id from `<state_dir>/environment-id`,
/// or generate and persist a new one atomically.
pub fn resolve_or_create_environment_id(state_dir: &Path) -> Result<EnvironmentId, String> {
    let target = state_dir.join("environment-id");

    // Fast path: file already exists.
    if let Ok(content) = fs::read_to_string(&target) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Ok(EnvironmentId::new(trimmed));
        }
    }

    fs::create_dir_all(state_dir).map_err(|e| format!("failed to create state dir {}: {e}", state_dir.display()))?;
    let new_id = Uuid::new_v4().to_string();

    for _ in 0..8 {
        let temp = state_dir.join(format!(".environment-id.{}", Uuid::new_v4()));
        match fs::write(&temp, format!("{new_id}\n")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to write temp environment-id: {e}")),
        }

        let link_result = fs::hard_link(&temp, &target);
        let _ = fs::remove_file(&temp);

        match link_result {
            Ok(()) => return Ok(EnvironmentId::new(new_id)),
            Err(_) if target.exists() => {
                let content = fs::read_to_string(&target).map_err(|e| format!("failed to read environment-id after link race: {e}"))?;
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    return Err("environment-id file exists but is empty".to_owned());
                }
                return Ok(EnvironmentId::new(trimmed));
            }
            Err(_) => continue,
        }
    }

    let content = fs::read_to_string(&target).map_err(|e| format!("failed to read environment-id after retries: {e}"))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("environment-id file exists but is empty".to_owned());
    }
    Ok(EnvironmentId::new(trimmed))
}

/// Resolve the remote state directory used for direct-environment identity.
///
/// Resolution order:
/// 1. `XDG_STATE_HOME/flotilla`
/// 2. `HOME/.local/state/flotilla`
/// 3. `None`
pub fn remote_environment_state_dir(env: &dyn EnvVars) -> Option<PathBuf> {
    if let Some(xdg_state_home) = env.get("XDG_STATE_HOME").filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(xdg_state_home).join("flotilla"));
    }

    env.get("HOME").filter(|value| !value.trim().is_empty()).map(|home| PathBuf::from(home).join(".local/state/flotilla"))
}

/// Resolve or create a persisted remote direct-environment id.
///
/// If the remote environment does not expose enough env vars to locate its
/// state directory, fall back to the caller-provided deterministic id. This
/// keeps static SSH registration working while preferring remote-persisted ids
/// whenever the remote environment can tell us where its state lives.
pub async fn resolve_or_create_remote_environment_id(
    runner: &dyn CommandRunner,
    env: &dyn EnvVars,
    fallback_id: EnvironmentId,
) -> Result<EnvironmentId, String> {
    let Some(state_dir) = remote_environment_state_dir(env) else {
        return Ok(fallback_id);
    };

    resolve_or_create_remote_environment_id_at(runner, &state_dir).await
}

/// Resolve or create a persisted remote host id.
///
/// Returns `Ok(None)` when the remote environment does not expose enough env
/// vars to locate a persistent state directory.
pub async fn resolve_or_create_remote_host_id(runner: &dyn CommandRunner, env: &dyn EnvVars) -> Result<Option<HostId>, String> {
    let Some(state_dir) = remote_environment_state_dir(env) else {
        return Ok(None);
    };

    resolve_or_create_remote_host_id_at(runner, &state_dir).await.map(Some)
}

async fn resolve_or_create_remote_host_id_at(runner: &dyn CommandRunner, state_dir: &Path) -> Result<HostId, String> {
    let target = state_dir.join("host-id");
    let target_str = target.to_string_lossy().into_owned();

    if let Ok(content) = runner.run("cat", &[&target_str], Path::new("/"), &ChannelLabel::Noop).await {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(format!("remote host-id file is empty: {}", target.display()));
        }
        return Ok(HostId::new(trimmed));
    }

    let content = runner
        .ensure_file(&target, &format!("{}\n", Uuid::new_v4()))
        .await
        .map_err(|err| format!("failed to ensure remote host-id at {}: {err}", target.display()))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(format!("remote host-id file is empty after ensure: {}", target.display()));
    }
    Ok(HostId::new(trimmed))
}

async fn resolve_or_create_remote_environment_id_at(runner: &dyn CommandRunner, state_dir: &Path) -> Result<EnvironmentId, String> {
    let target = state_dir.join("environment-id");
    let target_str = target.to_string_lossy().into_owned();

    if let Ok(content) = runner.run("cat", &[&target_str], Path::new("/"), &ChannelLabel::Noop).await {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(format!("remote environment-id file is empty: {}", target.display()));
        }
        return Ok(EnvironmentId::new(trimmed));
    }

    let content = runner
        .ensure_file(&target, &format!("{}\n", Uuid::new_v4()))
        .await
        .map_err(|err| format!("failed to ensure remote environment-id at {}: {err}", target.display()))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(format!("remote environment-id file is empty after ensure: {}", target.display()));
    }
    Ok(EnvironmentId::new(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProcessCommandRunner, discovery::test_support::TestEnvVars};

    #[test]
    fn generates_and_persists_host_id() {
        let dir = tempfile::tempdir().unwrap();
        let id1 = resolve_or_create_host_id(dir.path()).unwrap();
        let id2 = resolve_or_create_host_id(dir.path()).unwrap();
        assert_eq!(id1, id2, "should return same ID on second call");
        assert!(!id1.as_str().is_empty());
    }

    #[test]
    fn reads_existing_host_id() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("host-id");
        std::fs::write(&file, "my-custom-id\n").unwrap();
        let id = resolve_or_create_host_id(dir.path()).unwrap();
        assert_eq!(id.as_str(), "my-custom-id");
    }

    #[test]
    fn concurrent_host_id_calls_share_the_same_persisted_value() {
        use std::{
            sync::{Arc, Barrier, Mutex},
            thread,
        };

        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let results: Arc<Mutex<Vec<HostId>>> = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let results = Arc::clone(&results);
            let path = dir.path().to_path_buf();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let id = resolve_or_create_host_id(&path).expect("resolve host id");
                results.lock().unwrap().push(id);
            }));
        }

        for handle in handles {
            handle.join().expect("thread join");
        }

        let ids = results.lock().unwrap();
        assert_eq!(ids.len(), 8);
        let first = ids.first().expect("first id").clone();
        assert!(ids.iter().all(|id| *id == first), "all concurrent callers should observe the same id");
        let file = fs::read_to_string(dir.path().join("host-id")).expect("host-id file");
        assert_eq!(file.trim(), first.as_str());
    }

    #[tokio::test]
    async fn resolve_local_host_id_persists_in_local_state_dir() {
        let base = tempfile::tempdir().unwrap();
        let runner = crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build();

        let first = resolve_local_host_id(base.path(), None, &runner).await.expect("resolve local host id");
        let second = resolve_local_host_id(base.path(), None, &runner).await.expect("resolve local host id again");

        assert_eq!(first, second);
        assert!(!first.as_str().is_empty());
    }

    #[tokio::test]
    async fn resolve_or_create_remote_host_id_reads_persisted_remote_value() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("flotilla");
        let runner = crate::providers::discovery::test_support::DiscoveryMockRunner::builder()
            .with_file(state_dir.join("host-id"), "remote-host-id\n")
            .build();
        let env = TestEnvVars::new([("XDG_STATE_HOME", temp.path().to_string_lossy().into_owned())]);

        let resolved = resolve_or_create_remote_host_id(&runner, &env).await.expect("resolve remote host id");

        assert_eq!(resolved.as_ref().map(HostId::as_str), Some("remote-host-id"));
    }

    #[test]
    fn generates_and_persists_node_keypair_backed_id() {
        let dir = tempfile::tempdir().unwrap();
        let node_id_1 = resolve_or_create_node_id(dir.path()).unwrap();
        let node_id_2 = resolve_or_create_node_id(dir.path()).unwrap();

        assert_eq!(node_id_1, node_id_2);
        assert_eq!(node_id_1.as_str().len(), 32, "fingerprint should be 16 bytes / 32 hex chars");
        assert!(dir.path().join("node.key").exists());
        assert!(dir.path().join("node.pub").exists());
        assert!(!dir.path().join("node-id").exists(), "legacy node-id file should not be used");
    }

    #[test]
    fn node_id_is_derived_from_public_key_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let node_id = resolve_or_create_node_id(dir.path()).unwrap();
        let pub_bytes = fs::read(dir.path().join("node.pub")).unwrap();
        let pub_array: [u8; 32] = pub_bytes.try_into().unwrap();
        let public_key = VerifyingKey::from_bytes(&pub_array).unwrap();
        assert_eq!(node_id, node_id_from_public_key(&public_key));
    }

    #[test]
    fn resolve_or_create_node_id_recovers_missing_public_key_from_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let signing = SigningKey::generate(&mut OsRng);
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(dir.path().join("node.key"), signing.to_bytes()).unwrap();

        let node_id = resolve_or_create_node_id(dir.path()).unwrap();

        let pub_bytes = fs::read(dir.path().join("node.pub")).unwrap();
        let pub_array: [u8; 32] = pub_bytes.try_into().unwrap();
        let public_key = VerifyingKey::from_bytes(&pub_array).unwrap();
        assert_eq!(public_key, signing.verifying_key());
        assert_eq!(node_id, node_id_from_public_key(&public_key));
    }

    #[test]
    fn generates_and_persists_environment_id() {
        let dir = tempfile::tempdir().unwrap();
        let id1 = resolve_or_create_environment_id(dir.path()).unwrap();
        let id2 = resolve_or_create_environment_id(dir.path()).unwrap();
        assert_eq!(id1, id2, "should return same ID on second call");
        assert!(!id1.as_str().is_empty());
    }

    #[test]
    fn reads_existing_environment_id() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("environment-id");
        std::fs::write(&file, "my-custom-env-id\n").unwrap();
        let id = resolve_or_create_environment_id(dir.path()).unwrap();
        assert_eq!(id.as_str(), "my-custom-env-id");
    }

    #[test]
    fn environment_state_dir_falls_back_to_base_when_machine_scoping_fails() {
        let base = Path::new("/tmp/flotilla-test");
        let resolved = machine_scoped_state_dir_or_base(base, Err("boom".to_string()));
        assert_eq!(resolved, base);
    }

    #[test]
    fn environment_state_dir_prefers_machine_scope_when_available() {
        let base = Path::new("/tmp/flotilla-test");
        let resolved = machine_scoped_state_dir_or_base(base, Ok(base.join("machine-123")));
        assert_eq!(resolved, base.join("machine-123"));
    }

    #[test]
    fn concurrent_environment_id_calls_share_the_same_persisted_value() {
        use std::{
            sync::{Arc, Barrier, Mutex},
            thread,
        };

        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let results: Arc<Mutex<Vec<EnvironmentId>>> = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let results = Arc::clone(&results);
            let path = dir.path().to_path_buf();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let id = resolve_or_create_environment_id(&path).expect("resolve environment id");
                results.lock().unwrap().push(id);
            }));
        }

        for handle in handles {
            handle.join().expect("thread join");
        }

        let ids = results.lock().unwrap();
        assert_eq!(ids.len(), 8);
        let first = ids.first().expect("first id").clone();
        assert!(ids.iter().all(|id| *id == first), "all concurrent callers should observe the same id");
        let file = fs::read_to_string(dir.path().join("environment-id")).expect("environment-id file");
        assert_eq!(file.trim(), first.as_str());
    }

    #[tokio::test]
    async fn machine_scoped_dir_uses_config_override() {
        let base = std::path::Path::new("/tmp/flotilla-test");
        let runner = ProcessCommandRunner;
        let dir = machine_scoped_state_dir(base, Some("my-machine"), &runner).await.unwrap();
        assert_eq!(dir, base.join("my-machine"));
    }

    #[tokio::test]
    async fn machine_scoped_dir_falls_back_to_etc_machine_id() {
        let base = std::path::Path::new("/tmp/flotilla-test");
        let runner = ProcessCommandRunner;
        // This test only works on Linux with /etc/machine-id
        if std::path::Path::new("/etc/machine-id").exists() {
            let dir = machine_scoped_state_dir(base, None, &runner).await.unwrap();
            assert!(dir.starts_with(base));
            assert_ne!(dir, *base);
        }
    }

    #[test]
    fn remote_environment_state_dir_prefers_xdg_state_home() {
        let env = TestEnvVars::new([("XDG_STATE_HOME", "/state"), ("HOME", "/home/build")]);
        assert_eq!(remote_environment_state_dir(&env), Some(PathBuf::from("/state/flotilla")));
    }

    #[test]
    fn remote_environment_state_dir_falls_back_to_home() {
        let env = TestEnvVars::new([("HOME", "/home/build")]);
        assert_eq!(remote_environment_state_dir(&env), Some(PathBuf::from("/home/build/.local/state/flotilla")));
    }

    #[tokio::test]
    async fn remote_environment_id_uses_fallback_when_state_dir_env_is_missing() {
        let runner = ProcessCommandRunner;
        let fallback_id = EnvironmentId::new("static-ssh-fallback");
        let resolved = resolve_or_create_remote_environment_id(&runner, &TestEnvVars::default(), fallback_id.clone())
            .await
            .expect("resolve remote environment id");
        assert_eq!(resolved, fallback_id);
    }

    #[tokio::test]
    async fn remote_environment_id_is_created_and_reused_in_remote_state_dir() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let runner = ProcessCommandRunner;
        let env = TestEnvVars::new([("XDG_STATE_HOME", temp.path().to_string_lossy().into_owned())]);

        let first =
            resolve_or_create_remote_environment_id(&runner, &env, EnvironmentId::new("static-ssh-fallback")).await.expect("first resolve");
        let second = resolve_or_create_remote_environment_id(&runner, &env, EnvironmentId::new("static-ssh-fallback"))
            .await
            .expect("second resolve");

        assert_eq!(first, second);
        assert_ne!(first.as_str(), "static-ssh-fallback");
        let file = fs::read_to_string(temp.path().join("flotilla/environment-id")).expect("environment-id file");
        assert_eq!(file.trim(), first.as_str());
    }

    #[tokio::test]
    async fn resolve_local_node_id_uses_machine_scoped_identity_dir() {
        let base = tempfile::tempdir().unwrap();
        let machine_runner = crate::providers::discovery::test_support::DiscoveryMockRunner::builder()
            .on_run("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"], Ok("\"IOPlatformUUID\" = \"machine-uuid\"\n".into()))
            .build();

        let node_id_1 = resolve_local_node_id(base.path(), None, &machine_runner).await.unwrap();
        let node_id_2 = resolve_local_node_id(base.path(), None, &machine_runner).await.unwrap();

        assert_eq!(node_id_1, node_id_2);
        let identity_dir = machine_scoped_state_dir(&base.path().join("identity"), None, &machine_runner).await.unwrap();
        assert!(identity_dir.join("node.key").exists());
        assert!(identity_dir.join("node.pub").exists());
    }

    #[tokio::test]
    async fn resolve_local_node_id_errors_when_machine_identity_is_unavailable() {
        let base = tempfile::tempdir().unwrap();
        let runner = crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build();
        if read_etc_machine_id().is_none() {
            let err = resolve_local_node_id(base.path(), None, &runner).await.unwrap_err();
            assert!(err.contains("Cannot determine machine identity"));
        }
    }

    #[tokio::test]
    async fn resolve_local_node_id_uses_config_machine_id_override() {
        let base = tempfile::tempdir().unwrap();
        let runner = crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build();

        let node_id_1 = resolve_local_node_id(base.path(), Some("override-machine"), &runner).await.unwrap();
        let node_id_2 = resolve_local_node_id(base.path(), Some("override-machine"), &runner).await.unwrap();

        assert_eq!(node_id_1, node_id_2);
        let identity_dir = base.path().join("identity/override-machine");
        assert!(identity_dir.join("node.key").exists());
        assert!(identity_dir.join("node.pub").exists());
    }

    #[cfg(unix)]
    #[test]
    fn node_key_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let _ = resolve_or_create_node_id(dir.path()).unwrap();
        let mode = fs::metadata(dir.path().join("node.key")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
