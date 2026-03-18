use std::path::PathBuf;

use bollard::runtime::RuntimeLayout;

#[test]
fn named_sessions_use_supplied_name_as_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));

    let session = layout.create_session(Some("demo".into()), Some(PathBuf::from("/repo")), Some("bash".into())).expect("create session");

    assert_eq!(session.metadata.id, "demo");
    assert_eq!(session.metadata.name.as_deref(), Some("demo"));
    assert!(session.dir.ends_with("demo"));
}

#[test]
fn unnamed_sessions_get_generated_ids() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));

    let a = layout.create_session(None, None, None).expect("create session a");
    let b = layout.create_session(None, None, None).expect("create session b");

    assert_ne!(a.metadata.id, b.metadata.id);
    assert!(a.metadata.id.starts_with("session-"));
    assert!(b.metadata.id.starts_with("session-"));
}

#[test]
fn list_sessions_reads_metadata_from_per_session_directories() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));
    layout.create_session(Some("alpha".into()), None, None).expect("create alpha");
    layout.create_session(Some("beta".into()), None, Some("zsh".into())).expect("create beta");

    let sessions = layout.list_sessions().expect("list sessions");

    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].metadata.id, "alpha");
    assert_eq!(sessions[1].metadata.id, "beta");
    assert_eq!(sessions[1].metadata.cmd.as_deref(), Some("zsh"));
}
