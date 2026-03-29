use std::sync::Arc;

use flotilla_protocol::{arg, AttachableId, AttachableSet, AttachableSetId, HostName, QualifiedPath, TerminalStatus};
use tracing::warn;

use crate::{
    attachable::{Attachable, AttachableContent, SharedAttachableStore, TerminalAttachable, TerminalPurpose},
    hop_chain::{
        builder::HopPlanBuilder,
        environment::NoopEnvironmentHopResolver,
        remote::NoopRemoteHopResolver,
        resolver::{AlwaysWrap, HopResolver},
        terminal::PoolTerminalHopResolver,
        ResolutionContext, ResolvedAction,
    },
    path_context::ExecutionEnvironmentPath,
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
    pub working_directory: ExecutionEnvironmentPath,
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
    local_host: HostName,
}

impl TerminalManager {
    pub fn new(pool: Arc<dyn TerminalPool>, store: SharedAttachableStore, local_host: HostName) -> Self {
        Self { pool, store, local_host }
    }

    /// Returns the existing `AttachableSet` for the given checkout, or creates a new one.
    pub fn allocate_set(&self, host: HostName, checkout_path: QualifiedPath) -> Result<AttachableSetId, String> {
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
            environment_id: None,
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
        working_directory: ExecutionEnvironmentPath,
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
    pub async fn ensure_running(&self, attachable_id: &AttachableId, daemon_socket_path: Option<&str>) -> Result<(), String> {
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
        self.pool.ensure_session(&session_name, &command, &cwd, &env_vars).await
    }

    /// Returns the command string needed to attach to a terminal session.
    ///
    /// Uses the hop chain internally: builds a `HopPlan` via `HopPlanBuilder::build_for_attachable()`,
    /// resolves it with `PoolTerminalHopResolver` + `AlwaysWrap`, and flattens to a string.
    /// For local attach (same-host), the plan is just `[AttachTerminal(id)]` with no remote hop.
    pub async fn attach_command(&self, attachable_id: &AttachableId, daemon_socket_path: Option<&str>) -> Result<String, String> {
        let plan = {
            let store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
            let builder = HopPlanBuilder::new(&self.local_host);
            builder.build_for_attachable(attachable_id, &*store)?
        };

        // Guard: attach_command only supports local attachables.
        // Remote terminals should use the workspace flow which routes through
        // the hop chain with a real SSH resolver.
        if plan.0.iter().any(|hop| matches!(hop, crate::hop_chain::Hop::RemoteToHost { .. })) {
            return Err(
                "attach_command does not support remote attachables — use the workspace flow for remote terminal attach".to_string()
            );
        }

        let terminal_resolver =
            PoolTerminalHopResolver::new(Arc::clone(&self.pool), self.store.clone(), daemon_socket_path.map(|s| s.to_string()));
        let hop_resolver = HopResolver {
            remote: Arc::new(NoopRemoteHopResolver),
            environment: Arc::new(NoopEnvironmentHopResolver),
            terminal: Arc::new(terminal_resolver),
            strategy: Arc::new(AlwaysWrap),
        };

        let mut context = ResolutionContext {
            current_host: self.local_host.clone(),
            current_environment: None,
            working_directory: None,
            actions: Vec::new(),
            nesting_depth: 0,
        };
        let resolved = hop_resolver.resolve(&plan, &mut context)?;

        resolved
            .0
            .into_iter()
            .find_map(|action| match action {
                ResolvedAction::Command(args) => Some(arg::flatten(&args, 0)),
                _ => None,
            })
            .ok_or_else(|| "hop chain resolution produced no Command action for attach".to_string())
    }

    /// Returns a structured `Arg` tree for attaching to a terminal session.
    /// Like `attach_command()` but returns `Vec<Arg>` instead of a flat string.
    pub fn attach_args(
        &self,
        attachable_id: &AttachableId,
        daemon_socket_path: Option<&str>,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
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
        self.pool.attach_args(&session_name, &command, &cwd, &env_vars)
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
    pub async fn cascade_delete(&self, checkout_paths: &[QualifiedPath]) -> Result<(), String> {
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
