use std::sync::Arc;

use super::*;
use crate::{
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::testing::MockRunner,
};

/// Create a ShpoolTerminalPool in a temp dir so config writes succeed.
fn test_pool(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir for shpool test");
    let socket_path = DaemonHostPath::new(dir.path().join("shpool.socket"));
    let pool = ShpoolTerminalPool::new(runner, socket_path);
    (pool, dir)
}

#[test]
fn write_config_writes_expected_content() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("config.toml");
    assert!(ShpoolTerminalPool::write_config(&config_path));
    let content = std::fs::read_to_string(&config_path).expect("config should have been written");
    assert!(content.contains("prompt_prefix = \"\""));
    assert!(content.contains("TERMINFO"));
    assert!(content.contains("COLORTERM"));
}

#[test]
fn config_needs_update_tracks_staleness() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("config.toml");

    // File doesn't exist → needs update
    assert!(ShpoolTerminalPool::config_needs_update(&config_path));

    // Write config, now it matches → no update needed
    ShpoolTerminalPool::write_config(&config_path);
    assert!(!ShpoolTerminalPool::config_needs_update(&config_path));

    // Modify externally → needs update again
    std::fs::write(&config_path, "stale config").expect("write stale");
    assert!(ShpoolTerminalPool::config_needs_update(&config_path));
}

#[test]
fn parse_list_json_with_flotilla_named_sessions() {
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

    let sessions = ShpoolTerminalPool::parse_list_json(json).unwrap();
    assert_eq!(sessions.len(), 2); // user-manual-session filtered out

    assert_eq!(sessions[0].session_name, "flotilla/my-feature/shell/0");
    assert_eq!(sessions[0].status, TerminalStatus::Running);

    assert_eq!(sessions[1].session_name, "flotilla/my-feature/agent/0");
    assert_eq!(sessions[1].status, TerminalStatus::Disconnected);
}

#[test]
fn parse_list_json_with_slashy_branch_names() {
    let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/feature/foo/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/feat/deep/nested/agent/1",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                }
            ]
    }"#;

    let sessions = ShpoolTerminalPool::parse_list_json(json).unwrap();
    assert_eq!(sessions.len(), 2);

    assert_eq!(sessions[0].session_name, "flotilla/feature/foo/shell/0");
    assert_eq!(sessions[1].session_name, "flotilla/feat/deep/nested/agent/1");
}

#[test]
fn parse_list_json_empty_sessions() {
    let json = r#"{"sessions": []}"#;
    let sessions = ShpoolTerminalPool::parse_list_json(json).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn parse_list_json_invalid_json() {
    assert!(ShpoolTerminalPool::parse_list_json("not json").is_err());
}

// --- TerminalPool tests (via session names) ---

#[tokio::test]
async fn list_sessions_parses_json() {
    let json = r#"{
            "sessions": [
                {"name": "flotilla/feat/shell/0", "started_at_unix_ms": 1709900000000, "status": "Attached"},
                {"name": "flotilla/feat/agent/0", "started_at_unix_ms": 1709900001000, "status": "Disconnected"},
                {"name": "user-manual", "started_at_unix_ms": 1709900002000, "status": "Attached"}
            ]
    }"#;
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![Ok(json.into())])));

    let sessions = TerminalPool::list_sessions(&pool).await.expect("list sessions");

    assert_eq!(sessions.len(), 2); // user-manual filtered out
    assert_eq!(sessions[0].session_name, "flotilla/feat/shell/0");
    assert_eq!(sessions[0].status, TerminalStatus::Running);
    assert!(sessions[0].command.is_none());
    assert!(sessions[0].working_directory.is_none());
    assert_eq!(sessions[1].session_name, "flotilla/feat/agent/0");
    assert_eq!(sessions[1].status, TerminalStatus::Disconnected);
}

