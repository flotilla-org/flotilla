use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{ManagedTerminal, ManagedTerminalId, TerminalStatus};

use super::TerminalPool;
use crate::providers::CommandRunner;

pub struct ShpoolTerminalPool {
    runner: Arc<dyn CommandRunner>,
    socket_path: PathBuf,
}

impl ShpoolTerminalPool {
    pub fn new(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        Self {
            runner,
            socket_path,
        }
    }

    /// Parse the JSON output of `shpool list --json`.
    fn parse_list_json(json: &str) -> Result<Vec<ManagedTerminal>, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("failed to parse shpool list: {e}"))?;

        let sessions = parsed["sessions"]
            .as_array()
            .ok_or("shpool list: no sessions array")?;

        let mut terminals = Vec::new();
        for session in sessions {
            let name = session["name"]
                .as_str()
                .ok_or("shpool session missing name")?;

            // Only show flotilla-managed sessions (prefixed "flotilla/")
            let Some(rest) = name.strip_prefix("flotilla/") else {
                continue;
            };

            // Parse "checkout/role/index"
            let parts: Vec<&str> = rest.splitn(3, '/').collect();
            if parts.len() != 3 {
                continue;
            }
            let index: u32 = parts[2].parse().unwrap_or(0);

            let status_str = session["status"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase();
            let status = match status_str.as_str() {
                "attached" => TerminalStatus::Running,
                "disconnected" => TerminalStatus::Disconnected,
                _ => TerminalStatus::Disconnected,
            };

            terminals.push(ManagedTerminal {
                id: ManagedTerminalId {
                    checkout: parts[0].into(),
                    role: parts[1].into(),
                    index,
                },
                role: parts[1].into(),
                command: String::new(), // shpool doesn't report the original command
                working_directory: PathBuf::new(), // populated separately if needed
                status,
            });
        }

        Ok(terminals)
    }
}

#[async_trait]
impl TerminalPool for ShpoolTerminalPool {
    fn display_name(&self) -> &str {
        "shpool"
    }

    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> {
        let socket_path_str = self.socket_path.display().to_string();
        let result = self
            .runner
            .run(
                "shpool",
                &["--socket", &socket_path_str, "list", "--json"],
                Path::new("/"),
            )
            .await;

        match result {
            Ok(json) => Self::parse_list_json(&json),
            Err(e) => {
                tracing::debug!("shpool list failed (daemon may not be running): {e}");
                Ok(vec![])
            }
        }
    }

    async fn ensure_running(
        &self,
        _id: &ManagedTerminalId,
        _command: &str,
        _cwd: &Path,
    ) -> Result<(), String> {
        // No-op: shpool creates sessions on first `attach`. The actual session
        // creation happens when the workspace manager runs the attach_command.
        Ok(())
    }

    async fn attach_command(
        &self,
        id: &ManagedTerminalId,
        command: &str,
        cwd: &Path,
    ) -> Result<String, String> {
        let session_name = format!("flotilla/{id}");
        let socket_path_str = self.socket_path.display().to_string();
        let cwd_str = cwd.display().to_string();
        fn sq(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
        // shpool attach creates the session if it doesn't exist (using --cmd/--dir),
        // or reattaches if it does (ignoring --cmd/--dir).
        Ok(format!(
            "shpool --socket {} attach --cmd {} --dir {} {}",
            sq(&socket_path_str),
            sq(command),
            sq(&cwd_str),
            sq(&session_name),
        ))
    }

    async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
        let session_name = format!("flotilla/{id}");
        let socket_path_str = self.socket_path.display().to_string();
        self.runner
            .run(
                "shpool",
                &["--socket", &socket_path_str, "kill", &session_name],
                Path::new("/"),
            )
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testing::MockRunner;

    #[test]
    fn parse_list_json_with_flotilla_sessions() {
        let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/my-feature/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/my-feature/agent/0",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                },
                {
                    "name": "user-manual-session",
                    "started_at_unix_ms": 1709900002000,
                    "status": "Attached"
                }
            ]
        }"#;

        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(terminals.len(), 2); // user-manual-session filtered out

        assert_eq!(terminals[0].id.checkout, "my-feature");
        assert_eq!(terminals[0].id.role, "shell");
        assert_eq!(terminals[0].id.index, 0);
        assert_eq!(terminals[0].status, TerminalStatus::Running);

        assert_eq!(terminals[1].id.checkout, "my-feature");
        assert_eq!(terminals[1].id.role, "agent");
        assert_eq!(terminals[1].status, TerminalStatus::Disconnected);
    }

    #[test]
    fn parse_list_json_empty_sessions() {
        let json = r#"{"sessions": []}"#;
        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert!(terminals.is_empty());
    }

    #[test]
    fn parse_list_json_invalid_json() {
        assert!(ShpoolTerminalPool::parse_list_json("not json").is_err());
    }

    #[tokio::test]
    async fn ensure_running_is_noop() {
        let runner = Arc::new(MockRunner::new(vec![]));
        let pool = ShpoolTerminalPool::new(runner, PathBuf::from("/tmp/test.sock"));
        let id = ManagedTerminalId {
            checkout: "feat".into(),
            role: "shell".into(),
            index: 0,
        };
        assert!(pool
            .ensure_running(&id, "bash", Path::new("/home/dev"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn attach_command_includes_cmd_and_dir() {
        let runner = Arc::new(MockRunner::new(vec![]));
        let pool = ShpoolTerminalPool::new(runner, PathBuf::from("/tmp/test.sock"));
        let id = ManagedTerminalId {
            checkout: "feat".into(),
            role: "shell".into(),
            index: 0,
        };
        let cmd = pool
            .attach_command(&id, "bash", Path::new("/home/dev"))
            .await
            .unwrap();
        assert!(cmd.contains("shpool"));
        assert!(cmd.contains("attach"));
        assert!(cmd.contains("--cmd"));
        assert!(cmd.contains("bash"));
        assert!(cmd.contains("--dir"));
        assert!(cmd.contains("/home/dev"));
        assert!(cmd.contains("flotilla/feat/shell/0"));
    }

    #[tokio::test]
    async fn list_terminals_returns_empty_when_daemon_not_running() {
        let runner = Arc::new(MockRunner::new(vec![Err("connection refused".into())]));
        let pool = ShpoolTerminalPool::new(runner, PathBuf::from("/tmp/test.sock"));
        let terminals = pool.list_terminals().await.unwrap();
        assert!(terminals.is_empty());
    }
}
