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
    let att_id = mgr.allocate_terminal(set_id, "agent", 1, "feat", "claude", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

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
async fn attach_command_rejects_remote_attachable() {
    use flotilla_protocol::AttachableSet;

    use crate::attachable::{Attachable, AttachableContent, TerminalAttachable, TerminalPurpose};

    let store = shared_in_memory_attachable_store();
    let local_host = HostName::new("my-laptop");
    let remote_host = HostName::new("remote-server");

    // Insert an attachable set with a remote host affinity.
    {
        let mut s = store.lock().expect("lock");
        let set_id = s.allocate_set_id();
        let att_id = s.allocate_attachable_id();
        s.insert_set(AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(remote_host),
            checkout: None,
            template_identity: None,
            members: vec![att_id.clone()],
        });
        s.insert_attachable(Attachable {
            id: att_id,
            set_id,
            content: AttachableContent::Terminal(TerminalAttachable {
                purpose: TerminalPurpose { checkout: "feat".to_string(), role: "shell".to_string(), index: 0 },
                command: "bash".to_string(),
                working_directory: PathBuf::from("/remote/wt-feat"),
                status: flotilla_protocol::TerminalStatus::Disconnected,
            }),
        });
    }

    let att_id = {
        let s = store.lock().expect("lock");
        s.registry().attachables.keys().next().expect("should have one attachable").clone()
    };

    let mgr = TerminalManager::new(Arc::new(MockTerminalPool::new()), store, local_host);
    let err = mgr.attach_command(&att_id, None).await.expect_err("should reject remote attachable");
    assert!(err.contains("does not support remote attachables"), "error should mention remote attachables not supported: {err}");
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
    let att_id_2 = mgr.allocate_terminal(set_id, "agent", 0, "feat", "claude", PathBuf::from("/repo/wt-feat")).expect("allocate_terminal");

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
