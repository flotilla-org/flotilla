use super::{ClientCapabilities, VtEngine};

#[derive(Debug, Clone)]
pub struct PassthroughVtEngine {
    cols: u16,
    rows: u16,
    bytes_seen: usize,
}

impl PassthroughVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows, bytes_seen: 0 }
    }

    pub fn bytes_seen(&self) -> usize {
        self.bytes_seen
    }
}

impl VtEngine for PassthroughVtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.bytes_seen += bytes.len();
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

    fn screen_text(&self) -> Result<String, String> {
        Err("screen text capture is unsupported for vt engine passthrough".to_string())
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}
