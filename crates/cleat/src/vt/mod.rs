pub mod passthrough;

#[cfg(feature = "ghostty-vt")]
pub mod ghostty;
#[cfg(feature = "ghostty-vt")]
mod ghostty_ffi;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ClientCapabilities {
    pub color_level: ColorLevel,
    pub kitty_keyboard: bool,
}

impl ClientCapabilities {
    pub fn new(color_level: ColorLevel, kitty_keyboard: bool) -> Self {
        Self { color_level, kitty_keyboard }
    }

    pub fn conservative_fallback() -> Self {
        Self::new(ColorLevel::Sixteen, false)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ColorLevel {
    Sixteen,
    Ansi256,
    #[default]
    TrueColor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum VtEngineKind {
    Passthrough,
    Ghostty,
}

impl VtEngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::Ghostty => "ghostty",
        }
    }

    pub fn ensure_available(self) -> Result<(), String> {
        match self {
            Self::Passthrough => Ok(()),
            Self::Ghostty => {
                #[cfg(feature = "ghostty-vt")]
                {
                    Ok(())
                }
                #[cfg(not(feature = "ghostty-vt"))]
                {
                    Err("vt engine ghostty is not compiled into this cleat build".to_string())
                }
            }
        }
    }
}

// NOTE: VtEngine is intentionally not Send. Engines may wrap foreign terminal-state
// handles that are only accessed from the single session daemon event loop.
pub trait VtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    fn supports_replay(&self) -> bool;
    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String>;
    fn screen_text(&self) -> Result<String, String>;
    fn size(&self) -> (u16, u16);
}

#[cfg(test)]
pub(crate) fn make_default_vt_engine(cols: u16, rows: u16) -> Box<dyn VtEngine> {
    make_vt_engine(default_vt_engine_kind(), cols, rows).expect("default vt engine should always be available")
}

pub(crate) fn make_vt_engine(kind: VtEngineKind, cols: u16, rows: u16) -> Result<Box<dyn VtEngine>, String> {
    kind.ensure_available()?;
    Ok(select_vt_engine(kind, cols, rows))
}

pub fn default_vt_engine_kind() -> VtEngineKind {
    select_default_vt_engine_kind()
}

#[cfg(feature = "ghostty-vt")]
fn select_vt_engine(kind: VtEngineKind, cols: u16, rows: u16) -> Box<dyn VtEngine> {
    match kind {
        VtEngineKind::Passthrough => Box::new(passthrough::PassthroughVtEngine::new(cols, rows)),
        VtEngineKind::Ghostty => Box::new(ghostty::GhosttyVtEngine::new(cols, rows)),
    }
}

#[cfg(feature = "ghostty-vt")]
fn select_default_vt_engine_kind() -> VtEngineKind {
    VtEngineKind::Ghostty
}

#[cfg(not(feature = "ghostty-vt"))]
fn select_vt_engine(kind: VtEngineKind, cols: u16, rows: u16) -> Box<dyn VtEngine> {
    match kind {
        VtEngineKind::Passthrough => Box::new(passthrough::PassthroughVtEngine::new(cols, rows)),
        VtEngineKind::Ghostty => unreachable!("availability check should reject ghostty when feature-disabled"),
    }
}

#[cfg(not(feature = "ghostty-vt"))]
fn select_default_vt_engine_kind() -> VtEngineKind {
    VtEngineKind::Passthrough
}

#[cfg(test)]
mod tests {
    use super::VtEngineKind;

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_smoke_constructs_resizes_and_drops() {
        let mut engine = super::make_default_vt_engine(80, 24);

        assert_eq!(super::default_vt_engine_kind(), VtEngineKind::Ghostty);
        assert_eq!(engine.size(), (80, 24));

        engine.resize(120, 40).expect("resize ghostty engine");

        assert_eq!(engine.size(), (120, 40));
    }

    #[test]
    fn passthrough_engine_is_always_available() {
        assert!(super::make_vt_engine(VtEngineKind::Passthrough, 80, 24).is_ok());
    }

    #[cfg(not(feature = "ghostty-vt"))]
    #[test]
    fn ghostty_engine_is_rejected_when_feature_disabled() {
        let err = match super::make_vt_engine(VtEngineKind::Ghostty, 80, 24) {
            Ok(_) => panic!("ghostty should be unavailable"),
            Err(err) => err,
        };
        assert!(err.contains("not compiled"));
    }
}
