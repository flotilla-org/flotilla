pub mod store;
pub mod types;

pub use store::{
    shared_attachable_store, shared_file_backed_attachable_store, shared_in_memory_attachable_store, AttachableRegistry, AttachableStore,
    AttachableStoreApi, InMemoryAttachableStore, RemovedSetInfo, SharedAttachableStore,
};
pub use types::{
    Attachable, AttachableContent, AttachableId, AttachableSet, AttachableSetId, BindingObjectKind, ProviderBinding, TerminalAttachable,
    TerminalPurpose,
};

pub const TERMINAL_SESSION_BINDING_PREFIX: &str = "flotilla/";

pub fn terminal_session_binding_ref(id: &flotilla_protocol::ManagedTerminalId) -> String {
    format!("{TERMINAL_SESSION_BINDING_PREFIX}{id}")
}

pub fn parse_terminal_session_binding_ref(session_ref: &str) -> Option<flotilla_protocol::ManagedTerminalId> {
    let rest = session_ref.strip_prefix(TERMINAL_SESSION_BINDING_PREFIX)?;
    let (before_index, index_str) = rest.rsplit_once('/')?;
    let (checkout, role) = before_index.rsplit_once('/')?;
    let index = index_str.parse().ok()?;
    Some(flotilla_protocol::ManagedTerminalId { checkout: checkout.into(), role: role.into(), index })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_terminal_session_binding_ref_roundtrips() {
        let id = flotilla_protocol::ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
        let session_ref = terminal_session_binding_ref(&id);
        let parsed = parse_terminal_session_binding_ref(&session_ref).expect("should parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_terminal_session_binding_ref_with_slashy_checkout() {
        let id = flotilla_protocol::ManagedTerminalId { checkout: "feature/deep/nested".into(), role: "agent".into(), index: 1 };
        let session_ref = terminal_session_binding_ref(&id);
        let parsed = parse_terminal_session_binding_ref(&session_ref).expect("should parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_terminal_session_binding_ref_rejects_invalid() {
        assert!(parse_terminal_session_binding_ref("not-a-session").is_none());
        assert!(parse_terminal_session_binding_ref("flotilla/").is_none());
        assert!(parse_terminal_session_binding_ref("flotilla/only-one-segment").is_none());
    }
}
