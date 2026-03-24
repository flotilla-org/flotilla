use std::sync::Arc;

use super::*;
use crate::providers::testing::MockRunner;

/// Create a ShpoolTerminalPool in a temp dir so config writes succeed.
fn test_pool(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir for shpool test");
    let socket_path = dir.path().join("shpool.socket");
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

    let cmd =
        TerminalPool::attach_command(&pool, "flotilla/feat/shell/0", "bash", Path::new("/home/dev"), &vec![]).await.expect("attach");

    assert!(cmd.contains("shpool"), "should reference shpool binary: {cmd}");
    assert!(cmd.contains("attach"), "should include attach subcommand: {cmd}");
    assert!(cmd.contains("--cmd"), "should include --cmd for non-empty command: {cmd}");
    assert!(cmd.contains("-lic"), "should use login interactive shell: {cmd}");
    assert!(cmd.contains("bash"), "should contain original command: {cmd}");
    assert!(cmd.contains("--dir"), "should include --dir: {cmd}");
    assert!(cmd.contains("/home/dev"), "should include cwd: {cmd}");
    assert!(cmd.contains("'flotilla/feat/shell/0'"), "session name should be last: {cmd}");
}

#[tokio::test]
async fn kill_calls_cli() {
    let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
    let (pool, _dir) = test_pool(runner.clone());

    TerminalPool::kill_session(&pool, "flotilla/feat/shell/0").await.expect("kill session");

    assert_eq!(runner.remaining(), 0, "kill command should have consumed the response");
}
