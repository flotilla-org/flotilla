use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use clap::Parser;
use cleat::{
    cli::{self, Cli},
    protocol::{Frame, SessionInfo},
    runtime::RuntimeLayout,
    server::SessionService,
    session::{daemon_pid_path, foreground_path, session_socket_path},
    vt::{self, ClientCapabilities, ColorLevel, VtEngineKind},
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
    assert_eq!(created.vt_engine, vt::default_vt_engine_kind());
    assert_eq!(created.cmd.as_deref(), Some("bash"));
}

#[test]
fn create_uses_requested_vt_engine() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "--json", "--vt", "passthrough", "alpha"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    let created: SessionInfo = serde_json::from_str(&output).expect("parse create output");

    assert_eq!(created.vt_engine, VtEngineKind::Passthrough);
}

#[cfg(not(feature = "ghostty-vt"))]
#[test]
fn create_rejects_unavailable_vt_engine() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "--vt", "ghostty", "alpha"]).expect("parse create");

    let err = cli::execute(cli, &service).expect_err("ghostty should be unavailable");

    assert!(err.contains("not compiled"));
}

#[test]
fn list_reports_existing_sessions() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, Some(PathBuf::from("/repo")), None).expect("create alpha");
    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("zsh".into())).expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let lines: Vec<_> = output.lines().collect();

    assert_eq!(lines, vec![
        format!("alpha\tdetached\t{}\t/repo", vt::default_vt_engine_kind().as_str()),
        "beta\tdetached\tpassthrough\tzsh".to_string(),
    ]);
}

#[test]
fn list_json_reports_existing_sessions() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, Some(PathBuf::from("/repo")), None).expect("create alpha");
    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("zsh".into())).expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let listed: Vec<SessionInfo> = serde_json::from_str(&output).expect("parse list output");

    assert_eq!(listed.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
    assert_eq!(listed[0].vt_engine, vt::default_vt_engine_kind());
    assert_eq!(listed[1].vt_engine, VtEngineKind::Passthrough);
}

#[test]
fn kill_removes_session_directory() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, None).expect("create alpha");
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

    let (first, attach) = service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("first attach");
    assert_eq!(first.id, "alpha");
    assert_eq!(first.vt_engine, vt::default_vt_engine_kind());
    assert!(daemon_pid_path(temp.path(), "alpha").exists());

    drop(attach);

    let (second, _attach2) = service.attach(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, None, false).expect("reattach");
    assert_eq!(second.id, "alpha");
    assert_eq!(second.vt_engine, vt::default_vt_engine_kind());
}

#[test]
fn attach_vt_only_applies_when_creating_new_session() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (created, attach) =
        service.attach(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 5".into()), false).expect("first attach");
    assert_eq!(created.vt_engine, VtEngineKind::Passthrough);
    drop(attach);

    let (reattached, _attach2) =
        service.attach(Some("alpha".into()), Some(vt::default_vt_engine_kind()), None, None, false).expect("reattach");
    assert_eq!(reattached.vt_engine, VtEngineKind::Passthrough);
}

#[test]
fn attach_rejects_second_foreground_client_while_one_is_active() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, _attach) = service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("first attach");
    let err = service.attach(Some("alpha".into()), None, None, None, false).expect_err("second attach should fail");

    assert!(err.contains("foreground client"));
}

#[test]
fn lifecycle_attach_init_with_capabilities_is_accepted_without_changing_single_client_policy() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, None, Some("sleep 5".into())).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");

    let response = Frame::read(&mut stream).expect("read attach response");
    assert_eq!(response, Frame::Ack);

    let err = service.attach(Some("alpha".into()), None, None, None, false).expect_err("second attach should fail");
    assert!(err.contains("foreground client"));
}

#[test]
fn lifecycle_attach_init_capabilities_drive_replay_output_on_daemon_path() {
    let _lock = env_lock().lock().expect("env lock");
    let _guard = EnvVarGuard::set("CLEAT_TEST_VT_ENGINE", "replay-probe");

    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 5".into())).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");

    let response = Frame::read(&mut stream).expect("read attach response");
    assert_eq!(response, Frame::Ack);

    let replay = Frame::read(&mut stream).expect("read replay output");
    assert_eq!(replay, Frame::Output(b"Ansi256:true".to_vec()));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn replay_reattach_delivers_restore_before_new_live_output() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), None, None, Some("printf 'before'; sleep 1; printf 'after'; sleep 5".into()))
        .expect("create alpha");

    let mut first = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect first socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut first)
        .expect("write first attach init");
    assert_eq!(Frame::read(&mut first).expect("read first attach response"), Frame::Ack);

    let first_live = Frame::read(&mut first).expect("read first live output");
    let first_live_bytes = match first_live {
        Frame::Output(bytes) => bytes,
        other => panic!("expected first live output, got {other:?}"),
    };
    assert!(String::from_utf8_lossy(&first_live_bytes).contains("before"));
    drop(first);

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut second = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect second socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut second)
        .expect("write second attach init");
    assert_eq!(Frame::read(&mut second).expect("read second attach response"), Frame::Ack);

    let replay = Frame::read(&mut second).expect("read replay output");
    let replay_bytes = match replay {
        Frame::Output(bytes) => bytes,
        other => panic!("expected replay output, got {other:?}"),
    };
    let replay_text = String::from_utf8_lossy(&replay_bytes);
    assert!(replay_text.contains("before"), "replay should include prior output: {replay_text:?}");
    assert!(!replay_text.contains("after"), "replay should arrive before later live output: {replay_text:?}");

    let live = loop {
        match Frame::read(&mut second).expect("read live output after replay") {
            Frame::Output(bytes) if String::from_utf8_lossy(&bytes).contains("after") => break bytes,
            Frame::Output(_) => continue,
            other => panic!("expected output frame after replay, got {other:?}"),
        }
    };
    assert!(String::from_utf8_lossy(&live).contains("after"));
}

