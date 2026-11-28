//! Crew attention reported by harness hooks, held in memory as what this home observes of the
//! crew's terminal launch, and its activity on the crew's AgentSession. Shared by the agent-hook
//! socket request and by in-process harnesses (the dummy fleet's crews) that report the same
//! events.

use std::sync::Arc;

use flotilla_engine_messages::session_request;
use flotilla_resources::{
    lifecycle_reads::LifecycleReads, terminal_records::TerminalRecords, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
};

/// The caller harness hooks' reads count for.
pub(crate) const AGENT_HOOKS: &str = "agent hooks";

/// Records a hook's attention on session `session_name`'s newest launch here, when it adds to
/// what is held. A native Codex hook also records that the harness reports native hooks. A tool
/// call is progress the daemon keeps in memory, not attention (#3108).
pub(crate) fn observe_hook_attention(
    terminals: &dyn TerminalRecords,
    session_name: &str,
    state: TerminalAttentionState,
    native_codex: bool,
) -> Result<(), String> {
    let launch = terminals
        .get(session_name, Some(terminals.home()))?
        .ok_or_else(|| format!("terminal session {session_name} has no launch here"))?;
    let adapter = launch.agent().map(|agent| agent.adapter.clone());
    let attention = TerminalAttention { state, as_of: flotilla_resources::clock::now(), source: TerminalAttentionSource::Hook };
    terminals.observations().update(&launch.name, |observation| {
        observation.codex_native_hooks |= native_codex;
        observation.observe_attention(attention, adapter.as_deref());
    });
    Ok(())
}

/// Record a hook's activity on the crew's AgentSession, the one session `session_name`'s newest
/// launch here declares, only when it changes: duplicate hooks stay off the store's writer. The
/// machine's gate remains authoritative across races.
pub(crate) async fn record_agent_activity(
    reads: Arc<dyn LifecycleReads>,
    primitives: &flotilla_engine_messages::MessagePrimitives,
    session_name: &str,
    activity: flotilla_state_machines::machines::agent_session::Activity,
    cause: &str,
) -> Result<(), String> {
    // A hook token can outlive its launch, so the launch is read rather than assumed: an
    // observation never creates an orphan record for a session with no launch here.
    let (session, launch_reads) = (session_name.to_owned(), Arc::clone(&reads));
    // A store read: off the async workers.
    let launch = tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || {
        let terminals = launch_reads.terminals()?;
        terminals.get(&session, Some(terminals.home()))
    }))
    .await
    .map_err(|error| error.to_string())??
    .ok_or_else(|| format!("terminal session {session_name} has no launch here"))?;
    let name = launch.crew_id().ok_or_else(|| format!("terminal launch {} is no agent's", launch.name))?.to_owned();
    if !primitives.sessions().needs_observation(&name, activity)? {
        return Ok(());
    }
    let sessions = Arc::clone(primitives.sessions());
    let cause = cause.to_owned();
    let now = flotilla_schema::Timestamp::from_datetime(&flotilla_resources::clock::now());
    tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || {
        if sessions.get(&name).is_none() {
            sessions.ensure(&name, session_request(reads.vessels()?, launch.vessel.as_deref(), &launch.role)?, now)?;
        }
        sessions.observe(&name, activity, cause, now)
    }))
    .await
    .map_err(|error| error.to_string())??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use flotilla_provisioning_testkit::TerminalsFixture;
    use flotilla_resources::{
        terminal_records::{TerminalObservation, TerminalRecords as _},
        TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
    };

    use super::observe_hook_attention;

    /// A running crew session whose observed hook attention is newer than any incoming hook.
    fn session_with_newer_attention(terminals: &TerminalsFixture) -> (String, Option<TerminalObservation>) {
        let launch = terminals.request(&TerminalsFixture::agent("crew-vessel", "coder", "env"));
        terminals.ready(&launch, "crew-session", &Default::default());
        terminals.terminals.observations().update(&launch, |observation| {
            observation.attention = Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                as_of: Utc::now() + Duration::hours(1),
                source: TerminalAttentionSource::Hook,
            });
        });
        let before = terminals.terminals.observations().get(&launch);
        (launch, before)
    }

    // Behaviour (kept from the socket handler it was extracted from): a plain
    // hook state change the session would not record changes nothing it observes.
    #[test]
    fn a_rejected_state_change_leaves_the_session_untouched() {
        let terminals = TerminalsFixture::default();
        let (launch, before) = session_with_newer_attention(&terminals);
        let session = terminals.record(&launch).session_name().to_owned();
        observe_hook_attention(terminals.terminals.as_ref(), &session, TerminalAttentionState::Idle, false).expect("observe");
        assert_eq!(terminals.terminals.observations().get(&launch), before);
    }
}
