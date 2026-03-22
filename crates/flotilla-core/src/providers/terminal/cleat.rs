use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use serde::Deserialize;

use super::{TerminalEnvVars, TerminalPool, TerminalSession};
use crate::providers::{run, CommandRunner};

#[derive(Debug, Deserialize)]
struct SessionInfo {
    id: String,
    cwd: Option<std::path::PathBuf>,
    cmd: Option<String>,
    status: SessionStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
enum SessionStatus {
    Attached,
    Detached,
}

pub struct CleatTerminalPool {
    runner: Arc<dyn CommandRunner>,
    binary: String,
}

impl CleatTerminalPool {
    pub fn new(runner: Arc<dyn CommandRunner>, binary: impl Into<String>) -> Self {
        Self { runner, binary: binary.into() }
    }

    fn parse_list_output(json: &str) -> Result<Vec<SessionInfo>, String> {
        serde_json::from_str(json).map_err(|err| format!("parse session list: {err}"))
    }
}

#[async_trait]
impl TerminalPool for CleatTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        let output = run!(self.runner, &self.binary, &["list", "--json"], Path::new("/"))?;
        let sessions = Self::parse_list_output(&output)?;
        Ok(sessions
            .into_iter()
            .map(|session| {
                let status = match session.status {
                    SessionStatus::Attached => flotilla_protocol::TerminalStatus::Running,
                    SessionStatus::Detached => flotilla_protocol::TerminalStatus::Disconnected,
                };
                TerminalSession { session_name: session.id, status, command: session.cmd, working_directory: session.cwd }
            })
            .collect())
    }

    async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String> {
        run!(
            self.runner,
            &self.binary,
            &["create", "--json", session_name, "--cwd", &cwd.display().to_string(), "--cmd", command],
            Path::new("/")
        )?;
        Ok(())
    }

    async fn attach_command(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<String, String> {
        fn sq(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }

        let mut parts = vec![sq(&self.binary), "attach".into(), sq(session_name), "--cwd".into(), sq(&cwd.display().to_string())];
        if !command.is_empty() || !env_vars.is_empty() {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let env_prefix = if env_vars.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = env_vars.iter().map(|(k, v)| format!("{k}={}", sq(v))).collect();
                format!("env {} ", pairs.join(" "))
            };
            let wrapped = if command.is_empty() {
                format!("{env_prefix}{shell}")
            } else {
                format!("{env_prefix}{shell} -lc '{}'", command.replace('\'', "'\\''"))
            };
            parts.push("--cmd".into());
            parts.push(sq(&wrapped));
        }
        Ok(parts.join(" "))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["kill", session_name], Path::new("/"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use async_trait::async_trait;

    use super::*;
    use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};

    struct MockRunner {
        responses: std::sync::Mutex<Vec<Result<String, String>>>,
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self { responses: std::sync::Mutex::new(responses), calls: std::sync::Mutex::new(vec![]) }
        }
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
            self.responses.lock().expect("responses").remove(0)
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), success: true })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn list_sessions_parses_json() {
        let json = r#"[
            {"id":"sess-1","cwd":"/repo","cmd":"bash","status":"Attached"},
            {"id":"sess-2","cwd":"/other","cmd":null,"status":"Detached"}
        ]"#;
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok(json.into())])), "cleat");

        let sessions = pool.list_sessions().await.expect("list sessions");

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_name, "sess-1");
        assert_eq!(sessions[0].status, flotilla_protocol::TerminalStatus::Running);
        assert_eq!(sessions[0].command.as_deref(), Some("bash"));
        assert_eq!(sessions[0].working_directory.as_deref(), Some(Path::new("/repo")));
        assert_eq!(sessions[1].session_name, "sess-2");
        assert_eq!(sessions[1].status, flotilla_protocol::TerminalStatus::Disconnected);
        assert!(sessions[1].command.is_none());
        assert_eq!(sessions[1].working_directory.as_deref(), Some(Path::new("/other")));
    }

    #[tokio::test]
    async fn ensure_creates_session() {
        let json = r#"{"id":"new-sess","cwd":"/repo","cmd":"bash","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![Ok(json.into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session("my-session", "bash", Path::new("/repo")).await.expect("ensure session");

        let calls = runner.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["create", "--json", "my-session", "--cwd", "/repo", "--cmd", "bash"]);
    }

    #[tokio::test]
    async fn attach_wraps_command() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");

        let cmd = pool.attach_command("my-session", "bash", Path::new("/repo"), &vec![]).await.expect("attach command");

        assert!(cmd.contains("'cleat' attach 'my-session'"));
        assert!(cmd.contains("--cwd '/repo'"));
        assert!(cmd.contains("--cmd"));
    }

    #[tokio::test]
    async fn kill_calls_cli() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.kill_session("my-session").await.expect("kill session");

        let calls = runner.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["kill", "my-session"]);
    }
}
