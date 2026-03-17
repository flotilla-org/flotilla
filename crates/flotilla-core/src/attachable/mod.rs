pub mod store;
pub mod types;

pub use store::{
    shared_attachable_store, shared_file_backed_attachable_store, shared_in_memory_attachable_store, AttachableRegistry, AttachableStore,
    AttachableStoreApi, InMemoryAttachableStore, SharedAttachableStore,
};
pub use types::{
    Attachable, AttachableContent, AttachableId, AttachableSet, AttachableSetId, BindingObjectKind, ProviderBinding, TerminalAttachable,
    TerminalPurpose,
};

pub const TERMINAL_SESSION_BINDING_PREFIX: &str = "flotilla/";

pub fn terminal_session_binding_ref(id: &flotilla_protocol::ManagedTerminalId) -> String {
    format!("{TERMINAL_SESSION_BINDING_PREFIX}{id}")
}
