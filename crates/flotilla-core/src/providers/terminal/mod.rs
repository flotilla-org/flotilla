pub mod cleat;
pub mod passthrough;
pub mod shpool;

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, TerminalStatus};

use crate::path_context::ExecutionEnvironmentPath;

/// Environment variables to inject into the terminal session.
pub type TerminalEnvVars = Vec<(String, String)>;

/// Raw session data returned by a terminal pool CLI adapter.
/// No AttachableId — the manager handles identity mapping.
#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub session_name: String,
    pub status: TerminalStatus,
    pub command: Option<String>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
}

/// Pure CLI adapter for terminal session management.
/// Session names are opaque strings (AttachableIds in practice).
/// No store, no identity management — the `TerminalManager` handles those concerns.
#[async_trait]
pub trait TerminalPool: Send + Sync {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String>;
    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<(), String>;

    /// Returns a structured `Arg` tree representing the attach command.
    /// Callers that need a flat string can use `flatten(&args, 0)`.
    fn attach_args(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String>;

    /// Returns the attach command as a flat shell string.
    /// Default implementation calls `attach_args()` + `flatten()`.
    async fn attach_command(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<String, String> {
        let args = self.attach_args(session_name, command, cwd, env_vars)?;
        Ok(flotilla_protocol::arg::flatten(&args, 0))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String>;
}
