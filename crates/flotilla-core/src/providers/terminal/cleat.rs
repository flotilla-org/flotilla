use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, commands::AttachMode};
use serde::Deserialize;

use super::{ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionTag, TerminalSize};
use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{run, CommandRunner},
};

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
const DELIVERY_ENTER_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
struct SessionInfo {
    id: String,
    cwd: Option<std::path::PathBuf>,
    cmd: Option<String>,
    status: SessionStatus,
    screen_activity: Option<ScreenActivityWire>,
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
    attach_capability: tokio::sync::OnceCell<()>,
}

impl CleatTerminalPool {
    pub fn new(runner: Arc<dyn CommandRunner>, binary: impl Into<String>) -> Self {
        Self { runner, binary: binary.into(), terminal_env_defaults: vec![], attach_capability: tokio::sync::OnceCell::new() }
    }

    pub fn with_terminal_env_defaults(mut self, defaults: TerminalEnvVars) -> Self {
        self.terminal_env_defaults = defaults;
        self
    }

    fn parse_list_output(json: &str) -> Result<Vec<SessionInfo>, String> {
        serde_json::from_str(json).map_err(|err| format!("parse session list: {err}"))
    }

    fn build_attach_args(&self, session_name: &str, mode: AttachMode) -> Vec<Arg> {
        let mut args = vec![Arg::Literal(self.binary.clone()), Arg::Literal("attach".into()), Arg::Literal("--no-create".into())];
        match mode {
            AttachMode::Default => {}
            AttachMode::PreferTake => args.push(Arg::Literal("--take".into())),
            AttachMode::Strict => args.push(Arg::Literal("--strict".into())),
            AttachMode::Take => args.push(Arg::Literal("--take".into())),
        }
        // Session names are UUIDs (attachable IDs) — always shell-safe, no quoting needed.
        args.push(Arg::Literal(session_name.into()));
        args
    }

