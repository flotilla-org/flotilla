use super::{ClientCapabilities, VtEngine};

#[derive(Debug, Clone)]
pub struct GhosttyVtEngine {
    cols: u16,
    rows: u16,
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl VtEngine for GhosttyVtEngine {
    fn feed(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        false
    }

    fn replay_payload(&self, _capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}
