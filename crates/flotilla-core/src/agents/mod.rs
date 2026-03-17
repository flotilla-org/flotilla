pub mod hooks;
pub mod store;

pub use hooks::{parser_for_harness, HarnessHookParser, ParsedHookEvent};

pub fn allocate_attachable_id() -> flotilla_protocol::AttachableId {
    flotilla_protocol::AttachableId::new(uuid::Uuid::new_v4().to_string())
}

pub use store::{
    shared_agent_state_store, shared_file_backed_agent_state_store, shared_in_memory_agent_state_store, AgentEntry, AgentRegistry,
    AgentStateStore, AgentStateStoreApi, InMemoryAgentStateStore, SharedAgentStateStore,
};
