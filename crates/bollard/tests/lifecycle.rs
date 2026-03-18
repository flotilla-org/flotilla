use std::path::PathBuf;

use clap::Parser;
use bollard::{
    cli::{self, Cli},
    protocol::SessionInfo,
    runtime::RuntimeLayout,
    server::SessionService,
    session::daemon_pid_path,
};

fn service_for(path: &std::path::Path) -> SessionService {
    SessionService::new(RuntimeLayout::new(path.to_path_buf()))
}

#[test]
fn create_makes_session_directory_and_returns_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["bollard", "create", "alpha", "--cmd", "bash"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    assert_eq!(output, "alpha");
    assert!(temp.path().join("alpha").exists());
}

#[test]
fn create_json_returns_structured_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["bollard", "create", "--json", "alpha", "--cmd", "bash"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    let created: SessionInfo = serde_json::from_str(&output).expect("parse create output");

    assert_eq!(created.id, "alpha");
    assert_eq!(created.cmd.as_deref(), Some("bash"));
}

#[test]
fn list_reports_existing_sessions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(PathBuf::from("/repo")), None).expect("create alpha");
    service.create(Some("beta".into()), None, Some("zsh".into())).expect("create beta");
    let cli = Cli::try_parse_from(["bollard", "list"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let lines: Vec<_> = output.lines().collect();

    assert_eq!(lines, vec!["alpha\tdetached\t/repo", "beta\tdetached\tzsh"]);
}

#[test]
fn list_json_reports_existing_sessions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(PathBuf::from("/repo")), None).expect("create alpha");
    service.create(Some("beta".into()), None, Some("zsh".into())).expect("create beta");
    let cli = Cli::try_parse_from(["bollard", "list", "--json"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let listed: Vec<SessionInfo> = serde_json::from_str(&output).expect("parse list output");

    assert_eq!(listed.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
}

#[test]
fn kill_removes_session_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None).expect("create alpha");
    let cli = Cli::try_parse_from(["bollard", "kill", "alpha"]).expect("parse kill");

    let output = cli::execute(cli, &service).expect("execute kill");

    assert_eq!(output, None);
    assert!(!temp.path().join("alpha").exists());
}

#[test]
fn kill_missing_session_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["bollard", "kill", "missing"]).expect("parse kill");

    let err = cli::execute(cli, &service).expect_err("missing kill should fail");

    assert!(err.contains("missing"));
}

#[test]
fn attach_creates_session_lazily_and_reuses_it_on_later_attach() {
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
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, _attach) = service.attach(Some("alpha".into()), None, Some("sleep 5".into()), false).expect("first attach");
    let err = service.attach(Some("alpha".into()), None, None, false).expect_err("second attach should fail");

    assert!(err.contains("foreground client"));
}

#[test]
fn dropping_foreground_attach_keeps_session_alive_for_later_attach() {
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
fn attach_no_create_rejects_missing_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["bollard", "attach", "--no-create", "missing"]).expect("parse attach");

    let err = cli::execute(cli, &service).expect_err("missing attach should fail");

    assert!(err.contains("missing"));
}
