pub mod cleat;
pub(crate) mod environment;
pub mod passthrough;

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, commands::AttachMode, result_set::CleatEndpoint, TerminalStatus};
pub use flotilla_resources::TerminalSessionTag;

use flotilla_paths::path_context::ExecutionEnvironmentPath;

/// Environment variables to inject into the terminal session.
pub type TerminalEnvVars = Vec<(String, String)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }
}

impl std::fmt::Display for TerminalSize {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}x{}", self.columns, self.rows)
    }
}

/// Raw session data returned by a terminal pool CLI adapter.
/// Session names are opaque; durable TerminalSession resources own identity.
#[derive(Debug, Clone, bon::Builder)]
pub struct TerminalSession {
    pub session_name: String,
    pub status: TerminalStatus,
    pub command: Option<String>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub screen_activity: Option<ScreenActivity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSessionLiveness {
    Running,
    Stopped,
    Absent,
    Lost(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenActivity {
    Active,
    Stable,
}

/// Pure CLI adapter for terminal session management.
/// Session names are opaque provider strings; durable TerminalSessions own identity.
#[async_trait]
pub trait TerminalPool: Send + Sync {
    /// Retain a lost recording after its replacement session has launched.
    async fn retain_recovered_recording(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
    /// Physical Cleat daemon hosting this session, when the pool can resolve it.
    async fn cleat_endpoint(&self, _session_id: &str) -> Result<Option<CleatEndpoint>, String> {
        Ok(None)
    }

    fn tracks_session_liveness(&self) -> bool {
        false
    }

    async fn session_liveness(&self, session_id: &str) -> Result<TerminalSessionLiveness, String> {
        if !self.tracks_session_liveness() {
            return Ok(TerminalSessionLiveness::Running);
        }
        Ok(if self.list_sessions().await?.iter().any(|session| session.session_name == session_id) {
            TerminalSessionLiveness::Running
        } else {
            TerminalSessionLiveness::Stopped
        })
    }

    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String>;
    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
    ) -> Result<(), String>;

    /// Ensure a session with an explicit initial terminal size when supported.
    /// Pools without launch-time sizing retain their normal behavior.
    async fn ensure_session_with_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
        _initial_size: Option<TerminalSize>,
    ) -> Result<(), String> {
        self.ensure_session(session_name, command, cwd, env_vars, tags).await
    }

    /// Returns a structured `Arg` tree representing the attach command.
    /// Callers that need a flat string can use `flatten(&args, 0)`.
    fn attach_args(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String>;

    /// Verify that the resolved pool supports the requested seat semantics.
    async fn preflight_attach(&self, mode: AttachMode) -> Result<(), String> {
        match mode {
            AttachMode::Default | AttachMode::PreferTake => Ok(()),
            AttachMode::Strict | AttachMode::Take => Err("terminal pool does not support controller-seat attach options".to_string()),
        }
    }

    /// Returns attach arguments for the requested controller-seat behavior.
    fn attach_args_for_mode(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        mode: AttachMode,
    ) -> Result<Vec<Arg>, String> {
        match mode {
            AttachMode::Default | AttachMode::PreferTake => self.attach_args(session_name, command, cwd, env_vars),
            AttachMode::Strict | AttachMode::Take => Err("terminal pool does not support controller-seat attach options".to_string()),
        }
    }

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

    /// Capture the currently rendered terminal screen when the pool has a
    /// functional VT engine. This is level-triggered observation for prompts
    /// that do not emit a harness hook event.
    async fn capture_screen(&self, _session_name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// Deliver and submit machinery text to a running agent session. This
    /// operation must use the pool's reliable TUI-composer submission path.
    async fn deliver(&self, _session_name: &str, _text: &str) -> Result<(), String> {
        Err("terminal pool does not support delivery".to_string())
    }

    /// Retry a delivery whose text is still present in a TUI composer. Pools
    /// with keyboard-level control should clear that text before resubmitting.
    async fn retry_delivery(&self, session_name: &str, text: &str) -> Result<(), String> {
        self.deliver(session_name, text).await
    }
}
