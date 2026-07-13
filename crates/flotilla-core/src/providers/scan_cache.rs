use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        presentation::PresentationManager,
        terminal::{TerminalEnvVars, TerminalPool, TerminalSession},
        types::{Workspace, WorkspaceAttachRequest},
    },
};

pub(crate) struct SharedScan<T> {
    ttl: Duration,
    state: Mutex<ScanState<T>>,
    scan_lock: AsyncMutex<()>,
}

struct ScanState<T> {
    generation: u64,
    result: Option<CachedScan<T>>,
}

struct CachedScan<T> {
    scanned_at: Instant,
    result: T,
}

impl<T: Clone> SharedScan<T> {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self { ttl, state: Mutex::new(ScanState { generation: 0, result: None }), scan_lock: AsyncMutex::new(()) }
    }

    pub(crate) async fn get_or_scan<F, Fut>(&self, scan: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        if let Some(result) = self.fresh_result() {
            return Ok(result);
        }

        let _guard = self.scan_lock.lock().await;
        if let Some(result) = self.fresh_result() {
            return Ok(result);
        }

        let generation = self.state.lock().expect("shared scan state lock poisoned").generation;
        let result = scan().await?;
        let mut state = self.state.lock().expect("shared scan state lock poisoned");
        if state.generation == generation {
            state.result = Some(CachedScan { scanned_at: Instant::now(), result: result.clone() });
        }
        Ok(result)
    }

    pub(crate) fn invalidate(&self) {
        let mut state = self.state.lock().expect("shared scan state lock poisoned");
        state.generation = state.generation.wrapping_add(1);
        state.result = None;
    }

    #[cfg(test)]
    pub(crate) fn seed(&self, value: T) {
        self.state.lock().expect("shared scan state lock poisoned").result = Some(CachedScan { scanned_at: Instant::now(), result: value });
    }

    fn fresh_result(&self) -> Option<T> {
        self.state
            .lock()
            .expect("shared scan state lock poisoned")
            .result
            .as_ref()
            .filter(|cached| cached.scanned_at.elapsed() < self.ttl)
            .map(|cached| cached.result.clone())
    }
}

pub(crate) struct SharedPresentationManager {
    inner: Arc<dyn PresentationManager>,
    workspaces: SharedScan<Vec<(String, Workspace)>>,
}

impl SharedPresentationManager {
    pub(crate) fn new(inner: Arc<dyn PresentationManager>, ttl: Duration) -> Self {
        Self { inner, workspaces: SharedScan::new(ttl) }
    }
}

#[async_trait]
impl PresentationManager for SharedPresentationManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        self.workspaces.get_or_scan(|| self.inner.list_workspaces()).await
    }

    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        let result = self.inner.create_workspace(config).await;
        if result.is_ok() {
            self.workspaces.invalidate();
        }
        result
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.inner.select_workspace(ws_ref).await
    }

    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String> {
        let result = self.inner.delete_workspace(ws_ref).await;
        if result.is_ok() {
            self.workspaces.invalidate();
        }
        result
    }

    fn binding_scope_prefix(&self) -> String {
        self.inner.binding_scope_prefix()
    }
}

pub(crate) struct SharedTerminalPool {
    inner: Arc<dyn TerminalPool>,
    sessions: SharedScan<Vec<TerminalSession>>,
}

impl SharedTerminalPool {
    pub(crate) fn new(inner: Arc<dyn TerminalPool>, ttl: Duration) -> Self {
        Self { inner, sessions: SharedScan::new(ttl) }
    }
}

#[async_trait]
impl TerminalPool for SharedTerminalPool {
    fn tracks_session_liveness(&self) -> bool {
        self.inner.tracks_session_liveness()
    }

    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        self.sessions.get_or_scan(|| self.inner.list_sessions()).await
    }

    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<(), String> {
        let result = self.inner.ensure_session(session_name, command, cwd, env_vars).await;
        if result.is_ok() {
            self.sessions.invalidate();
        }
        result
    }

    fn attach_args(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        self.inner.attach_args(session_name, command, cwd, env_vars)
    }

    async fn attach_command(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<String, String> {
        self.inner.attach_command(session_name, command, cwd, env_vars).await
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        let result = self.inner.kill_session(session_name).await;
        if result.is_ok() {
            self.sessions.invalidate();
        }
        result
    }

    async fn deliver(&self, session_name: &str, text: &str, submit: bool) -> Result<(), String> {
        self.inner.deliver(session_name, text, submit).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Mutex;

    use super::*;

    #[tokio::test]
    async fn failed_scans_are_retried_instead_of_cached() {
        let scan = SharedScan::new(Duration::from_secs(10));
        let calls = AtomicUsize::new(0);

        let first = scan
            .get_or_scan(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<usize, _>("transient failure".to_string())
            })
            .await;
        let second = scan
            .get_or_scan(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            })
            .await;

        assert_eq!(first, Err("transient failure".to_string()));
        assert_eq!(second, Ok(42));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidation_during_a_scan_prevents_the_stale_result_from_being_cached() {
        let scan = Arc::new(SharedScan::new(Duration::from_secs(10)));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let in_flight = {
            let scan = Arc::clone(&scan);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                scan.get_or_scan(|| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(1)
                })
                .await
            })
        };

        started.notified().await;
        scan.invalidate();
        release.notify_one();
        assert_eq!(in_flight.await.expect("scan task"), Ok(1));
        assert_eq!(scan.get_or_scan(|| async { Ok(2) }).await, Ok(2));
    }

    struct CountingPresentationManager {
        calls: AtomicUsize,
        workspaces: Mutex<Vec<(String, Workspace)>>,
    }

    impl CountingPresentationManager {
        fn new() -> Self {
            Self { calls: AtomicUsize::new(0), workspaces: Mutex::new(vec![]) }
        }
    }

    #[async_trait]
    impl PresentationManager for CountingPresentationManager {
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(self.workspaces.lock().await.clone())
        }

        async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
            let workspace = Workspace { name: config.name.clone(), correlation_keys: vec![], attachable_set_id: None };
            let entry = (format!("workspace:{}", config.name), workspace);
            self.workspaces.lock().await.push(entry.clone());
            Ok(entry)
        }

        async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
            Ok(())
        }

        async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String> {
            self.workspaces.lock().await.retain(|(candidate, _)| candidate != ws_ref);
            Ok(())
        }

        fn binding_scope_prefix(&self) -> String {
            String::new()
        }
    }

    #[tokio::test]
    async fn concurrent_workspace_reads_share_one_scan_and_mutations_invalidate_it() {
        let inner = Arc::new(CountingPresentationManager::new());
        let manager = SharedPresentationManager::new(inner.clone(), Duration::from_secs(10));

        let (first, second) = tokio::join!(manager.list_workspaces(), manager.list_workspaces());
        assert!(first.expect("first workspace scan").is_empty());
        assert!(second.expect("second workspace scan").is_empty());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1, "concurrent readers should share the underlying scan");

        let request = WorkspaceAttachRequest {
            name: "new".into(),
            working_directory: ExecutionEnvironmentPath::new("/repo"),
            attach_commands: vec![],
            stamp: None,
            template_yaml: None,
            template_vars: Default::default(),
        };
        manager.create_workspace(&request).await.expect("create workspace");
        let workspaces = manager.list_workspaces().await.expect("scan after mutation");

        assert_eq!(workspaces.len(), 1);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2, "a successful mutation should invalidate the shared result");
    }
}
