pub mod contracts;
pub mod passthrough;

pub trait VtEngine: Send {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    fn supports_replay(&self) -> bool;
    fn replay_payload(&self) -> Result<Option<Vec<u8>>, String>;
    fn size(&self) -> (u16, u16);
}