    async fn ensure_session_at_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
        initial_size: Option<TerminalSize>,
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
        let mut args = vec!["launch", "--json", "--record", session_name, "--cwd", &cwd, "--cmd", &effective_cmd];
        let encoded_size = initial_size.map(|size| size.to_string());
        if let Some(size) = &encoded_size {
            args.extend(["--size", size]);
        }
        let encoded_tags = tags.iter().map(|tag| format!("{}={}", tag.key, tag.value)).collect::<Vec<_>>();
        for tag in &encoded_tags {
            args.push("--tag");
            args.push(tag);
        }
        run!(self.runner, &self.binary, &args, Path::new("/"))?;
        Ok(())
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
        self.ensure_session_at_size(session_name, command, cwd, env_vars, tags, None).await
    }

    async fn ensure_session_with_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
        initial_size: Option<TerminalSize>,
    ) -> Result<(), String> {
        self.ensure_session_at_size(session_name, command, cwd, env_vars, tags, initial_size).await
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        Ok(self.build_attach_args(session_name, AttachMode::Default))
    }

    async fn preflight_attach(&self, _mode: AttachMode) -> Result<(), String> {
        // These flags arrived with cleat's controller-seat handshake, default
        // degrade-to-watch behavior, and watcher banner. Check before starting
        // the interactive attach so a stale selected pool fails on the primary screen.
        self.attach_capability
            .get_or_try_init(|| async {
                let help = run!(self.runner, &self.binary, &["attach", "--help"], Path::new("/"));
                if help.as_ref().is_ok_and(|help| help.contains("--strict") && help.contains("--take")) {
                    return Ok(());
                }
                let version = run!(self.runner, &self.binary, &["--version"], Path::new("/"))
                    .unwrap_or_else(|error| format!("version unavailable: {error}"));
                Err(format!(
                    "cleat terminal pool binary '{}' lacks controller-seat attach support (--strict/--take); detected {}",
                    self.binary,
                    version.trim()
                ))
            })
            .await
            .copied()
    }

    fn attach_args_for_mode(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
        mode: AttachMode,
    ) -> Result<Vec<Arg>, String> {
        Ok(self.build_attach_args(session_name, mode))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["kill", session_name], Path::new("/"))?;
        Ok(())
    }

    async fn capture_screen(&self, session_name: &str) -> Result<Option<String>, String> {
        run!(self.runner, &self.binary, &["capture", session_name], Path::new("/")).map(Some)
    }

    async fn deliver(&self, session_name: &str, text: &str) -> Result<(), String> {
        let paste = format!("{BRACKETED_PASTE_START}{text}{BRACKETED_PASTE_END}");
        run!(self.runner, &self.binary, &["send", session_name, &paste, "--no-enter"], Path::new("/"))?;
        tokio::time::sleep(DELIVERY_ENTER_DELAY).await;
        run!(self.runner, &self.binary, &["send-keys", session_name, "Enter"], Path::new("/"))?;
        Ok(())
    }

    async fn retry_delivery(&self, session_name: &str, text: &str) -> Result<(), String> {
        run!(self.runner, &self.binary, &["send-keys", session_name, "C-c"], Path::new("/"))?;
        self.deliver(session_name, text).await
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
    async fn ensure_launches_recorded_session() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"bash","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty (session doesn't exist)
            Ok(create_json.into()), // launch response
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[]).await.expect("ensure session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "cleat");
        assert_eq!(calls[1].1, vec!["launch", "--json", "--record", "my-session", "--cwd", "/repo", "--cmd", "bash"]);
    }

    #[tokio::test]
    async fn ensure_launches_session_with_requested_initial_size() {
        let runner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("{}".into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        pool.ensure_session_with_size(
            "my-session",
            "bash",
            &ExecutionEnvironmentPath::new("/repo"),
            &vec![],
            &[],
            Some(TerminalSize::new(200, 50)),
        )
        .await
        .expect("ensure sized session");

        assert_eq!(runner.calls()[1].1, vec![
            "launch",
            "--json",
            "--record",
            "my-session",
            "--cwd",
            "/repo",
            "--cmd",
            "bash",
            "--size",
            "200x50",
        ]);
    }

    #[tokio::test]
    async fn ensure_session_includes_env_vars_in_cmd() {
        let create_json = r#"{"id":"my-session","cwd":"/repo","cmd":"env FOO='bar' claude","status":"Detached"}"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok("[]".into()),        // list_sessions: empty
            Ok(create_json.into()), // launch response
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
            "launch",
            "--json",
            "--record",
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
    async fn ensure_sized_session_preserves_existing_live_session() {
        let list_json = r#"[{"id":"my-session","cwd":"/repo","cmd":"claude","status":"Detached"}]"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(list_json.into()), // list_sessions: session exists
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];

        pool.ensure_session_with_size(
            "my-session",
            "claude",
            &ExecutionEnvironmentPath::new("/repo"),
            &env,
            &[],
            Some(TerminalSize::new(200, 50)),
        )
        .await
        .expect("ensure session");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "should only call list, not launch: {calls:?}");
        assert!(calls[0].1.contains(&"list".to_string()), "should be a list call: {:?}", calls[0].1);
    }

    #[tokio::test]
    async fn attach_wraps_command() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");

        let cmd =
            pool.attach_command("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("attach command");

        assert_eq!(cmd, "cleat attach --no-create my-session");
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

    #[tokio::test(start_paused = true)]
    async fn delivery_writes_bracketed_paste_then_enter_for_single_and_multiline_messages() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new()), Ok(String::new()), Ok(String::new()), Ok(String::new())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");

        let single_started = tokio::time::Instant::now();
        pool.deliver("reviewer-session", "Please review commit abc123").await.expect("deliver single-line message");
        assert_eq!(single_started.elapsed(), DELIVERY_ENTER_DELAY);
        let multiline_started = tokio::time::Instant::now();
        pool.deliver("reviewer-session", "handoff from coder@work\n\nPlease review commit abc123")
            .await
            .expect("deliver multiline message");
        assert_eq!(multiline_started.elapsed(), DELIVERY_ENTER_DELAY);

        let calls = runner.calls();
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["send", "reviewer-session", "\x1b[200~Please review commit abc123\x1b[201~", "--no-enter"]);
        assert_eq!(calls[1].1, vec!["send-keys", "reviewer-session", "Enter"]);
        assert_eq!(calls[2].1, vec![
            "send",
            "reviewer-session",
            "\x1b[200~handoff from coder@work\n\nPlease review commit abc123\x1b[201~",
            "--no-enter"
        ]);
        assert_eq!(calls[3].1, vec!["send-keys", "reviewer-session", "Enter"]);
    }

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("--no-create".into()),
            Arg::Literal("my-session".into()),
        ]);
    }

    #[test]
    fn attach_args_flatten_with_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert_eq!(flat, "cleat attach --no-create my-session");
    }

    #[test]
    fn human_take_attach_adds_take_while_watch_omits_it() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let take = pool
            .attach_args_for_mode("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], AttachMode::PreferTake)
            .expect("take attach args");
        let watch = pool
            .attach_args_for_mode("my-session", "bash", &ExecutionEnvironmentPath::new("/repo"), &vec![], AttachMode::Default)
            .expect("watch attach args");

        assert_eq!(flotilla_protocol::arg::flatten(&take, 0), "cleat attach --no-create --take my-session");
        assert_eq!(flotilla_protocol::arg::flatten(&watch, 0), "cleat attach --no-create my-session");
    }

    #[tokio::test]
    async fn attach_preflight_accepts_controller_seat_capabilities() {
        let runner = Arc::new(MockRunner::new(vec![Ok("Options:\n  --strict\n  --take\n".into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "/pool/bin/cleat");

        pool.preflight_attach(AttachMode::Default).await.expect("modern cleat should pass preflight");
        pool.preflight_attach(AttachMode::Take).await.expect("capability result should be cached");

        assert_eq!(runner.calls(), [("/pool/bin/cleat".to_string(), vec!["attach".to_string(), "--help".to_string()])]);
    }

    #[tokio::test]
    async fn attach_preflight_names_a_stale_pool_binary_and_version() {
        let runner = Arc::new(MockRunner::new(vec![Ok("Options:\n  --no-create\n".into()), Ok("cleat 0.5.0".into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "/pool/bin/cleat");

        let error = pool.preflight_attach(AttachMode::Default).await.expect_err("stale cleat should fail preflight");

        assert!(error.contains("/pool/bin/cleat"), "{error}");
        assert!(error.contains("cleat 0.5.0"), "{error}");
        assert!(error.contains("--strict/--take"), "{error}");
    }

    #[tokio::test]
    async fn attach_preflight_retries_after_a_stale_binary_is_upgraded() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("Options:\n  --no-create\n".into()),
            Ok("cleat 0.5.0".into()),
            Ok("Options:\n  --strict\n  --take\n".into()),
        ]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "/pool/bin/cleat");

        pool.preflight_attach(AttachMode::Default).await.expect_err("stale cleat should fail preflight");
        pool.preflight_attach(AttachMode::Default).await.expect("upgraded cleat should pass without restarting the daemon");

        assert_eq!(runner.calls().len(), 3, "failed capability probes must not be cached");
    }

    #[test]
    fn attach_args_empty_command_no_env() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let args = pool.attach_args("sess-1", "", &ExecutionEnvironmentPath::new("/home/dev"), &vec![]).expect("attach_args");

        // Same structure regardless of command
        assert_eq!(args, vec![
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("--no-create".into()),
            Arg::Literal("sess-1".into()),
        ]);
    }

    #[test]
    fn attach_args_with_env_vars() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string()), ("BAZ".to_string(), "qu'x".to_string())];
        let args = pool.attach_args("sess", "cmd", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

        // Env vars are baked in at ensure_session/launch time — not in attach_args
        assert_eq!(args, vec![
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("--no-create".into()),
            Arg::Literal("sess".into()),
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
            Arg::Literal("--no-create".into()),
            Arg::Literal("sess".into()),
        ]);
    }

    #[test]
    fn attach_args_flatten_roundtrip_env_vars() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("sess", "bash", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        // No --cmd, no NestedCommand
        assert_eq!(flat, "cleat attach --no-create sess");
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
    async fn retry_delivery_clears_a_stuck_composer_before_resubmitting() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new()), Ok(String::new()), Ok(String::new())]));
        let pool = CleatTerminalPool::new(runner.clone(), "cleat");

        pool.retry_delivery("reviewer-session", "Please review commit abc123").await.expect("retry delivery");

        let calls = runner.calls();
        assert_eq!(calls[0].1, vec!["send-keys", "reviewer-session", "C-c"]);
        assert_eq!(calls[1].1, vec!["send", "reviewer-session", "\x1b[200~Please review commit abc123\x1b[201~", "--no-enter"]);
        assert_eq!(calls[2].1, vec!["send-keys", "reviewer-session", "Enter"]);
    }
}
