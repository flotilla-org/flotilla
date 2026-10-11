use crate::providers::terminal::{TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionLiveness, TerminalSize};
use async_trait::async_trait;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::commands::AttachMode;
use flotilla_provider_runtime::scan_cache::SharedScan;
use std::{sync::Arc, time::Duration};

/// Stays in core because it implements the core-owned TerminalPool provider trait.
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