#[tokio::test]
async fn attach_builds_command() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));

    let cwd = ExecutionEnvironmentPath::new("/home/dev");
    let cmd = TerminalPool::attach_command(&pool, "flotilla/feat/shell/0", "bash", &cwd, &vec![]).await.expect("attach");

    assert!(cmd.contains("shpool"), "should reference shpool binary: {cmd}");
    assert!(cmd.contains("attach"), "should include attach subcommand: {cmd}");
    assert!(cmd.contains("--force"), "should include --force: {cmd}");
    assert!(!cmd.contains("--cmd"), "should NOT have --cmd: {cmd}");
    assert!(cmd.contains("--dir"), "should include --dir: {cmd}");
    assert!(cmd.contains("/home/dev"), "should include cwd: {cmd}");
    assert!(cmd.ends_with("flotilla/feat/shell/0"), "session name should be last: {cmd}");
}

#[tokio::test]
async fn kill_calls_cli() {
    let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
    let (pool, _dir) = test_pool(runner.clone());

    TerminalPool::kill_session(&pool, "flotilla/feat/shell/0").await.expect("kill session");

    assert_eq!(runner.remaining(), 0, "kill command should have consumed the response");
}

// ── attach_args tests ──────────────────────────────────────────

#[test]
fn attach_args_with_command_no_env() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
    let socket = pool.socket_path.as_path().display().to_string();
    let config = pool.config_path.as_path().display().to_string();
    let args =
        pool.attach_args("flotilla/feat/shell/0", "bash", &ExecutionEnvironmentPath::new("/home/dev"), &vec![]).expect("attach_args");

    assert_eq!(args, vec![
        Arg::Literal("shpool".into()),
        Arg::Literal("--socket".into()),
        Arg::Quoted(socket),
        Arg::Literal("-c".into()),
        Arg::Quoted(config),
        Arg::Literal("attach".into()),
        Arg::Literal("--force".into()),
        Arg::Literal("--dir".into()),
        Arg::Quoted("/home/dev".into()),
        Arg::Literal("flotilla/feat/shell/0".into()),
    ]);
}

#[test]
fn attach_args_flatten_with_command_no_env() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
    let args =
        pool.attach_args("flotilla/feat/shell/0", "bash", &ExecutionEnvironmentPath::new("/home/dev"), &vec![]).expect("attach_args");
    let flat = flotilla_protocol::arg::flatten(&args, 0);

    assert!(flat.starts_with("shpool --socket "), "should start with shpool: {flat}");
    assert!(flat.contains("attach"), "should include attach: {flat}");
    assert!(flat.contains("--force"), "should include --force: {flat}");
    assert!(!flat.contains("--cmd"), "should NOT have --cmd: {flat}");
    assert!(flat.contains("--dir"), "should include --dir: {flat}");
    assert!(flat.contains("'/home/dev'"), "should include cwd: {flat}");
    assert!(flat.ends_with("flotilla/feat/shell/0"), "session name should be last: {flat}");
}

#[test]
fn attach_args_empty_command_no_env() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
    let args = pool.attach_args("sess", "", &ExecutionEnvironmentPath::new("/wd"), &vec![]).expect("attach_args");

    // Same structure as with command — command was baked in at ensure_session time
    assert!(args.iter().any(|a| matches!(a, Arg::Literal(s) if s == "--force")), "should have --force");
    assert!(!args.iter().any(|a| matches!(a, Arg::Literal(s) if s == "--cmd")), "should NOT have --cmd");
}

#[test]
fn attach_args_with_env_vars() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
    let env = vec![("FOO".to_string(), "bar".to_string())];
    let args = pool.attach_args("sess", "cmd", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

    // Env vars are baked in at ensure_session time — not in attach_args
    assert!(!args.iter().any(|a| matches!(a, Arg::NestedCommand(_))), "should have no NestedCommand");
    assert!(!args.iter().any(|a| matches!(a, Arg::Literal(s) if s == "--cmd")), "should NOT have --cmd");
}

