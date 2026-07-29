use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};

pub(crate) const CONTAINER_CODEX_HOME: &str = "/run/flotilla/codex";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexSlotLease {
    Leased { host_path: PathBuf },
    Waiting { message: String },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LeaseState {
    #[serde(default)]
    leases: BTreeMap<String, String>,
}

pub(crate) struct CodexSlotPool {
    pool_dir: PathBuf,
    state_path: PathBuf,
    supported: bool,
    lock: Mutex<()>,
}

impl CodexSlotPool {
    pub(crate) fn new(pool_dir: PathBuf, state_path: PathBuf) -> Self {
        Self { pool_dir, state_path, supported: cfg!(any(target_os = "linux", test)), lock: Mutex::new(()) }
    }

    #[cfg(test)]
    pub(crate) fn supported(pool_dir: PathBuf, state_path: PathBuf) -> Self {
        Self { pool_dir, state_path, supported: true, lock: Mutex::new(()) }
    }

    #[cfg(test)]
    fn unsupported(pool_dir: PathBuf, state_path: PathBuf) -> Self {
        Self { pool_dir, state_path, supported: false, lock: Mutex::new(()) }
    }

    pub(crate) async fn acquire(&self, environment_ref: &str) -> Result<CodexSlotLease, String> {
        if !self.supported {
            return Err("Codex login slot delivery is supported only on Linux placement hosts".to_string());
        }
        let _guard = self.lock.lock().await;
        let slots = self.usable_slots().await?;
        let slot_names = slots.iter().map(|(name, _)| name.clone()).collect::<BTreeSet<_>>();
        let mut state = self.load_state().await?;
        state.leases.retain(|slot, _| slot_names.contains(slot));

        if let Some((slot, _)) = state.leases.iter().find(|(_, holder)| holder.as_str() == environment_ref) {
            let host_path =
                slots.iter().find(|(name, _)| name == slot).map(|(_, path)| path.clone()).expect("retained lease points at a usable slot");
            self.persist_state(&state).await?;
            return Ok(CodexSlotLease::Leased { host_path });
        }

        if let Some((slot, host_path)) = slots.iter().find(|(slot, _)| !state.leases.contains_key(slot.as_str())) {
            state.leases.insert(slot.clone(), environment_ref.to_string());
            self.persist_state(&state).await?;
            return Ok(CodexSlotLease::Leased { host_path: host_path.clone() });
        }

        self.persist_state(&state).await?;
        Ok(CodexSlotLease::Waiting {
            message: format!(
                "waiting for codex login slot; {} in pool, all leased; mint another slot to increase concurrency",
                slots.len()
            ),
        })
    }

    pub(crate) async fn release(&self, environment_ref: &str) -> Result<(), String> {
        if !self.supported {
            return Ok(());
        }
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        state.leases.retain(|_, holder| holder != environment_ref);
        self.persist_state(&state).await
    }

    pub(crate) async fn recover(&self, active_environment_refs: &BTreeSet<String>) -> Result<(), String> {
        if !self.supported {
            return Ok(());
        }
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        state.leases.retain(|_, holder| active_environment_refs.contains(holder));
        self.persist_state(&state).await
    }

    async fn usable_slots(&self) -> Result<Vec<(String, PathBuf)>, String> {
        let mut entries = match fs::read_dir(&self.pool_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("read Codex login pool {}: {error}", self.pool_dir.display())),
        };
        let mut numbered = Vec::new();
        while let Some(entry) =
            entries.next_entry().await.map_err(|error| format!("read Codex login pool {}: {error}", self.pool_dir.display()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(number) = name.strip_prefix("slot-").and_then(|suffix| suffix.parse::<u64>().ok()) else {
                continue;
            };
            let path = entry.path();
            let auth_path = path.join("auth.json");
            let Ok(metadata) = fs::metadata(&auth_path).await else {
                continue;
            };
            if metadata.is_file() && metadata.len() > 0 && metadata.permissions().mode() & 0o777 == 0o600 {
                numbered.push((number, name, path));
            }
        }
        numbered.sort_by_key(|(number, _, _)| *number);
        Ok(numbered.into_iter().map(|(_, name, path)| (name, path)).collect())
    }

    async fn load_state(&self) -> Result<LeaseState, String> {
        match fs::read_to_string(&self.state_path).await {
            Ok(encoded) => serde_json::from_str(&encoded)
                .map_err(|error| format!("parse Codex login lease state {}: {error}", self.state_path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LeaseState::default()),
            Err(error) => Err(format!("read Codex login lease state {}: {error}", self.state_path.display())),
        }
    }

    async fn persist_state(&self, state: &LeaseState) -> Result<(), String> {
        let parent =
            self.state_path.parent().ok_or_else(|| format!("Codex login lease state {} has no parent", self.state_path.display()))?;
        fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("create Codex login lease state directory {}: {error}", parent.display()))?;
        let temporary = parent.join(format!(".codex-slot-leases-{}.tmp", uuid::Uuid::new_v4()));
        let encoded = serde_json::to_vec_pretty(state).map_err(|error| format!("serialize Codex login lease state: {error}"))?;
        let operation = async {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .await
                .map_err(|error| format!("create Codex login lease state {}: {error}", temporary.display()))?;
            file.write_all(&encoded).await.map_err(|error| format!("write Codex login lease state {}: {error}", temporary.display()))?;
            file.sync_all().await.map_err(|error| format!("sync Codex login lease state {}: {error}", temporary.display()))?;
            drop(file);
            fs::rename(&temporary, &self.state_path)
                .await
                .map_err(|error| format!("replace Codex login lease state {}: {error}", self.state_path.display()))
        }
        .await;
        if operation.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        operation
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn write_slot(root: &Path, number: u64) -> PathBuf {
        let slot = root.join(format!("slot-{number}"));
        std::fs::create_dir_all(&slot).expect("create slot");
        let auth = slot.join("auth.json");
        std::fs::write(&auth, format!("{{\"slot\":{number}}}")).expect("write auth");
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).expect("protect auth");
        slot
    }

    #[tokio::test]
    async fn concurrent_environments_receive_distinct_slots_and_waiter_proceeds_after_release() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool_dir = temp.path().join("pool");
        let slot_0 = write_slot(&pool_dir, 0);
        let slot_1 = write_slot(&pool_dir, 1);
        let pool = CodexSlotPool::supported(pool_dir, temp.path().join("leases.json"));

        assert_eq!(pool.acquire("env-a").await.expect("lease a"), CodexSlotLease::Leased { host_path: slot_0 });
        assert_eq!(pool.acquire("env-b").await.expect("lease b"), CodexSlotLease::Leased { host_path: slot_1.clone() });
        assert_eq!(pool.acquire("env-c").await.expect("wait c"), CodexSlotLease::Waiting {
            message: "waiting for codex login slot; 2 in pool, all leased; mint another slot to increase concurrency".to_string(),
        });

        pool.release("env-b").await.expect("release b");
        assert_eq!(pool.acquire("env-c").await.expect("lease c"), CodexSlotLease::Leased { host_path: slot_1 });
    }

    #[tokio::test]
    async fn persisted_lease_survives_restart_and_recovery_releases_orphans() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool_dir = temp.path().join("pool");
        let slot_0 = write_slot(&pool_dir, 0);
        let state_path = temp.path().join("leases.json");
        CodexSlotPool::supported(pool_dir.clone(), state_path.clone()).acquire("env-a").await.expect("lease a");

        let restarted = CodexSlotPool::supported(pool_dir, state_path);
        assert_eq!(restarted.acquire("env-a").await.expect("recover a"), CodexSlotLease::Leased { host_path: slot_0.clone() });
        assert!(matches!(restarted.acquire("env-b").await.expect("wait b"), CodexSlotLease::Waiting { .. }));

        restarted.recover(&BTreeSet::new()).await.expect("release orphan");
        assert_eq!(restarted.acquire("env-b").await.expect("lease b"), CodexSlotLease::Leased { host_path: slot_0 });
    }

    #[tokio::test]
    async fn slots_without_protected_nonempty_auth_are_not_leaseable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool_dir = temp.path().join("pool");
        let slot = write_slot(&pool_dir, 0);
        std::fs::set_permissions(slot.join("auth.json"), std::fs::Permissions::from_mode(0o644)).expect("weaken auth");
        let pool = CodexSlotPool::supported(pool_dir, temp.path().join("leases.json"));

        assert_eq!(pool.acquire("env-a").await.expect("wait"), CodexSlotLease::Waiting {
            message: "waiting for codex login slot; 0 in pool, all leased; mint another slot to increase concurrency".to_string(),
        });
    }

    #[tokio::test]
    async fn unsupported_hosts_do_not_create_lease_state_during_recovery_or_release() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_path = temp.path().join("leases.json");
        let pool = CodexSlotPool::unsupported(temp.path().join("pool"), state_path.clone());

        pool.recover(&BTreeSet::from(["env-a".to_string()])).await.expect("skip unsupported recovery");
        pool.release("env-a").await.expect("skip unsupported release");

        assert!(!state_path.exists());
    }
}
