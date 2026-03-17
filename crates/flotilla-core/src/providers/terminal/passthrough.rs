use async_trait::async_trait;
use flotilla_protocol::{ManagedTerminal, ManagedTerminalId};

use super::{TerminalEnvVars, TerminalPool};

pub struct PassthroughTerminalPool;

#[async_trait]
impl TerminalPool for PassthroughTerminalPool {
    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> {
        Ok(vec![])
    }

    async fn ensure_running(&self, _id: &ManagedTerminalId, _command: &str, _cwd: &std::path::Path) -> Result<(), String> {
        Ok(())
    }

    async fn attach_command(
        &self,
        _id: &ManagedTerminalId,
        command: &str,
        _cwd: &std::path::Path,
        env_vars: &TerminalEnvVars,
    ) -> Result<String, String> {
        if env_vars.is_empty() {
            Ok(command.to_string())
        } else {
            let prefix: Vec<String> = env_vars.iter().map(|(k, v)| format!("{k}={}", shell_escape(v))).collect();
            Ok(format!("env {} {command}", prefix.join(" ")))
        }
    }

    async fn kill_terminal(&self, _id: &ManagedTerminalId) -> Result<(), String> {
        Ok(())
    }
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
        let result = pool.attach_command(&id, "bash", "/tmp".as_ref(), &vec![]).await.unwrap();
        assert_eq!(result, "bash");
    }

    #[tokio::test]
    async fn passthrough_attach_command_injects_env_vars() {
        let pool = PassthroughTerminalPool;
        let id = ManagedTerminalId { checkout: "test".into(), role: "shell".into(), index: 0 };
        let env = vec![
            ("FLOTILLA_ATTACHABLE_ID".to_string(), "att-123".to_string()),
            ("FLOTILLA_DAEMON_SOCKET".to_string(), "/tmp/flotilla.sock".to_string()),
        ];
        let result = pool.attach_command(&id, "bash", "/tmp".as_ref(), &env).await.unwrap();
        assert!(result.starts_with("env "));
        assert!(result.contains("FLOTILLA_ATTACHABLE_ID='att-123'"));
        assert!(result.contains("FLOTILLA_DAEMON_SOCKET='/tmp/flotilla.sock'"));
        assert!(result.ends_with("bash"));
    }
}
