//! Which Presentation Manager encloses this process — detection and the
//! per-PM knowledge producers need to speak to it.
//!
//! This is the only module that knows PM-specific environment spellings
//! (`ZELLIJ*`, …) or which transport a PM uses. Producers hold a
//! [`PmInstance`] and ask it for a [`PatchSink`] and for process-local facts
//! (the enclosing pane); they never branch on PM kind themselves — the
//! manifest architecture's "producers swap only their send function".

use std::{path::PathBuf, sync::Arc};

use crate::{
    sink::{PatchSink, UnixSocketSink, ZellijPipeSink},
    stamp::parse_zellij_pane_id,
    wire::PaneTarget,
};

/// A detected (or explicitly configured) Presentation Manager instance.
pub enum PmInstance {
    Zellij {
        bin: String,
        plugin_url: Option<String>,
        /// The pane this process runs in, when the PM exposes it.
        pane: Option<PaneTarget>,
    },
    /// Wheelhouse listens on a unix socket. No environment detection yet —
    /// the listener contract is Leg 3 of the manifest extraction; until it
    /// lands the socket is always explicit configuration.
    Wheelhouse { socket: PathBuf },
}

impl PmInstance {
    /// Detect the enclosing PM from the process environment (injected for
    /// tests). `ZELLIJ` set means we run inside a zellij session.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Option<PmInstance> {
        env("ZELLIJ")?;
        Some(PmInstance::Zellij {
            bin: env("ZELLIJ_BIN").unwrap_or_else(|| "zellij".to_owned()),
            plugin_url: None,
            pane: env("ZELLIJ_PANE_ID").as_deref().and_then(parse_zellij_pane_id).map(PaneTarget::Terminal),
        })
    }

    pub fn wheelhouse(socket: impl Into<PathBuf>) -> PmInstance {
        PmInstance::Wheelhouse { socket: socket.into() }
    }

    /// Override the zellij binary used for pipe delivery. No-op for PMs that
    /// don't shell out.
    pub fn with_zellij_bin(mut self, zellij_bin: Option<String>) -> PmInstance {
        if let (PmInstance::Zellij { bin, .. }, Some(zellij_bin)) = (&mut self, zellij_bin) {
            *bin = zellij_bin;
        }
        self
    }

    /// Restrict pipe delivery to one plugin instance. No-op for PMs without
    /// broadcast pipes.
    pub fn with_plugin_url(mut self, url: Option<String>) -> PmInstance {
        if let (PmInstance::Zellij { plugin_url, .. }, Some(url)) = (&mut self, url) {
            *plugin_url = Some(url);
        }
        self
    }

    /// The transport patches travel over.
    pub fn sink(&self) -> Arc<dyn PatchSink> {
        match self {
            PmInstance::Zellij { bin, plugin_url, .. } => {
                let mut sink = ZellijPipeSink::new(bin.clone());
                if let Some(url) = plugin_url {
                    sink = sink.with_plugin_url(url.clone());
                }
                Arc::new(sink)
            }
            PmInstance::Wheelhouse { socket } => Arc::new(UnixSocketSink::new(socket.clone())),
        }
    }

    /// The pane this process runs in, for Pane-target stamping.
    pub fn current_pane(&self) -> Option<PaneTarget> {
        match self {
            PmInstance::Zellij { pane, .. } => *pane,
            PmInstance::Wheelhouse { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zellij_env(key: &str) -> Option<String> {
        match key {
            "ZELLIJ" => Some("0".to_owned()),
            "ZELLIJ_PANE_ID" => Some("42".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn detects_zellij_with_pane_and_default_bin() {
        let pm = PmInstance::detect(&zellij_env).expect("zellij detected");
        assert_eq!(pm.current_pane(), Some(PaneTarget::Terminal(42)));
        let PmInstance::Zellij { bin, .. } = &pm else {
            panic!("expected zellij");
        };
        assert_eq!(bin, "zellij");
    }

    #[test]
    fn no_pm_environment_detects_nothing() {
        assert!(PmInstance::detect(&|_| None).is_none());
    }

    #[test]
    fn overrides_apply_only_to_zellij() {
        let pm = PmInstance::detect(&zellij_env).expect("zellij detected").with_zellij_bin(Some("/opt/zellij".to_owned()));
        let PmInstance::Zellij { bin, .. } = &pm else {
            panic!("expected zellij");
        };
        assert_eq!(bin, "/opt/zellij");

        let wheelhouse = PmInstance::wheelhouse("/tmp/wheelhouse.sock").with_zellij_bin(Some("/opt/zellij".to_owned()));
        assert!(wheelhouse.current_pane().is_none());
        assert!(matches!(wheelhouse, PmInstance::Wheelhouse { .. }));
    }
}
