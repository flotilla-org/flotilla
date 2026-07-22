pub mod cleat;
pub mod passthrough;
pub mod shpool;

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, AttachableId, AttachableSetId, TerminalStatus};
pub use flotilla_resources::TerminalSessionTag;

use crate::path_context::ExecutionEnvironmentPath;

/// Environment variables to inject into the terminal session.
pub type TerminalEnvVars = Vec<(String, String)>;

/// Raw session data returned by a terminal pool CLI adapter.
/// No AttachableId — the manager handles identity mapping.
#[derive(Debug, Clone, bon::Builder)]
pub struct TerminalSession {
    pub session_name: String,
    pub status: TerminalStatus,
    pub command: Option<String>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub screen_activity: Option<ScreenActivity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenActivity {
    Active,
    Stable,
}

pub(crate) const MANAGED_SESSION_PREFIX: &str = "flotilla-v2:";

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ManagedSessionMetadata {
    pub set_id: AttachableSetId,
    pub attachable_id: AttachableId,
    pub checkout: String,
    pub role: String,
    pub index: u32,
    pub working_directory: ExecutionEnvironmentPath,
}

pub(crate) fn managed_session_name(metadata: &ManagedSessionMetadata) -> String {
    let encode = |value: &str| urlencoding::encode(value).into_owned();
    format!(
        "{MANAGED_SESSION_PREFIX}{}:{}:{}:{}:{}:{}",
        encode(metadata.set_id.as_str()),
        encode(metadata.attachable_id.as_str()),
        encode(&metadata.checkout),
        encode(&metadata.role),
        metadata.index,
        encode(&metadata.working_directory.as_path().to_string_lossy()),
    )
}

pub(crate) fn parse_managed_session_name(session_name: &str) -> Option<ManagedSessionMetadata> {
    let mut parts = session_name.strip_prefix(MANAGED_SESSION_PREFIX)?.split(':');
    let decode = |value: &str| urlencoding::decode(value).ok().map(|value| value.into_owned());
    let set_id = AttachableSetId::new(decode(parts.next()?)?);
    let attachable_id = AttachableId::new(decode(parts.next()?)?);
    let checkout = decode(parts.next()?)?;
    let role = decode(parts.next()?)?;
    let index = parts.next()?.parse().ok()?;
    let working_directory = ExecutionEnvironmentPath::new(decode(parts.next()?)?);
    parts.next().is_none().then(|| {
        ManagedSessionMetadata::builder()
            .set_id(set_id)
            .attachable_id(attachable_id)
            .checkout(checkout)
            .role(role)
            .index(index)
            .working_directory(working_directory)
            .build()
    })
}

/// Pure CLI adapter for terminal session management.
/// Session names are opaque provider strings. Pools that support discovery may
/// derive a self-describing name from `ManagedSessionMetadata`.
/// No store, no identity management — the `TerminalManager` handles those concerns.
#[async_trait]
pub trait TerminalPool: Send + Sync {
    /// Returns a provider-safe, self-describing session name when the pool can
    /// rediscover managed sessions after the local registry is lost.
    fn managed_session_name(&self, _metadata: &ManagedSessionMetadata) -> Option<String> {
        None
    }

    fn tracks_session_liveness(&self) -> bool {
        false
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

    /// Deliver text to a running session. This trait grows only for concrete
    /// flotilla consumers; it is not intended to mirror a pool backend's API.
    async fn deliver(&self, _session_name: &str, _text: &str, _submit: bool) -> Result<(), String> {
        Err("terminal pool does not support delivery".to_string())
    }
}
