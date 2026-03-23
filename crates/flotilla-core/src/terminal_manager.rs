use std::{path::PathBuf, sync::Arc};

use flotilla_protocol::{arg, AttachableId, AttachableSet, AttachableSetId, HostName, HostPath, TerminalStatus};
use tracing::warn;

use crate::{
    attachable::{Attachable, AttachableContent, SharedAttachableStore, TerminalAttachable, TerminalPurpose},
    hop_chain::{
        builder::HopPlanBuilder,
        remote::NoopRemoteHopResolver,
        resolver::{AlwaysWrap, HopResolver},
        terminal::PoolTerminalHopResolver,
        ResolutionContext, ResolvedAction,
    },
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
    local_host: HostName,
}

impl TerminalManager {
    pub fn new(pool: Arc<dyn TerminalPool>, store: SharedAttachableStore, local_host: HostName) -> Self {
        Self { pool, store, local_host }
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

        let terminal_resolver =
            PoolTerminalHopResolver::new(Arc::clone(&self.pool), self.store.clone(), daemon_socket_path.map(|s| s.to_string()));
        let hop_resolver =
            HopResolver { remote: Arc::new(NoopRemoteHopResolver), terminal: Arc::new(terminal_resolver), strategy: Arc::new(AlwaysWrap) };

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
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use async_trait::async_trait;
    use flotilla_protocol::{HostName, HostPath, TerminalStatus};

    use super::*;
    use crate::{
        attachable::{shared_in_memory_attachable_store, AttachableContent},
        providers::terminal::{TerminalEnvVars, TerminalPool, TerminalSession},
    };

    #[derive(Debug, Clone)]
    #[allow(dead_code)] // Fields used for test assertions via Debug matching
    enum PoolCall {
        ListSessions,
        EnsureSession { session_name: String, command: String, cwd: PathBuf },
        AttachCommand { session_name: String, command: String, cwd: PathBuf, env_vars: TerminalEnvVars },
        KillSession { session_name: String },
    }

    struct MockTerminalPool {
        calls: Mutex<Vec<PoolCall>>,
        list_response: Mutex<Vec<TerminalSession>>,
    }

    impl MockTerminalPool {
        fn new() -> Self {
            Self { calls: Mutex::new(Vec::new()), list_response: Mutex::new(Vec::new()) }
        }

        fn with_sessions(sessions: Vec<TerminalSession>) -> Self {
            Self { calls: Mutex::new(Vec::new()), list_response: Mutex::new(sessions) }
        }
    }

    #[async_trait]
    impl TerminalPool for MockTerminalPool {
        async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
            self.calls.lock().expect("lock calls").push(PoolCall::ListSessions);
            Ok(self.list_response.lock().expect("lock list_response").clone())
        }

        async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String> {
            self.calls.lock().expect("lock calls").push(PoolCall::EnsureSession {
                session_name: session_name.to_string(),
                command: command.to_string(),
                cwd: cwd.to_path_buf(),
            });
            Ok(())
        }

        fn attach_args(
            &self,
            session_name: &str,
            command: &str,
            cwd: &Path,
            env_vars: &TerminalEnvVars,
        ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
            self.calls.lock().expect("lock calls").push(PoolCall::AttachCommand {
                session_name: session_name.to_string(),
                command: command.to_string(),
                cwd: cwd.to_path_buf(),
                env_vars: env_vars.clone(),
            });
            Ok(vec![flotilla_protocol::arg::Arg::Literal(format!("attach {session_name}"))])
        }

        async fn kill_session(&self, session_name: &str) -> Result<(), String> {
            self.calls.lock().expect("lock calls").push(PoolCall::KillSession { session_name: session_name.to_string() });
            Ok(())
        }
    }

    fn test_host() -> HostName {
        HostName::new("test-host")
    }

    fn test_checkout() -> HostPath {
        HostPath::new(test_host(), PathBuf::from("/repo/wt-feat"))
    }

