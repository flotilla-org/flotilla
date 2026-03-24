use std::{path::PathBuf, sync::Arc};

use flotilla_protocol::{AttachableId, AttachableSet, AttachableSetId, HostName, HostPath, TerminalStatus};
use tracing::warn;

use crate::{
    attachable::{Attachable, AttachableContent, SharedAttachableStore, TerminalAttachable, TerminalPurpose},
    providers::terminal::{TerminalEnvVars, TerminalPool},
};

/// Summary of a managed terminal for external consumers.
#[derive(Debug, Clone)]
pub struct TerminalInfo {
    pub attachable_id: AttachableId,
    pub attachable_set_id: AttachableSetId,
    pub role: String,
    pub checkout: String,
    pub index: u32,
    pub command: String,
    pub working_directory: PathBuf,
    pub status: TerminalStatus,
}

/// Manages terminal session lifecycle using a `TerminalPool` for CLI operations
/// and an `AttachableStore` for identity and state persistence.
///
/// The `TerminalManager` owns the mapping between `AttachableId`s (stable identities)
/// and session names (opaque strings passed to the pool). Currently the session name
/// is simply `attachable_id.to_string()`.
pub struct TerminalManager {
    pool: Arc<dyn TerminalPool>,
    store: SharedAttachableStore,
}

impl TerminalManager {
    pub fn new(pool: Arc<dyn TerminalPool>, store: SharedAttachableStore) -> Self {
        Self { pool, store }
    }

    /// Returns the existing `AttachableSet` for the given checkout, or creates a new one.
    pub fn allocate_set(&self, host: HostName, checkout_path: HostPath) -> Result<AttachableSetId, String> {
        let mut store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
        let existing = store.sets_for_checkout(&checkout_path);
        if let Some(id) = existing.into_iter().next() {
            return Ok(id);
        }
        let id = store.allocate_set_id();
        store.insert_set(AttachableSet {
            id: id.clone(),
            host_affinity: Some(host),
            checkout: Some(checkout_path),
            template_identity: None,
            members: Vec::new(),
        });
        Ok(id)
    }

    /// Returns the existing terminal for the given purpose within a set, or creates a new one.
    pub fn allocate_terminal(
        &self,
        set_id: AttachableSetId,
        role: &str,
        index: u32,
        checkout: &str,
        command: &str,
        working_directory: PathBuf,
    ) -> Result<AttachableId, String> {
        let mut store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
        let target_purpose = TerminalPurpose { checkout: checkout.to_string(), role: role.to_string(), index };
        // Return existing terminal if one matches the purpose within this set.
        for (id, attachable) in store.registry().attachables.iter() {
            if attachable.set_id != set_id {
                continue;
            }
            let AttachableContent::Terminal(t) = &attachable.content;
            if t.purpose == target_purpose {
                return Ok(id.clone());
            }
        }
        let id = store.allocate_attachable_id();
        store.insert_attachable(Attachable {
            id: id.clone(),
            set_id: set_id.clone(),
            content: AttachableContent::Terminal(TerminalAttachable {
                purpose: target_purpose,
                command: command.to_string(),
                working_directory,
                status: TerminalStatus::Disconnected,
            }),
        });
        // Add the member link to the set.
        let mut set = store.registry().sets.get(&set_id).cloned().ok_or_else(|| format!("set not found: {set_id}"))?;
        if !set.members.contains(&id) {
            set.members.push(id.clone());
            store.insert_set(set);
        }
        Ok(id)
    }

    /// Ensures the terminal session is running in the pool.
    /// Reads command and working directory from the stored attachable.
    pub async fn ensure_running(&self, attachable_id: &AttachableId) -> Result<(), String> {
        let (command, cwd) = {
            let store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
            let attachable =
                store.registry().attachables.get(attachable_id).ok_or_else(|| format!("attachable not found: {attachable_id}"))?;
            match &attachable.content {
                AttachableContent::Terminal(t) => (t.command.clone(), t.working_directory.clone()),
            }
        };
        let session_name = attachable_id.to_string();
        self.pool.ensure_session(&session_name, &command, &cwd).await
    }

