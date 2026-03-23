use std::sync::Arc;

use flotilla_protocol::AttachableId;

use super::{ResolutionContext, ResolvedAction};
use crate::{
    attachable::{AttachableContent, SharedAttachableStore},
    providers::terminal::{TerminalEnvVars, TerminalPool},
};

/// Resolves a `Hop::AttachTerminal` into a terminal-attach action on the context.
pub trait TerminalHopResolver: Send + Sync {
    fn resolve(&self, attachable_id: &AttachableId, context: &mut ResolutionContext) -> Result<(), String>;
}

/// Resolves terminal hops by looking up the attachable in the store, building
/// env vars, and delegating to `TerminalPool::attach_args()`.
///
/// This mirrors the logic in `TerminalManager::attach_args()` but pushes the
/// result onto `ResolutionContext::actions` instead of returning directly.
pub struct PoolTerminalHopResolver {
    pool: Arc<dyn TerminalPool>,
    store: SharedAttachableStore,
    daemon_socket_path: Option<String>,
}

impl PoolTerminalHopResolver {
    pub fn new(pool: Arc<dyn TerminalPool>, store: SharedAttachableStore, daemon_socket_path: Option<String>) -> Self {
        Self { pool, store, daemon_socket_path }
    }
}

impl TerminalHopResolver for PoolTerminalHopResolver {
    fn resolve(&self, attachable_id: &AttachableId, context: &mut ResolutionContext) -> Result<(), String> {
        let (command, cwd) = {
            let store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
            let attachable =
                store.registry().attachables.get(attachable_id).ok_or_else(|| format!("attachable not found: {attachable_id}"))?;
            match &attachable.content {
                AttachableContent::Terminal(t) => (t.command.clone(), t.working_directory.clone()),
            }
        };

        let mut env_vars: TerminalEnvVars = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), attachable_id.to_string())];
        if let Some(socket) = &self.daemon_socket_path {
            env_vars.push(("FLOTILLA_DAEMON_SOCKET".to_string(), socket.clone()));
        }

        let session_name = attachable_id.to_string();
        let args = self.pool.attach_args(&session_name, &command, &cwd, &env_vars)?;
        context.actions.push(ResolvedAction::Command(args));
        Ok(())
    }
}

/// No-op terminal hop resolver that always errors. Used when the hop plan
/// contains no `AttachTerminal` hops (e.g. prepared command plans that only
/// have `RemoteToHost` + `RunCommand`).
pub struct NoopTerminalHopResolver;

impl TerminalHopResolver for NoopTerminalHopResolver {
    fn resolve(&self, attachable_id: &AttachableId, _context: &mut ResolutionContext) -> Result<(), String> {
        Err(format!("no terminal pool available to resolve attachable: {attachable_id}"))
    }
}