#[test]
fn attach_args_with_env_vars_empty_command() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
    let env = vec![("KEY".to_string(), "val".to_string())];
    let args = pool.attach_args("sess", "", &ExecutionEnvironmentPath::new("/wd"), &env).expect("attach_args");

    // Same as above — env vars ignored in attach_args
    assert!(!args.iter().any(|a| matches!(a, Arg::NestedCommand(_))), "should have no NestedCommand");
    assert!(!args.iter().any(|a| matches!(a, Arg::Literal(s) if s == "--cmd")), "should NOT have --cmd");
}

// ── ensure_session tests ───────────────────────────────────────

/// Empty list response for ensure_session tests (session doesn't exist yet).
const EMPTY_LIST_JSON: &str = r#"{"sessions": []}"#;

#[tokio::test]
async fn ensure_session_creates_via_attach_then_detach() {
    let runner = Arc::new(MockRunner::new(vec![
        Ok(EMPTY_LIST_JSON.into()), // list_sessions: session doesn't exist
        Ok(String::new()),          // attach response
        Ok(String::new()),          // detach response
    ]));
    let (pool, _dir) = test_pool(runner.clone());
    let env = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), "test-uuid".to_string())];

    pool.ensure_session("test-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env).await.expect("ensure_session");

    let calls = runner.calls();
    assert_eq!(calls.len(), 3, "should call list, attach, detach: {calls:?}");

    // Second call (index 1): shpool attach with --cmd
    let attach_args = &calls[1].1;
    assert!(attach_args.contains(&"attach".to_string()));
    assert!(attach_args.contains(&"--cmd".to_string()));
    assert!(attach_args.contains(&"test-session".to_string()));

    // The --cmd value should contain the resolved shell (not ${SHELL:-/bin/sh})
    let cmd_idx = attach_args.iter().position(|a| a == "--cmd").expect("--cmd present");
    let cmd_val = &attach_args[cmd_idx + 1];
    assert!(!cmd_val.contains("${SHELL"), "should not contain unresolved shell variable: {cmd_val}");
    assert!(cmd_val.contains("FLOTILLA_ATTACHABLE_ID"), "should contain env var: {cmd_val}");

    // Third call (index 2): shpool detach
    let detach_args = &calls[2].1;
    assert!(detach_args.contains(&"detach".to_string()));
    assert!(detach_args.contains(&"test-session".to_string()));
}

#[tokio::test]
async fn ensure_session_skips_if_session_exists() {
    let existing_list = r#"{"sessions": [{"name": "test-session", "started_at_unix_ms": 1709900000000, "status": "Attached"}]}"#;
    let runner = Arc::new(MockRunner::new(vec![
        Ok(existing_list.into()), // session_exists: session found
    ]));
    let (pool, _dir) = test_pool(runner.clone());
    let env = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), "test-uuid".to_string())];

    pool.ensure_session("test-session", "claude", &ExecutionEnvironmentPath::new("/repo"), &env).await.expect("ensure_session");

    let calls = runner.calls();
    assert_eq!(calls.len(), 1, "should only call list (session_exists), not attach: {calls:?}");
    // The single call should be shpool list --json
    assert!(calls[0].1.contains(&"list".to_string()), "should be a list call: {:?}", calls[0].1);
}

#[tokio::test]
async fn ensure_session_empty_command_starts_login_shell() {
    let runner = Arc::new(MockRunner::new(vec![
        Ok(EMPTY_LIST_JSON.into()), // list_sessions
        Ok(String::new()),          // attach
        Ok(String::new()),          // detach
    ]));
    let (pool, _dir) = test_pool(runner.clone());

    pool.ensure_session("test-session", "", &ExecutionEnvironmentPath::new("/repo"), &vec![]).await.expect("ensure_session");

    let calls = runner.calls();
    let cmd_idx = calls[1].1.iter().position(|a| a == "--cmd").expect("--cmd present");
    let cmd_val = &calls[1].1[cmd_idx + 1];
    assert!(!cmd_val.contains("-lic"), "empty command should not have -lic: {cmd_val}");
}
