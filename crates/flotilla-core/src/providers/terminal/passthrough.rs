use async_trait::async_trait;
use flotilla_protocol::{ManagedTerminal, ManagedTerminalId};

use super::TerminalPool;

pub struct PassthroughTerminalPool;

#[async_trait]
impl TerminalPool for PassthroughTerminalPool {
    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> {
        Ok(vec![])
    }

    async fn ensure_running(&self, _id: &ManagedTerminalId, _command: &str, _cwd: &std::path::Path) -> Result<(), String> {
        Ok(())
    }

    async fn attach_command(&self, _id: &ManagedTerminalId, command: &str, _cwd: &std::path::Path) -> Result<String, String> {
        Ok(command.to_string())
    }

    async fn kill_terminal(&self, _id: &ManagedTerminalId) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passthrough_list_returns_empty() {
        let pool = PassthroughTerminalPool;
        let terminals = pool.list_terminals().await.unwrap();
        assert!(terminals.is_empty());
    }

    #[tokio::test]
    async fn passthrough_ensure_running_is_noop() {
        let pool = PassthroughTerminalPool;
        let id = ManagedTerminalId { checkout: "test".into(), role: "shell".into(), index: 0 };
        assert!(pool.ensure_running(&id, "bash", "/tmp".as_ref()).await.is_ok());
    }

    #[tokio::test]
    async fn passthrough_attach_command_passes_through() {
        let pool = PassthroughTerminalPool;
        let id = ManagedTerminalId { checkout: "test".into(), role: "shell".into(), index: 0 };
        let result = pool.attach_command(&id, "bash", "/tmp".as_ref()).await.unwrap();
        assert_eq!(result, "bash");
    }
}