#[test]
fn dropping_foreground_attach_keeps_session_alive_for_later_attach() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, attach) = service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("first attach");
    let pid_path = daemon_pid_path(temp.path(), "alpha");
    assert!(pid_path.exists());

    drop(attach);

    let (_session, _reattach) = service.attach(Some("alpha".into()), None, None, None, false).expect("reattach after disconnect");
    assert!(pid_path.exists());
}

#[test]
fn stale_foreground_file_does_not_block_attach() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, None, Some("sleep 5".into())).expect("create alpha");
    std::fs::write(foreground_path(temp.path(), "alpha"), b"999999").expect("write stale foreground marker");

    let (_session, _attach) = service.attach(Some("alpha".into()), None, None, None, false).expect("attach with stale foreground marker");
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

#[test]
fn cleat_attach_exits_when_session_is_killed() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 30".into())).expect("create alpha");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut child = Command::new(cleat_bin)
        .arg("--runtime-root")
        .arg(temp.path())
        .arg("attach")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cleat attach");
    let _stdin = child.stdin.take().expect("attach stdin");

    let attach_deadline = Instant::now() + Duration::from_secs(2);
    while !foreground_path(temp.path(), "alpha").exists() && Instant::now() < attach_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(foreground_path(temp.path(), "alpha").exists(), "attach should establish a foreground client before kill");

    service.kill("alpha").expect("kill session");

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait attach child") {
            assert!(status.success(), "attach should exit cleanly after session kill: {status:?}");
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cleat attach did not exit after session kill");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cleat_detach_exits_foreground_client_and_keeps_session_alive() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 30".into())).expect("create alpha");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut child = Command::new(cleat_bin)
        .arg("--runtime-root")
        .arg(temp.path())
        .arg("attach")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cleat attach");
    let _stdin = child.stdin.take().expect("attach stdin");

    let attach_deadline = Instant::now() + Duration::from_secs(2);
    while !foreground_path(temp.path(), "alpha").exists() && Instant::now() < attach_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(foreground_path(temp.path(), "alpha").exists(), "attach should establish a foreground client before detach");

    service.detach("alpha").expect("detach session");

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait attach child") {
            assert!(status.success(), "attach should exit cleanly after detach: {status:?}");
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cleat attach did not exit after detach");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!foreground_path(temp.path(), "alpha").exists(), "detach should clear the foreground marker");
    assert!(temp.path().join("alpha").exists(), "detach should leave the session directory intact");
}

#[test]
fn cleat_attach_exits_on_sigterm_and_keeps_session_alive() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 30".into())).expect("create alpha");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut child = Command::new(cleat_bin)
        .arg("--runtime-root")
        .arg(temp.path())
        .arg("attach")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cleat attach");
    let _stdin = child.stdin.take().expect("attach stdin");

    let attach_deadline = Instant::now() + Duration::from_secs(2);
    while !foreground_path(temp.path(), "alpha").exists() && Instant::now() < attach_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(foreground_path(temp.path(), "alpha").exists(), "attach should establish a foreground client before signal exit");

    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "send SIGTERM to attach process");

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait attach child") {
            assert!(status.success(), "attach should exit cleanly after SIGTERM: {status:?}");
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cleat attach did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let cleared_deadline = Instant::now() + Duration::from_secs(2);
    while foreground_path(temp.path(), "alpha").exists() && Instant::now() < cleared_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!foreground_path(temp.path(), "alpha").exists(), "signal exit should clear the foreground marker");
    assert!(temp.path().join("alpha").exists(), "signal exit should leave the session directory intact");
}

#[test]
fn short_lived_session_reaps_its_directory_after_child_exit() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("printf done; sleep 0.1".into())).expect("create alpha");

    let session_dir = temp.path().join("alpha");
    let deadline = Instant::now() + Duration::from_secs(2);
    while session_dir.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!session_dir.exists(), "session directory should be reaped after child exit");
}
