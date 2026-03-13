pub mod passthrough;
pub mod shpool;

use std::path::Path;

use async_trait::async_trait;
use flotilla_protocol::{ManagedTerminal, ManagedTerminalId};

#[async_trait]
pub trait TerminalPool: Send + Sync {
    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String>;
    async fn ensure_running(&self, id: &ManagedTerminalId, command: &str, cwd: &Path) -> Result<(), String>;
    async fn attach_command(&self, id: &ManagedTerminalId, command: &str, cwd: &Path) -> Result<String, String>;
    async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String>;
}
