pub mod hooks;
pub mod store;

pub use hooks::{parser_for_harness, HarnessHookParser, ParsedHookEvent};

/// Allocate a fresh attachable ID for an agent in an unmanaged terminal.
///
/// Used when `FLOTILLA_ATTACHABLE_ID` is not set and no existing session_id
/// mapping exists. Callers should ensure the harness provides a `session_id`
/// so subsequent events for the same agent can be deduplicated via the
/// session index. Without a session_id, each hook call creates an orphaned
/// entry that has no `Ended` event to clean it up.
pub fn allocate_attachable_id() -> flotilla_protocol::AttachableId {
    flotilla_protocol::AttachableId::new(uuid::Uuid::new_v4().to_string())
}

pub use store::{
    shared_agent_state_store, shared_file_backed_agent_state_store, shared_in_memory_agent_state_store, AgentEntry, AgentRegistry,
    AgentStateStore, AgentStateStoreApi, InMemoryAgentStateStore, SharedAgentStateStore,
};
