//! Stable host identity with machine-id scoping.
//!
//! Pre-wired for the node identity spec (2026-03-28). Will be called from
//! daemon startup to resolve the local machine's `HostId` once `NodeId`
//! and the full identity lifecycle are implemented.

use std::{
    fs,
    path::{Path, PathBuf},
};

use flotilla_protocol::{qualified_path::HostId, EnvironmentId};
use uuid::Uuid;
use tracing::warn;

use crate::providers::{ChannelLabel, CommandRunner};

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
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
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
pub async fn resolve_local_environment_state_dir(base_state_dir: &Path, runner: &dyn CommandRunner) -> PathBuf {
    let resolved = machine_scoped_state_dir(base_state_dir, None, runner).await;
    machine_scoped_state_dir_or_base(base_state_dir, resolved)
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

    // Generate a new ID and write atomically.
    let new_id = Uuid::new_v4().to_string();
    let temp = state_dir.join(format!(".host-id.{}", std::process::id()));

    fs::create_dir_all(state_dir).map_err(|e| format!("failed to create state dir {}: {e}", state_dir.display()))?;
    fs::write(&temp, format!("{new_id}\n")).map_err(|e| format!("failed to write temp host-id: {e}"))?;

    match fs::hard_link(&temp, &target) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
            Ok(HostId::new(new_id))
        }
        Err(_) => {
            // Another process won the race — use its value.
            let _ = fs::remove_file(&temp);
            let content = fs::read_to_string(&target).map_err(|e| format!("failed to read host-id after link race: {e}"))?;
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Err("host-id file exists but is empty".to_owned());
            }
            Ok(HostId::new(trimmed))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProcessCommandRunner;

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
}
