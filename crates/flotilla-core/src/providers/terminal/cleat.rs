use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::arg::Arg;
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

    fn attach_args(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<Vec<Arg>, String> {
        let mut args = vec![
            Arg::Quoted(self.binary.clone()),
            Arg::Literal("attach".into()),
            Arg::Quoted(session_name.into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted(cwd.display().to_string()),
        ];
        if !command.is_empty() || !env_vars.is_empty() {
            let mut cmd_inner: Vec<Arg> = Vec::new();
            if !env_vars.is_empty() {
                cmd_inner.push(Arg::Literal("env".into()));
                for (k, v) in env_vars {
                    // KEY='value' as a single shell token — key is a safe identifier,
                    // value is single-quoted using the same quoting as Arg::Quoted.
                    cmd_inner.push(Arg::Literal(format!("{k}={}", flotilla_protocol::arg::shell_quote(v))));
                }
            }
            cmd_inner.push(Arg::Literal("$SHELL".into()));
            if !command.is_empty() {
                cmd_inner.push(Arg::Literal("-lc".into()));
                cmd_inner.push(Arg::Quoted(command.into()));
            }
            args.push(Arg::Literal("--cmd".into()));
            args.push(Arg::NestedCommand(cmd_inner));
        }
        Ok(args)
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
        let json = r#"{"id":"my-session","cwd":"/repo","cmd":"bash","status":"Detached"}"#;
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

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", Path::new("/repo"), &vec![]).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("my-session".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/repo".into()),
            Arg::Literal("--cmd".into()),
            Arg::NestedCommand(vec![Arg::Literal("$SHELL".into()), Arg::Literal("-lc".into()), Arg::Quoted("bash".into()),]),
        ]);
    }

    #[test]
    fn attach_args_flatten_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", Path::new("/repo"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "'cleat' attach 'my-session' --cwd '/repo' --cmd '$SHELL -lc '\\''bash'\\'''");
    }

    #[test]
    fn attach_args_empty_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("sess-1", "", Path::new("/home/dev"), &vec![]).expect("attach_args");

        // No --cmd when both command and env_vars are empty
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
        let args = pool.attach_args("sess", "cmd", Path::new("/wd"), &env).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("sess".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/wd".into()),
            Arg::Literal("--cmd".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("env".into()),
                Arg::Literal("FOO='bar'".into()),
                Arg::Literal("BAZ='qu'\\''x'".into()),
                Arg::Literal("$SHELL".into()),
                Arg::Literal("-lc".into()),
                Arg::Quoted("cmd".into()),
            ]),
        ]);
    }

    #[test]
    fn attach_args_with_env_vars_empty_command() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("KEY".to_string(), "val".to_string())];
        let args = pool.attach_args("sess", "", Path::new("/wd"), &env).expect("attach_args");

        // Empty command with env vars: spawns $SHELL with env prefix, no -lc
        assert_eq!(args, vec![
            Arg::Quoted("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("sess".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/wd".into()),
            Arg::Literal("--cmd".into()),
            Arg::NestedCommand(vec![Arg::Literal("env".into()), Arg::Literal("KEY='val'".into()), Arg::Literal("$SHELL".into()),]),
        ]);
    }

    #[test]
    fn attach_args_flatten_roundtrip_env_vars() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("sess", "bash", Path::new("/wd"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        // --cmd value is a NestedCommand, so the inner string is shell-quoted.
        // The inner flattened string is: env FOO='bar' $SHELL -lc 'bash'
        // After outer shell-quoting, single quotes are escaped as '\''.
        assert!(flat.contains("--cmd"), "should have --cmd: {flat}");
        // Verify the inner command structure by checking the NestedCommand directly
        let nested = args.iter().find(|a| matches!(a, Arg::NestedCommand(_)));
        assert!(nested.is_some(), "should have NestedCommand for --cmd: {args:?}");
        if let Some(Arg::NestedCommand(inner)) = nested {
            let inner_flat = flotilla_protocol::arg::flatten(inner, 0);
            assert!(inner_flat.contains("FOO='bar'"), "inner should contain env assignment: {inner_flat}");
            assert!(inner_flat.contains("$SHELL"), "inner should reference $SHELL: {inner_flat}");
        }
    }
}
