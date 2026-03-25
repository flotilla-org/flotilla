use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::arg::Arg;
use serde::Deserialize;

use super::{TerminalEnvVars, TerminalPool, TerminalSession};
use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{run, CommandRunner},
};

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
                TerminalSession {
                    session_name: session.id,
                    status,
                    command: session.cmd,
                    working_directory: session.cwd.map(ExecutionEnvironmentPath::new),
                }
            })
            .collect())
    }

    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<(), String> {
        // If the session already exists, leave it alone.
        let existing = self.list_sessions().await.unwrap_or_default();
        if existing.iter().any(|s| s.session_name == session_name) {
            return Ok(());
        }

        let effective_cmd = if env_vars.is_empty() {
            command.to_string()
        } else {
            let mut parts = vec!["env".to_string()];
            for (k, v) in env_vars {
                parts.push(format!("{k}={}", flotilla_protocol::arg::shell_quote(v)));
            }
            parts.push(command.to_string());
            parts.join(" ")
        };

        run!(
            self.runner,
            &self.binary,
            &["create", "--json", session_name, "--cwd", &cwd.as_path().display().to_string(), "--cmd", &effective_cmd],
            Path::new("/")
        )?;
        Ok(())
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: &str,
        cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        Ok(vec![
            Arg::Quoted(self.binary.clone()),
            Arg::Literal("attach".into()),
            Arg::Quoted(session_name.into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted(cwd.as_path().display().to_string()),
        ])
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["kill", session_name], Path::new("/"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::*;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::{testing::MockRunner, CommandRunner},
    };

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
        assert_eq!(sessions[0].working_directory.as_ref().map(|p| p.as_path()), Some(Path::new("/repo")));
        assert_eq!(sessions[1].session_name, "sess-2");
        assert_eq!(sessions[1].status, flotilla_protocol::TerminalStatus::Disconnected);
        assert!(sessions[1].command.is_none());
        assert_eq!(sessions[1].working_directory.as_ref().map(|p| p.as_path()), Some(Path::new("/other")));
    }

    #[tokio::test]
    async fn ensure_creates_session() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"bash","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty (session doesn't exist)
            Ok(create_json.into()), // create response
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("ensure session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "cleat");
        assert_eq!(calls[1].1, vec!["create", "--json", "my-session", "--cwd", "/repo", "--cmd", "bash"]);
    }

    #[tokio::test]
    async fn ensure_session_includes_env_vars_in_cmd() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"env FOO='bar' claude","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty
            Ok(create_json.into()), // create response
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];

        pool.ensure_session("my-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env).await.expect("ensure session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "cleat");
        let cmd_idx = calls[1].1.iter().position(|a| a == "--cmd").expect("--cmd present");
        let cmd_val = &calls[1].1[cmd_idx + 1];
        assert!(cmd_val.starts_with("env "), "should prefix with env: {cmd_val}");
        assert!(cmd_val.contains("FOO='bar'"), "should contain quoted env var: {cmd_val}");
        assert!(cmd_val.ends_with("claude"), "should end with command: {cmd_val}");
    }

    #[tokio::test]
    async fn ensure_session_skips_if_session_exists() {
        let list_json = r#"[{"id":"my-session","cwd":"/repo","cmd":"claude","status":"Detached"}]"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(list_json.into()), // list_sessions: session exists
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];

        pool.ensure_session("my-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env).await.expect("ensure session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "should only call list, not create: {calls:?}");
        assert!(calls[0].1.contains(&"list".to_string()), "should be a list call: {:?}", calls[0].1);
    }

    #[tokio::test]
    async fn attach_wraps_command() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");

        let cmd =
            pool.attach_command("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("attach command");

        assert!(cmd.contains("'cleat' attach 'my-session'"));
        assert!(cmd.contains("--cwd '/repo'"));
        assert!(!cmd.contains("--cmd"), "should NOT have --cmd");
    }

    #[tokio::test]
    async fn kill_calls_cli() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.kill_session("my-session").await.expect("kill session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["kill", "my-session"]);
    }

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("my-session".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/repo".into()),
        ]);
    }

    #[test]
    fn attach_args_flatten_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "'cleat' attach 'my-session' --cwd '/repo'");
    }

    #[test]
    fn attach_args_empty_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("sess-1", "", &ExecutionEnvironmentPath::new("/home/dev"), &vec![]).expect("attach_args");

        // Same structure regardless of command
        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("sess-1".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/home/dev".into()),
        ]);
    }

    #[test]
    fn attach_args_with_env_vars() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string()), ("BAZ".to_string(), "qu'x".to_string())];
        let args = pool.attach_args("sess", "cmd", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

        // Env vars are baked in at ensure_session/create time — not in attach_args
        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("sess".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/wd".into()),
        ]);
    }

    #[test]
    fn attach_args_with_env_vars_empty_command() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("KEY".to_string(), "val".to_string())];
        let args = pool.attach_args("sess", "", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("sess".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/wd".into()),
        ]);
    }

    #[test]
    fn attach_args_flatten_roundtrip_env_vars() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("sess", "bash", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        // No --cmd, no NestedCommand
        assert_eq!(flat, "'cleat' attach 'sess' --cwd '/wd'");
    }
}
