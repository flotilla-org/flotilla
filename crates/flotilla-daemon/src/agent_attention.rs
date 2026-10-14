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

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use flotilla_resources::{
        InputMeta, Selector, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalBrief, TerminalCrewContext,
        TerminalSession, TerminalSessionDegradedCondition, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec,
        TerminalSessionStatus,
    };
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;

    use super::observe_hook_attention;

    const NAMESPACE: &str = "flotilla";

    /// A running crew session whose recorded hook attention is newer than any
    /// incoming observation, and which carries a non-delivery degraded message.
    async fn session_with_newer_attention(backend: &ResourceBackend) -> TerminalSessionStatus {
        let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
        let created = sessions
            .create(
                &InputMeta::builder().name("crew".to_string()).build(),
                &TerminalSessionSpec {
                    env_ref: "env".into(),
                    role: "coder".into(),
                    source: TerminalSessionSource::Agent {
                        selector: Selector { capability: "code".into(), adapter: None, model: None },
                        brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "work".into(), copies: Vec::new() },
                        context: Box::new(TerminalCrewContext { namespace: NAMESPACE.into(), convoy: "c".into(), vessel_ref: "v".into() }),
                        message: None,
                    },
                    cwd: "/workspace".into(),
                    env: Default::default(),
                    pool: "test".into(),
                },
            )
            .await
            .expect("create session");
        let status = TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                as_of: Utc::now() + Duration::hours(1),
                source: TerminalAttentionSource::Hook,
            }),
            message: Some("pane unreadable".into()),
            degraded: Some(TerminalSessionDegradedCondition {
                reason: "ObservationFailed".into(),
                message: "pane unreadable".into(),
                message_id: None,
                consecutive_failures: 2,
                observed_at: Utc::now(),
            }),
            ..TerminalSessionStatus::default()
        };
        sessions.update_status("crew", &created.metadata.resource_version, &status).await.expect("status");
        status
    }

    async fn status(backend: &ResourceBackend) -> TerminalSessionStatus {
        backend.clone().using::<TerminalSession>(NAMESPACE).get("crew").await.expect("session").status.expect("status")
    }

    // Behaviour (kept from the socket handler it was extracted from): a plain
    // hook state change the session would not record writes nothing, so it
    // neither replaces attention nor clears a non-delivery degraded message.
    #[tokio::test]
    async fn a_rejected_state_change_leaves_the_session_untouched() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let before = session_with_newer_attention(&backend).await;
        observe_hook_attention(&backend, NAMESPACE, "crew", TerminalAttentionState::Idle, false).await.expect("observe");
        let after = status(&backend).await;
        assert_eq!(after.attention, before.attention);
        assert_eq!(after.degraded, before.degraded);
        assert_eq!(after.message, before.message);
    }

    // Behaviour: tool activity is always recorded, as the handler did; it
    // marks the activity and clears the non-delivery degraded message even
    // when the attention itself is not replaced.
    #[tokio::test]
    async fn tool_activity_is_recorded_even_when_attention_is_not_replaced() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let before = session_with_newer_attention(&backend).await;
        observe_hook_attention(&backend, NAMESPACE, "crew", TerminalAttentionState::Working, true).await.expect("observe");
        let after = status(&backend).await;
        assert!(after.last_tool_activity_at.is_some());
        assert_eq!(after.attention, before.attention, "older attention does not replace newer");
        assert_eq!((after.degraded, after.message), (None, None));
    }
}