    /// Returns the command string needed to attach to a terminal session.
    /// Injects `FLOTILLA_ATTACHABLE_ID` and optionally `FLOTILLA_DAEMON_SOCKET` env vars.
    pub async fn attach_command(&self, attachable_id: &AttachableId, daemon_socket_path: Option<&str>) -> Result<String, String> {
        let (command, cwd) = {
            let store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
            let attachable =
                store.registry().attachables.get(attachable_id).ok_or_else(|| format!("attachable not found: {attachable_id}"))?;
            match &attachable.content {
                AttachableContent::Terminal(t) => (t.command.clone(), t.working_directory.clone()),
            }
        };
        let mut env_vars: TerminalEnvVars = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), attachable_id.to_string())];
        if let Some(socket) = daemon_socket_path {
            env_vars.push(("FLOTILLA_DAEMON_SOCKET".to_string(), socket.to_string()));
        }
        let session_name = attachable_id.to_string();
        self.pool.attach_command(&session_name, &command, &cwd, &env_vars).await
    }

    /// Kills a terminal session in the pool.
    pub async fn kill_terminal(&self, attachable_id: &AttachableId) -> Result<(), String> {
        let session_name = attachable_id.to_string();
        self.pool.kill_session(&session_name).await
    }

    /// Refreshes terminal state by querying the pool and reconciling with the store.
    /// Returns info for all known terminals.
    pub async fn refresh(&self) -> Result<Vec<TerminalInfo>, String> {
        let live_sessions = self.pool.list_sessions().await?;
        let live_names: std::collections::HashSet<String> = live_sessions.iter().map(|s| s.session_name.clone()).collect();
        let live_status: std::collections::HashMap<String, TerminalStatus> =
            live_sessions.into_iter().map(|s| (s.session_name, s.status)).collect();

        let mut store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
        let terminal_ids: Vec<AttachableId> = store
            .registry()
            .attachables
            .iter()
            .filter(|(_, a)| matches!(&a.content, AttachableContent::Terminal(_)))
            .map(|(id, _)| id.clone())
            .collect();

        let mut infos = Vec::new();
        for id in &terminal_ids {
            let session_name = id.to_string();
            let new_status = if live_names.contains(&session_name) {
                live_status.get(&session_name).cloned().unwrap_or(TerminalStatus::Running)
            } else {
                TerminalStatus::Disconnected
            };
            store.update_terminal_status(id, new_status.clone());

            if let Some(attachable) = store.registry().attachables.get(id) {
                match &attachable.content {
                    AttachableContent::Terminal(t) => {
                        infos.push(TerminalInfo {
                            attachable_id: id.clone(),
                            attachable_set_id: attachable.set_id.clone(),
                            role: t.purpose.role.clone(),
                            checkout: t.purpose.checkout.clone(),
                            index: t.purpose.index,
                            command: t.command.clone(),
                            working_directory: t.working_directory.clone(),
                            status: new_status,
                        });
                    }
                }
            }
        }
        Ok(infos)
    }

    /// Removes all sets matching the given checkout paths and kills their sessions.
    /// Session kill failures are logged but do not cause the overall operation to fail.
    pub async fn cascade_delete(&self, checkout_paths: &[HostPath]) -> Result<(), String> {
        let attachable_ids_to_kill = {
            let mut store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
            let mut ids_to_kill = Vec::new();

            let mut any_removed = false;
            for checkout in checkout_paths {
                let set_ids = store.sets_for_checkout(checkout);
                for set_id in set_ids {
                    if let Some(set) = store.registry().sets.get(&set_id) {
                        ids_to_kill.extend(set.members.iter().cloned());
                    }
                    if store.remove_set(&set_id).is_some() {
                        any_removed = true;
                    }
                }
            }
            if any_removed {
                if let Err(e) = store.save() {
                    warn!(error = %e, "failed to persist store after cascade delete");
                }
            }
            ids_to_kill
        };

        for id in &attachable_ids_to_kill {
            let session_name = id.to_string();
            if let Err(e) = self.pool.kill_session(&session_name).await {
                warn!(%session_name, error = %e, "failed to kill session during cascade delete");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
