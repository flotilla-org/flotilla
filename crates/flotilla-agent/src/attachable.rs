// Agent hooks retain this identity until AgentSession replaces it.
pub use flotilla_protocol::AttachableId;

/// Resolve the temporary Codex hook identity without introducing AgentSession.
/// Retire this shim with the legacy terminal/attachable model tracked by #2912.
pub fn codex_hook_id(explicit: Option<&str>, terminal: Option<&flotilla_protocol::AgentHookTerminalRef>) -> AttachableId {
    if let Some(terminal) = terminal {
        return AttachableId::new(format!("terminal:{}/{}", terminal.namespace, terminal.session_name));
    }
    explicit.filter(|id| !id.is_empty()).map(AttachableId::new).unwrap_or_else(crate::allocate_attachable_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::AgentHookTerminalRef;

    // #3028: contained launch environments carry a TerminalSession reference,
    // without the retired FLOTILLA_ATTACHABLE_ID. Every notify must reuse it.
    #[hegel::test]
    fn codex_notifications_keep_terminal_identity(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(2).max_value(6));
        let terminal = AgentHookTerminalRef { namespace: "flotilla".into(), session_name: "terminal-work-coder".into() };
        let first = codex_hook_id(None, Some(&terminal));
        for _ in 1..count {
            assert_eq!(codex_hook_id(None, Some(&terminal)), first);
            assert_eq!(codex_hook_id(Some(""), Some(&terminal)), first);
            assert_eq!(codex_hook_id(Some("obsolete-attachable"), Some(&terminal)), first);
        }
        let other = AgentHookTerminalRef { namespace: "other".into(), ..terminal.clone() };
        assert_ne!(codex_hook_id(None, Some(&other)), first);
        assert_eq!(codex_hook_id(Some("legacy"), None), AttachableId::new("legacy"));
        assert_ne!(codex_hook_id(None, None), codex_hook_id(None, None));
    }
}
