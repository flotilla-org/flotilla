use async_trait::async_trait;

use super::{TerminalEnvVars, TerminalPool, TerminalSession};

pub struct PassthroughTerminalPool;

#[async_trait]
impl TerminalPool for PassthroughTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        Ok(vec![])
    }

    async fn ensure_session(&self, _session_name: &str, _command: &str, _cwd: &std::path::Path) -> Result<(), String> {
        Ok(())
    }

    async fn attach_command(
        &self,
        _session_name: &str,
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

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
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
    async fn list_returns_empty() {
        let pool = PassthroughTerminalPool;
        let sessions = pool.list_sessions().await.unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn ensure_is_noop() {
        let pool = PassthroughTerminalPool;
        assert!(pool.ensure_session("my-session", "bash", "/tmp".as_ref()).await.is_ok());
    }

    #[tokio::test]
    async fn attach_passes_through() {
        let pool = PassthroughTerminalPool;
        let result = pool.attach_command("my-session", "bash", "/tmp".as_ref(), &vec![]).await.unwrap();
        assert_eq!(result, "bash");
    }

    #[tokio::test]
    async fn attach_injects_env_vars() {
        let pool = PassthroughTerminalPool;
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let result = pool.attach_command("my-session", "bash", "/tmp".as_ref(), &env).await.unwrap();
        assert!(result.starts_with("env "));
        assert!(result.contains("FOO='bar'"));
        assert!(result.ends_with("bash"));
    }

    #[tokio::test]
    async fn kill_is_noop() {
        let pool = PassthroughTerminalPool;
        assert!(pool.kill_session("my-session").await.is_ok());
    }
}
