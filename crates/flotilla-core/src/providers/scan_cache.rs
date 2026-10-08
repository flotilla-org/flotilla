use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::commands::AttachMode;
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::terminal::{TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionLiveness, TerminalSize},
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
    async fn session_liveness(&self, session_id: &str) -> Result<TerminalSessionLiveness, String> {
        // Attach needs the endpoint's current state, not the shared discovery snapshot.
        self.inner.session_liveness(session_id).await
    }

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
        tags: &[super::terminal::TerminalSessionTag],
    ) -> Result<(), String> {
        let result = self.inner.ensure_session(session_name, command, cwd, env_vars, tags).await;
        if result.is_ok() {
            self.sessions.invalidate();
        }
        result
    }

    async fn ensure_session_with_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[super::terminal::TerminalSessionTag],
        initial_size: Option<TerminalSize>,
    ) -> Result<(), String> {
        let result = self.inner.ensure_session_with_size(session_name, command, cwd, env_vars, tags, initial_size).await;
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

    async fn preflight_attach(&self, mode: AttachMode) -> Result<(), String> {
        self.inner.preflight_attach(mode).await
    }

    fn attach_args_for_mode(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        mode: AttachMode,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        self.inner.attach_args_for_mode(session_name, command, cwd, env_vars, mode)
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

    async fn deliver(&self, session_name: &str, text: &str) -> Result<(), String> {
        self.inner.deliver(session_name, text).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
