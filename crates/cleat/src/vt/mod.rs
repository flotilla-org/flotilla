pub mod passthrough;

#[cfg(feature = "ghostty-vt")]
pub mod ghostty;

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

pub trait VtEngine: Send {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    fn supports_replay(&self) -> bool;
    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String>;
    fn size(&self) -> (u16, u16);
}

pub(crate) fn make_default_vt_engine(cols: u16, rows: u16) -> Box<dyn VtEngine> {
    select_default_vt_engine(cols, rows)
}

#[cfg(test)]
pub(crate) fn default_vt_engine_kind() -> &'static str {
    select_default_vt_engine_kind()
}

#[cfg(feature = "ghostty-vt")]
fn select_default_vt_engine(cols: u16, rows: u16) -> Box<dyn VtEngine> {
    Box::new(ghostty::GhosttyVtEngine::new(cols, rows))
}

#[cfg(test)]
#[cfg(feature = "ghostty-vt")]
fn select_default_vt_engine_kind() -> &'static str {
    "ghostty"
}

#[cfg(not(feature = "ghostty-vt"))]
fn select_default_vt_engine(cols: u16, rows: u16) -> Box<dyn VtEngine> {
    Box::new(passthrough::PassthroughVtEngine::new(cols, rows))
}

#[cfg(test)]
#[cfg(not(feature = "ghostty-vt"))]
fn select_default_vt_engine_kind() -> &'static str {
    "passthrough"
}

#[cfg(test)]
mod tests {
    use super::{make_default_vt_engine, select_default_vt_engine_kind};

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_smoke_constructs_resizes_and_drops() {
        let mut engine = make_default_vt_engine(80, 24);

        assert_eq!(select_default_vt_engine_kind(), "ghostty");
        assert_eq!(engine.size(), (80, 24));

        engine.resize(120, 40).expect("resize ghostty engine");

        assert_eq!(engine.size(), (120, 40));
    }
}
