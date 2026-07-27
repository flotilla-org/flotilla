use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::arg::Arg;
use serde::Deserialize;

use super::{ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionTag};
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
    screen_activity: Option<ScreenActivityWire>,
}

#[derive(Debug, Deserialize)]
struct InspectResult {
    attachments: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
enum SessionStatus {
    Attached,
    Detached,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScreenActivityWire {
    Active,
    Stable,
}

pub struct CleatTerminalPool {
    runner: Arc<dyn CommandRunner>,
    binary: String,
    terminal_env_defaults: TerminalEnvVars,
}

impl CleatTerminalPool {
    pub fn new(runner: Arc<dyn CommandRunner>, binary: impl Into<String>) -> Self {
        Self { runner, binary: binary.into(), terminal_env_defaults: vec![] }
    }

    pub fn with_terminal_env_defaults(mut self, defaults: TerminalEnvVars) -> Self {
        self.terminal_env_defaults = defaults;
        self
    }

    fn parse_list_output(json: &str) -> Result<Vec<SessionInfo>, String> {
        serde_json::from_str(json).map_err(|err| format!("parse session list: {err}"))
    }

    fn attachment_identity(attachment: &serde_json::Value) -> Option<String> {
        ["holder", "identity", "principal", "name"].into_iter().find_map(|field| {
            let value = attachment.get(field)?;
            value.as_str().map(str::to_string).or_else(|| {
                value
                    .as_object()
                    .and_then(|identity| ["display_name", "name", "id"].into_iter().find_map(|key| identity.get(key)?.as_str()))
                    .map(str::to_string)
            })
        })
    }
}

#[async_trait]
impl TerminalPool for CleatTerminalPool {
    fn tracks_session_liveness(&self) -> bool {
        true
    }

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
                    screen_activity: session.screen_activity.map(|activity| match activity {
                        ScreenActivityWire::Active => ScreenActivity::Active,
                        ScreenActivityWire::Stable => ScreenActivity::Stable,
                    }),
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
        tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        let existing = self.list_sessions().await.unwrap_or_default();
        if existing.iter().any(|session| session.session_name == session_name) {
            return Ok(());
        }

        let has_env = !env_vars.is_empty() || !self.terminal_env_defaults.is_empty();
        let effective_cmd = if !has_env {
            command.to_string()
        } else {
            let mut parts = vec!["env".to_string()];
            for (key, value) in self.terminal_env_defaults.iter().chain(env_vars) {
                parts.push(format!("{key}={}", flotilla_protocol::arg::shell_quote(value)));
            }
            parts.push(command.to_string());
            parts.join(" ")
        };
        let cwd = cwd.as_path().display().to_string();
        let mut args = vec!["create", "--json", session_name, "--cwd", &cwd, "--cmd", &effective_cmd];
        let encoded_tags = tags.iter().map(|tag| format!("{}={}", tag.key, tag.value)).collect::<Vec<_>>();
        for tag in &encoded_tags {
            args.push("--tag");
            args.push(tag);
        }
        run!(self.runner, &self.binary, &args, Path::new("/"))?;
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
            Arg::Literal(self.binary.clone()),
            Arg::Literal("attach".into()),
            // Session names are UUIDs (attachable IDs) — always shell-safe, no quoting needed.
            Arg::Literal(session_name.into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted(cwd.as_path().display().to_string()),
        ])
    }

    async fn controller_holder(&self, session_name: &str) -> Result<Option<String>, String> {
        let output = run!(self.runner, &self.binary, &["inspect", "--json", session_name], Path::new("/"))?;
        let inspect: InspectResult = serde_json::from_str(&output).map_err(|err| format!("parse cleat inspect output: {err}"))?;
        Ok(inspect.attachments.iter().find_map(|attachment| {
            (attachment.get("role").and_then(serde_json::Value::as_str) == Some("controller"))
                .then(|| Self::attachment_identity(attachment).unwrap_or_default())
        }))
    }

