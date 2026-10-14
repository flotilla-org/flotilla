//! Crew attention reported by harness hooks, recorded on the crew's
//! TerminalSession. Shared by the agent-hook socket request and by in-process
//! harnesses (the dummy fleet's crews) that report the same events.

use flotilla_resources::{TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession, TerminalSessionStatusPatch};
use flotilla_store::ResourceBackend;

/// Record a hook's attention observation. A tool-activity event is always
/// recorded; a plain state change only when the session would accept it.
/// The early check only skips a write that would change nothing: the patch
/// re-applies `should_record_attention` to the current status on every
/// optimistic retry, so concurrent hooks cannot record stale attention.
pub(crate) async fn observe_hook_attention(
    backend: &ResourceBackend,
    namespace: &str,
    session_name: &str,
    state: TerminalAttentionState,
    tool_activity: bool,
) -> Result<(), String> {
    let sessions = backend.clone().using::<TerminalSession>(namespace);
    let session = sessions.get(session_name).await.map_err(|error| error.to_string())?;
    let attention = TerminalAttention { state, as_of: chrono::Utc::now(), source: TerminalAttentionSource::Hook };
    if !tool_activity && session.status.as_ref().is_some_and(|status| !status.should_record_attention(&attention)) {
        return Ok(());
    }
    let patch = if tool_activity {
        TerminalSessionStatusPatch::ObserveToolActivity { attention }
    } else {
        TerminalSessionStatusPatch::ObserveAttention { attention }
    };
    flotilla_store::apply_status_patch(&sessions, session_name, &patch).await.map_err(|error| error.to_string())?;
    Ok(())
}