    #[tokio::test]
    async fn allocate_set_creates_store_entry() {
        let store = shared_in_memory_attachable_store();
        let mgr = TerminalManager::new(Arc::new(MockTerminalPool::new()), store.clone(), test_host());

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");

        let store = store.lock().expect("lock store");
        let set = store.registry().sets.get(&set_id).expect("set should exist");
        assert_eq!(set.host_affinity, Some(test_host()));
        assert_eq!(set.checkout, Some(test_checkout()));
        assert!(set.members.is_empty());
    }

    #[tokio::test]
    async fn allocate_terminal_creates_attachable() {
        let store = shared_in_memory_attachable_store();
        let mgr = TerminalManager::new(Arc::new(MockTerminalPool::new()), store.clone(), test_host());

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id =
            mgr.allocate_terminal(set_id.clone(), "shell", 0, "feat", "$SHELL", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        let store = store.lock().expect("lock store");
        let attachable = store.registry().attachables.get(&att_id).expect("attachable should exist");
        assert_eq!(attachable.set_id, set_id);
        match &attachable.content {
            AttachableContent::Terminal(t) => {
                assert_eq!(t.purpose.role, "shell");
                assert_eq!(t.purpose.checkout, "feat");
                assert_eq!(t.purpose.index, 0);
                assert_eq!(t.command, "$SHELL");
                assert_eq!(t.working_directory, PathBuf::from("/repo/wt-feat"));
                assert_eq!(t.status, TerminalStatus::Disconnected);
            }
        }
    }

    #[tokio::test]
    async fn ensure_running_delegates_to_pool() {
        let store = shared_in_memory_attachable_store();
        let pool = MockTerminalPool::new();
        let mgr = TerminalManager::new(Arc::new(pool), store.clone(), test_host());

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id = mgr.allocate_terminal(set_id, "shell", 0, "feat", "bash", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        mgr.ensure_running(&att_id).await.expect("ensure_running");

        // We can't access the pool directly after moving it, so we verify through the store
        // that the method completed successfully (no error returned).
    }

    #[tokio::test]
    async fn ensure_running_uses_attachable_id_as_session_name() {
        let store = shared_in_memory_attachable_store();
        let mock = std::sync::Arc::new(MockTerminalPool::new());

        // We need to use Arc to share the mock between the manager and our test.
        // But TerminalManager takes Box<dyn TerminalPool>. Let's use a different approach.
        // We'll create a wrapper that records calls via shared state.
        let calls: std::sync::Arc<Mutex<Vec<PoolCall>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();

        struct SharedMock {
            calls: std::sync::Arc<Mutex<Vec<PoolCall>>>,
        }

        #[async_trait]
        impl TerminalPool for SharedMock {
            async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
                self.calls.lock().expect("lock").push(PoolCall::ListSessions);
                Ok(Vec::new())
            }
            async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String> {
                self.calls.lock().expect("lock").push(PoolCall::EnsureSession {
                    session_name: session_name.to_string(),
                    command: command.to_string(),
                    cwd: cwd.to_path_buf(),
                });
                Ok(())
            }
            fn attach_args(
                &self,
                session_name: &str,
                command: &str,
                cwd: &Path,
                env_vars: &TerminalEnvVars,
            ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
                self.calls.lock().expect("lock").push(PoolCall::AttachCommand {
                    session_name: session_name.to_string(),
                    command: command.to_string(),
                    cwd: cwd.to_path_buf(),
                    env_vars: env_vars.clone(),
                });
                Ok(vec![flotilla_protocol::arg::Arg::Literal(format!("attach {session_name}"))])
            }
            async fn kill_session(&self, session_name: &str) -> Result<(), String> {
                self.calls.lock().expect("lock").push(PoolCall::KillSession { session_name: session_name.to_string() });
                Ok(())
            }
        }

        let mgr = TerminalManager::new(Arc::new(SharedMock { calls: calls_clone }), store.clone(), test_host());
        let _ = mock; // silence unused warning

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id = mgr.allocate_terminal(set_id, "shell", 0, "feat", "bash", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        mgr.ensure_running(&att_id).await.expect("ensure_running");

        let recorded = calls.lock().expect("lock");
        assert_eq!(recorded.len(), 1);
        match &recorded[0] {
            PoolCall::EnsureSession { session_name, command, cwd } => {
                assert_eq!(session_name, &att_id.to_string());
                assert_eq!(command, "bash");
                assert_eq!(cwd, &PathBuf::from("/repo/wt-feat"));
            }
            other => panic!("expected EnsureSession, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn attach_command_includes_env_vars() {
        let store = shared_in_memory_attachable_store();
        let calls: std::sync::Arc<Mutex<Vec<PoolCall>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();

        struct SharedMock {
            calls: std::sync::Arc<Mutex<Vec<PoolCall>>>,
        }

        #[async_trait]
        impl TerminalPool for SharedMock {
            async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
                Ok(Vec::new())
            }
            async fn ensure_session(&self, _: &str, _: &str, _: &Path) -> Result<(), String> {
                Ok(())
            }
            fn attach_args(
                &self,
                session_name: &str,
                command: &str,
                cwd: &Path,
                env_vars: &TerminalEnvVars,
            ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
                self.calls.lock().expect("lock").push(PoolCall::AttachCommand {
                    session_name: session_name.to_string(),
                    command: command.to_string(),
                    cwd: cwd.to_path_buf(),
                    env_vars: env_vars.clone(),
                });
                Ok(vec![flotilla_protocol::arg::Arg::Literal(format!("attach {session_name}"))])
            }
            async fn kill_session(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let mgr = TerminalManager::new(Arc::new(SharedMock { calls: calls_clone }), store.clone(), test_host());

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id =
            mgr.allocate_terminal(set_id, "agent", 1, "feat", "claude", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        let result = mgr.attach_command(&att_id, Some("/tmp/flotilla.sock")).await.expect("attach_command");
        assert!(result.contains("attach"));

        let recorded = calls.lock().expect("lock");
        assert_eq!(recorded.len(), 1);
        match &recorded[0] {
            PoolCall::AttachCommand { session_name, env_vars, .. } => {
                assert_eq!(session_name, &att_id.to_string());
                assert!(env_vars.iter().any(|(k, v)| k == "FLOTILLA_ATTACHABLE_ID" && v == att_id.as_str()));
                assert!(env_vars.iter().any(|(k, v)| k == "FLOTILLA_DAEMON_SOCKET" && v == "/tmp/flotilla.sock"));
            }
            other => panic!("expected AttachCommand, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_terminal_delegates_to_pool() {
        let store = shared_in_memory_attachable_store();
        let calls: std::sync::Arc<Mutex<Vec<PoolCall>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();

        struct SharedMock {
            calls: std::sync::Arc<Mutex<Vec<PoolCall>>>,
        }

        #[async_trait]
        impl TerminalPool for SharedMock {
            async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
                Ok(Vec::new())
            }
            async fn ensure_session(&self, _: &str, _: &str, _: &Path) -> Result<(), String> {
                Ok(())
            }
            fn attach_args(&self, _: &str, _: &str, _: &Path, _: &TerminalEnvVars) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
                Ok(vec![])
            }
            async fn kill_session(&self, session_name: &str) -> Result<(), String> {
                self.calls.lock().expect("lock").push(PoolCall::KillSession { session_name: session_name.to_string() });
                Ok(())
            }
        }

        let mgr = TerminalManager::new(Arc::new(SharedMock { calls: calls_clone }), store.clone(), test_host());

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id = mgr.allocate_terminal(set_id, "shell", 0, "feat", "bash", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        mgr.kill_terminal(&att_id).await.expect("kill_terminal");

        let recorded = calls.lock().expect("lock");
        assert_eq!(recorded.len(), 1);
        match &recorded[0] {
            PoolCall::KillSession { session_name } => {
                assert_eq!(session_name, &att_id.to_string());
            }
            other => panic!("expected KillSession, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_updates_statuses() {
        let store = shared_in_memory_attachable_store();
        let mgr_for_setup = TerminalManager::new(Arc::new(MockTerminalPool::new()), store.clone(), test_host());

        let set_id = mgr_for_setup.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id =
            mgr_for_setup.allocate_terminal(set_id, "shell", 0, "feat", "bash", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        // Create a new manager with a pool that reports the session as running.
        let pool = MockTerminalPool::with_sessions(vec![TerminalSession {
            session_name: att_id.to_string(),
            status: TerminalStatus::Running,
            command: Some("bash".to_string()),
            working_directory: Some(PathBuf::from("/repo/wt-feat")),
        }]);
        let mgr = TerminalManager::new(Arc::new(pool), store.clone(), test_host());

        let infos = mgr.refresh().await.expect("refresh");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].attachable_id, att_id);
        assert_eq!(infos[0].status, TerminalStatus::Running);
        assert_eq!(infos[0].role, "shell");
        assert_eq!(infos[0].checkout, "feat");
    }

    #[tokio::test]
    async fn refresh_reports_disconnected_for_missing_sessions() {
        let store = shared_in_memory_attachable_store();
        let mgr_for_setup = TerminalManager::new(Arc::new(MockTerminalPool::new()), store.clone(), test_host());

        let set_id = mgr_for_setup.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id =
            mgr_for_setup.allocate_terminal(set_id, "shell", 0, "feat", "bash", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        // Pool returns empty — no live sessions.
        let pool = MockTerminalPool::new();
        let mgr = TerminalManager::new(Arc::new(pool), store.clone(), test_host());

        let infos = mgr.refresh().await.expect("refresh");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].attachable_id, att_id);
        assert_eq!(infos[0].status, TerminalStatus::Disconnected);
    }

    #[tokio::test]
    async fn cascade_delete_removes_sets_and_kills_sessions() {
        let store = shared_in_memory_attachable_store();
        let calls: std::sync::Arc<Mutex<Vec<PoolCall>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();

        struct SharedMock {
            calls: std::sync::Arc<Mutex<Vec<PoolCall>>>,
        }

        #[async_trait]
        impl TerminalPool for SharedMock {
            async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
                Ok(Vec::new())
            }
            async fn ensure_session(&self, _: &str, _: &str, _: &Path) -> Result<(), String> {
                Ok(())
            }
            fn attach_args(&self, _: &str, _: &str, _: &Path, _: &TerminalEnvVars) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
                Ok(vec![])
            }
            async fn kill_session(&self, session_name: &str) -> Result<(), String> {
                self.calls.lock().expect("lock").push(PoolCall::KillSession { session_name: session_name.to_string() });
                Ok(())
            }
        }

        let mgr = TerminalManager::new(Arc::new(SharedMock { calls: calls_clone }), store.clone(), test_host());

        let set_id = mgr.allocate_set(test_host(), test_checkout()).expect("allocate_set");
        let att_id_1 =
            mgr.allocate_terminal(set_id.clone(), "shell", 0, "feat", "bash", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");
        let att_id_2 =
            mgr.allocate_terminal(set_id, "agent", 0, "feat", "claude", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

        mgr.cascade_delete(&[test_checkout()]).await.expect("cascade_delete");

        // Verify store is empty.
        let store = store.lock().expect("lock store");
        assert!(store.registry().sets.is_empty(), "sets should be removed");
        assert!(store.registry().attachables.is_empty(), "attachables should be removed");

        // Verify pool.kill_session was called for both terminals.
        let recorded = calls.lock().expect("lock");
        let killed_names: Vec<&str> = recorded
            .iter()
            .filter_map(|c| match c {
                PoolCall::KillSession { session_name } => Some(session_name.as_str()),
                _ => None,
            })
            .collect();
        assert!(killed_names.contains(&att_id_1.as_str()));
        assert!(killed_names.contains(&att_id_2.as_str()));
    }
}
