//! Which Presentation Manager encloses this process — detection and the
//! per-PM knowledge producers need to speak to it.
//!
//! This is the only module that knows PM-specific environment spellings
//! (`WHEELHOUSE_*`) or which transport a PM uses. Producers hold a
//! [`PmInstance`] and ask it for a [`PatchSink`] and for process-local facts
//! (the enclosing pane); they never branch on PM kind themselves — the
//! manifest architecture's "producers swap only their send function".

use std::{path::PathBuf, sync::Arc};

use crate::{
    sink::{PatchSink, WheelhouseHttpSink},
    stamp::parse_pane_id,
    wire::PaneTarget,
};

/// A detected (or explicitly configured) Presentation Manager instance.
pub enum PmInstance {
    /// Wheelhouse accepts HTTP metadata patches at `WHEELHOUSE_SOCKET`: a Unix
    /// socket path on Unix, or a named-pipe path (`\\.\pipe\<name>`) on Windows.
    /// A Unix-style path is not translated on Windows: it is passed literally
    /// to the named-pipe API and connection failure is reported as a Wheelhouse
    /// metadata POST error after the normal transport retry. Use `\\.\pipe\<name>`.
    /// The contract is owned by wheelhouse/docs/protocol/pm-connect.md (#22).
    /// `WHEELHOUSE_PANE_ID` remains provisional until native pane identity is
    /// generalized; the current Wheelhouse host does not export it.
    Wheelhouse {
        socket: PathBuf,
        /// The pane this process runs in, when wheelhouse exposes it.
        pane: Option<PaneTarget>,
    },
}

impl PmInstance {
    /// Detect the enclosing PM from the process environment (injected for
    /// tests): `WHEELHOUSE_SOCKET` means a wheelhouse instance.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Option<PmInstance> {
        let socket = env("WHEELHOUSE_SOCKET")?;
        Some(PmInstance::Wheelhouse {
            socket: PathBuf::from(socket),
            pane: env("WHEELHOUSE_PANE_ID").as_deref().and_then(parse_pane_id).map(PaneTarget::Terminal),
        })
    }

    pub fn wheelhouse(socket: impl Into<PathBuf>) -> PmInstance {
        PmInstance::Wheelhouse { socket: socket.into(), pane: None }
    }

    /// The transport patches travel over.
    pub fn sink(&self) -> Arc<dyn PatchSink> {
        match self {
            PmInstance::Wheelhouse { socket, .. } => Arc::new(WheelhouseHttpSink::new(socket.clone())),
        }
    }

    /// The pane this process runs in, for Pane-target stamping.
    pub fn current_pane(&self) -> Option<PaneTarget> {
        match self {
            PmInstance::Wheelhouse { pane, .. } => *pane,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pm_environment_detects_nothing() {
        assert!(PmInstance::detect(&|_| None).is_none());
    }

    #[test]
    fn detects_wheelhouse_socket_and_pane() {
        let env = |key: &str| match key {
            "WHEELHOUSE_SOCKET" => Some("/tmp/wheelhouse.sock".to_owned()),
            "WHEELHOUSE_PANE_ID" => Some("9".to_owned()),
            _ => None,
        };
        let pm = PmInstance::detect(&env).expect("wheelhouse detected");
        assert!(matches!(pm, PmInstance::Wheelhouse { .. }));
        assert_eq!(pm.current_pane(), Some(PaneTarget::Terminal(9)), "attach can stamp a wheelhouse pane");
    }
}