    fn watch_args(&self, session_name: &str) -> Result<Vec<Arg>, String> {
        Ok(vec![Arg::Literal(self.binary.clone()), Arg::Literal("watch".into()), Arg::Literal(session_name.into())])
    }

    async fn take_args(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        let help = run!(self.runner, &self.binary, &["attach", "--help"], Path::new("/"))?;
        if !help.lines().any(|line| line.split_whitespace().any(|word| word == "--take")) {
            return Err(
                "`flotilla attach --take` requires cleat force-take support; this cleat does not provide `attach --take` (see flotilla-org/cleat#177)"
                    .to_string(),
            );
        }
        let mut args = self.attach_args(session_name, command, cwd, env_vars)?;
        args.insert(2, Arg::Literal("--take".into()));
        Ok(args)
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["kill", session_name], Path::new("/"))?;
        Ok(())
    }

    async fn capture_screen(&self, session_name: &str) -> Result<Option<String>, String> {
        run!(self.runner, &self.binary, &["capture", session_name], Path::new("/")).map(Some)
    }

    async fn deliver(&self, session_name: &str, text: &str, submit: bool) -> Result<(), String> {
        let mut args = vec!["send", session_name, text];
        if submit {
            args.push("--submit");
        }
        run!(self.runner, &self.binary, &args, Path::new("/"))?;
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
            {"id":"sess-1","cwd":"/repo","cmd":"bash","status":"Attached","screen_activity":"active"},
            {"id":"sess-2","cwd":"/other","cmd":null,"status":"Detached","screen_activity":"stable"}
        ]"#;
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok(json.into())])), "cleat");

        let sessions = pool.list_sessions().await.expect("list sessions");

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_name, "sess-1");
        assert_eq!(sessions[0].status, flotilla_protocol::TerminalStatus::Running);
        assert_eq!(sessions[0].command.as_deref(), Some("bash"));
        assert_eq!(sessions[0].working_directory.as_ref().map(|p| p.as_path()), Some(Path::new("/repo")));
        assert_eq!(sessions[0].screen_activity, Some(ScreenActivity::Active));
        assert_eq!(sessions[1].session_name, "sess-2");
        assert_eq!(sessions[1].status, flotilla_protocol::TerminalStatus::Disconnected);
        assert!(sessions[1].command.is_none());
        assert_eq!(sessions[1].working_directory.as_ref().map(|p| p.as_path()), Some(Path::new("/other")));
        assert_eq!(sessions[1].screen_activity, Some(ScreenActivity::Stable));
    }

    #[tokio::test]
    async fn capture_screen_reads_the_rendered_terminal() {
        let runner = Arc::new(MockRunner::new(vec![Ok("Do you trust the contents of this directory?\n".into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        assert_eq!(
            pool.capture_screen("terminal-demo-work-coder").await.expect("capture screen").as_deref(),
            Some("Do you trust the contents of this directory?\n")
        );
        assert_eq!(runner.calls()[0], ("cleat".to_string(), vec!["capture".to_string(), "terminal-demo-work-coder".to_string()]));
    }

    #[tokio::test]
    async fn ensure_creates_session() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"bash","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty (session doesn't exist)
            Ok(create_json.into()), // create response
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[]).await.expect("ensure session");

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

        pool.ensure_session("my-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env, &[]).await.expect("ensure session");

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
    async fn ensure_session_tags_convoy_and_vessel() {
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("{}".into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session("terminal-demo-coder", "codex", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[
            TerminalSessionTag::new("convoy", "demo"),
            TerminalSessionTag::new("vessel", "demo-work"),
        ])
        .await
        .expect("ensure tagged session");

        assert_eq!(runner.calls()[1].1, vec![
            "create",
            "--json",
            "terminal-demo-coder",
            "--cwd",
            "/repo",
            "--cmd",
            "codex",
            "--tag",
            "convoy=demo",
            "--tag",
            "vessel=demo-work",
        ]);
    }

    #[tokio::test]
    async fn ensure_session_skips_if_session_exists() {
        let list_json = r#"[{"id":"my-session","cwd":"/repo","cmd":"claude","status":"Detached"}]"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(list_json.into()), // list_sessions: session exists
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];

        pool.ensure_session("my-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env, &[]).await.expect("ensure session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "should only call list, not create: {calls:?}");
        assert!(calls[0].1.contains(&"list".to_string()), "should be a list call: {:?}", calls[0].1);
    }

    #[tokio::test]
    async fn attach_wraps_command() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");

        let cmd =
            pool.attach_command("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("attach command");

        assert!(cmd.contains("cleat attach my-session"));
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

    #[tokio::test]
    async fn deliver_submits_text_to_the_existing_session() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.deliver("reviewer-session", "Please review commit abc123", true).await.expect("deliver message");

        let calls = runner.calls();
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["send", "reviewer-session", "Please review commit abc123", "--submit"]);
    }

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("my-session".into()),
            Arg::Literal("--cwd".into()),
            Arg::Quoted("/repo".into()),
        ]);
    }

    #[test]
    fn attach_args_flatten_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "cleat attach my-session --cwd '/repo'");
    }

    #[test]
    fn attach_args_empty_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("sess-1", "", &ExecutionEnvironmentPath::new("/home/dev"), &vec![]).expect("attach_args");

        // Same structure regardless of command
        assert_eq!(args, vec![
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("sess-1".into()),
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
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("sess".into()),
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
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("sess".into()),
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
        assert_eq!(flat, "cleat attach sess --cwd '/wd'");
    }

    #[tokio::test]
    async fn ensure_session_terminal_env_defaults_appear_before_caller_env() {
        let create_json = r#"{"id":"sess","cwd":"/repo","cmd":"env TERM='xterm-256color' FOO='bar' claude","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok(create_json.into())]));
        let env_defaults = vec![("TERM".to_string(), "xterm-256color".to_string())];
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat").with_terminal_env_defaults(env_defaults);
        let caller_env = vec![("FOO".to_string(), "bar".to_string())];

        pool.ensure_session("sess", "claude", &ExecutionEnvironmentPath::new("/repo"), &caller_env, &[]).await.expect("ensure session");

        let calls = runner.calls();
        let cmd_idx = calls[1].1.iter().position(|a| a == "--cmd").expect("--cmd present");
        let cmd_val = &calls[1].1[cmd_idx + 1];
        let term_pos = cmd_val.find("TERM=").expect("should contain TERM");
        let foo_pos = cmd_val.find("FOO=").expect("should contain FOO");
        assert!(term_pos < foo_pos, "terminal defaults should appear before caller env vars: {cmd_val}");
    }

    #[tokio::test]
    async fn controller_holder_reads_identity_when_cleat_reports_it() {
        let inspect = r#"{"attachments":[{"role":"controller","identity":{"display_name":"alice"}},{"role":"watcher"}]}"#;
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok(inspect.into())])), "cleat");

        assert_eq!(pool.controller_holder("sess").await.expect("inspect controller").as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn controller_holder_marks_an_unidentified_controller() {
        let inspect = r#"{"attachments":[{"role":"controller"}]}"#;
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok(inspect.into())])), "cleat");

        assert_eq!(pool.controller_holder("sess").await.expect("inspect controller").as_deref(), Some(""));
    }

    #[tokio::test]
    async fn take_reports_when_installed_cleat_lacks_the_capability() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok("Usage: cleat attach [OPTIONS]".into())])), "cleat");

        let error = pool
            .take_args("sess", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![])
            .await
            .expect_err("old cleat should reject take");

        assert!(error.contains("does not provide `attach --take`"));
        assert!(error.contains("cleat#177"));
    }

    #[tokio::test]
    async fn take_uses_the_capability_when_cleat_advertises_it() {
        let pool =
            CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok("Options:\n      --take  Force take control".into())])), "cleat");

        let args =
            pool.take_args("sess", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("new cleat should support take");

        assert_eq!(flotilla_protocol::arg::flatten(&args, 0), "cleat attach --take sess --cwd '/repo'");
    }
}
