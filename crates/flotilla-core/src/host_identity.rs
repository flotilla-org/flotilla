//! Stable host identity with machine-id scoping.
//!
//! Pre-wired for the node identity spec (2026-03-28). Will be called from
//! daemon startup to resolve the local machine's `HostId` once `NodeId`
//! and the full identity lifecycle are implemented.

use std::{
    fs,
    path::{Path, PathBuf},
};

use flotilla_protocol::qualified_path::HostId;
use uuid::Uuid;

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
