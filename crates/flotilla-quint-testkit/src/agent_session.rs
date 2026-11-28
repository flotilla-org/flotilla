//! A request for tests that create AgentSessions and need no particular convoy.
use flotilla_schema::Ref;
use flotilla_state_machines::machines::agent_session::AgentSessionRequest;

/// A `coder` session of convoy `convoy` homed at `home`, on no vessel.
pub fn agent_session_request() -> AgentSessionRequest {
    AgentSessionRequest { convoy: Ref::new("convoy", "home"), role: "coder".into(), vessel: None }
}
