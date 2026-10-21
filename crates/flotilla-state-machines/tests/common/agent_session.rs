//! Shared AgentSession state-to-ITF mapping for conformance and replay.
use flotilla_quint_testkit::itf::variant;
use flotilla_state_machines::machines::agent_session::{AgentSessionState, Channel, EndReason};
use serde_json::Value;

pub fn session_value(state: &AgentSessionState) -> Value {
    use flotilla_state_machines::machines::agent_session::Activity;
    let observation = state.observation().map_or_else(|| variant("Unobserved", serde_json::json!({"#tup":[]})), |o| {
        variant("Observed", serde_json::json!({
            "activity": variant(match o.activity { Activity::Working => "Working", Activity::Idle => "Idle", Activity::Interrupted => "Interrupted" }, serde_json::json!({"#tup":[]})),
            "since": serde_json::json!({"#bigint": (o.activity_since.0 / flotilla_quint_testkit::graph::UNIT).to_string()}),
            "cause": o.activity_cause,
        }))
    });
    match state {
        AgentSessionState::Live { channel, .. } => variant(
            "Live",
            serde_json::json!({
                "channel": variant(match channel { Channel::Detached => "Detached", Channel::Attached => "Attached" }, serde_json::json!({"#tup":[]})),
                "observation": observation,
            }),
        ),
        AgentSessionState::Ended { reason, .. } => variant(
            "Ended",
            serde_json::json!({
                "reason": variant(match reason { EndReason::Completed => "Completed", EndReason::Lost => "Lost" }, serde_json::json!({"#tup":[]})),
                "observation": observation,
            }),
        ),
    }
}
