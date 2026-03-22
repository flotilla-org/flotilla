pub mod cleat;
pub mod passthrough;
pub mod shpool;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use flotilla_protocol::TerminalStatus;

/// Environment variables to inject into the terminal session.
pub type TerminalEnvVars = Vec<(String, String)>;

/// Raw session data returned by a terminal pool CLI adapter.
/// No AttachableId — the manager handles identity mapping.
#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub session_name: String,
    pub status: TerminalStatus,
    pub command: Option<String>,
    pub working_directory: Option<PathBuf>,
}

/// Pure CLI adapter for terminal session management.
/// Session names are opaque strings (AttachableIds in practice).
/// No store, no identity management — the `TerminalManager` handles those concerns.
#[async_trait]
pub trait TerminalPool: Send + Sync {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String>;
    async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String>;
    async fn attach_command(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<String, String>;
    async fn kill_session(&self, session_name: &str) -> Result<(), String>;
}
