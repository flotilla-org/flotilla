//! Agent harness adapters, hook handling, agent state and process-exit receipts.
pub mod account_access;
pub mod adapter;
pub mod attachable;
pub mod hooks;
pub mod process;
pub mod store;

pub use hooks::{
    claude_code_hook_entries, claude_code_hook_entries_over, claude_code_hook_settings, claude_code_hook_settings_for,
    claude_code_hook_settings_over, codex_hook_overrides, parser_for_harness, update_codex_hooks, ClaudeCodeHookTransport,
    HarnessHookParser, ParsedHookEvent, CLAUDE_CODE_HOOK_COMMAND_PREFIX, CLAUDE_CODE_HOOK_TOKEN_ENV, CLAUDE_CODE_HOOK_URL_ENV,
    CODEX_NOTIFY_COMMAND, FLOTILLA_CLI_ENV,
};

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

#[cfg(test)]
mod testing;

pub mod brief_artifact;
