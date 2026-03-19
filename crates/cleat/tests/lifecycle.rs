use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use clap::Parser;
use cleat::{
    cli::{self, Cli},
    protocol::{Frame, SessionInfo},
    runtime::RuntimeLayout,
    server::SessionService,
    session::{daemon_pid_path, foreground_path, session_socket_path},
    vt::{ClientCapabilities, ColorLevel},
};

fn service_for(path: &std::path::Path) -> SessionService {
    SessionService::new(RuntimeLayout::new(path.to_path_buf()))
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn create_makes_session_directory_and_returns_metadata() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "alpha", "--cmd", "bash"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    assert_eq!(output, "alpha");
    assert!(temp.path().join("alpha").exists());
}

#[test]
fn create_json_returns_structured_metadata() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "--json", "alpha", "--cmd", "bash"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    let created: SessionInfo = serde_json::from_str(&output).expect("parse create output");

    assert_eq!(created.id, "alpha");
    assert_eq!(created.cmd.as_deref(), Some("bash"));
}

#[test]
fn list_reports_existing_sessions() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(PathBuf::from("/repo")), None).expect("create alpha");
    service.create(Some("beta".into()), None, Some("zsh".into())).expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let lines: Vec<_> = output.lines().collect();

    assert_eq!(lines, vec!["alpha\tdetached\t/repo", "beta\tdetached\tzsh"]);
}

#[test]
fn list_json_reports_existing_sessions() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(PathBuf::from("/repo")), None).expect("create alpha");
    service.create(Some("beta".into()), None, Some("zsh".into())).expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let listed: Vec<SessionInfo> = serde_json::from_str(&output).expect("parse list output");

    assert_eq!(listed.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
}

#[test]
fn kill_removes_session_directory() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None).expect("create alpha");
    let cli = Cli::try_parse_from(["cleat", "kill", "alpha"]).expect("parse kill");

    let output = cli::execute(cli, &service).expect("execute kill");

    assert_eq!(output, None);
    assert!(!temp.path().join("alpha").exists());
}

#[test]
fn kill_missing_session_is_an_error() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "kill", "missing"]).expect("parse kill");

    let err = cli::execute(cli, &service).expect_err("missing kill should fail");

    assert!(err.contains("missing"));
}

#[test]
fn attach_creates_session_lazily_and_reuses_it_on_later_attach() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (first, attach) = service.attach(Some("alpha".into()), None, Some("sleep 5".into()), false).expect("first attach");
    assert_eq!(first.id, "alpha");
    assert!(daemon_pid_path(temp.path(), "alpha").exists());

    drop(attach);

    let (second, _attach2) = service.attach(Some("alpha".into()), None, None, false).expect("reattach");
    assert_eq!(second.id, "alpha");
}

#[test]
fn attach_rejects_second_foreground_client_while_one_is_active() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, _attach) = service.attach(Some("alpha".into()), None, Some("sleep 5".into()), false).expect("first attach");
    let err = service.attach(Some("alpha".into()), None, None, false).expect_err("second attach should fail");

    assert!(err.contains("foreground client"));
}

#[test]
fn lifecycle_attach_init_with_capabilities_is_accepted_without_changing_single_client_policy() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, Some("sleep 5".into())).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");

    let response = Frame::read(&mut stream).expect("read attach response");
    assert_eq!(response, Frame::Ack);

    let err = service.attach(Some("alpha".into()), None, None, false).expect_err("second attach should fail");
    assert!(err.contains("foreground client"));
}

#[test]
fn lifecycle_attach_init_capabilities_drive_replay_output_on_daemon_path() {
    let _lock = env_lock().lock().expect("env lock");
    let _guard = EnvVarGuard::set("CLEAT_TEST_VT_ENGINE", "replay-probe");

    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, Some("sleep 5".into())).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");

    let response = Frame::read(&mut stream).expect("read attach response");
    assert_eq!(response, Frame::Ack);

    let replay = Frame::read(&mut stream).expect("read replay output");
    assert_eq!(replay, Frame::Output(b"Ansi256:true".to_vec()));
}

#[test]
fn dropping_foreground_attach_keeps_session_alive_for_later_attach() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, attach) = service.attach(Some("alpha".into()), None, Some("sleep 5".into()), false).expect("first attach");
    let pid_path = daemon_pid_path(temp.path(), "alpha");
    assert!(pid_path.exists());

    drop(attach);

    let (_session, _reattach) = service.attach(Some("alpha".into()), None, None, false).expect("reattach after disconnect");
    assert!(pid_path.exists());
}

#[test]
fn stale_foreground_file_does_not_block_attach() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, Some("sleep 5".into())).expect("create alpha");
    std::fs::write(foreground_path(temp.path(), "alpha"), b"999999").expect("write stale foreground marker");

    let (_session, _attach) = service.attach(Some("alpha".into()), None, None, false).expect("attach with stale foreground marker");
}

#[test]
fn attach_no_create_rejects_missing_session() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "attach", "--no-create", "missing"]).expect("parse attach");

    let err = cli::execute(cli, &service).expect_err("missing attach should fail");

    assert!(err.contains("missing"));
}
