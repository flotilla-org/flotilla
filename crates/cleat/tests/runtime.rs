use std::path::PathBuf;

use cleat::{
    runtime::RuntimeLayout,
    vt::{self, VtEngineKind},
};

#[test]
fn named_sessions_use_supplied_name_as_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));

    let session = layout
        .create_session(Some("demo".into()), VtEngineKind::Passthrough, Some(PathBuf::from("/repo")), Some("bash".into()))
        .expect("create session");

    assert_eq!(session.metadata.id, "demo");
    assert_eq!(session.metadata.name.as_deref(), Some("demo"));
    assert_eq!(session.metadata.vt_engine, VtEngineKind::Passthrough);
    assert!(session.dir.ends_with("demo"));
}

#[test]
fn unnamed_sessions_get_generated_ids() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));

    let a = layout.create_session(None, VtEngineKind::Passthrough, None, None).expect("create session a");
    let b = layout.create_session(None, VtEngineKind::Passthrough, None, None).expect("create session b");

    assert_ne!(a.metadata.id, b.metadata.id);
    assert!(a.metadata.id.starts_with("session-"));
    assert!(b.metadata.id.starts_with("session-"));
}

#[test]
fn list_sessions_reads_metadata_from_per_session_directories() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));
    layout.create_session(Some("alpha".into()), vt::default_vt_engine_kind(), None, None).expect("create alpha");
    layout.create_session(Some("beta".into()), VtEngineKind::Passthrough, None, Some("zsh".into())).expect("create beta");

    let sessions = layout.list_sessions().expect("list sessions");

    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].metadata.id, "alpha");
    assert_eq!(sessions[1].metadata.id, "beta");
    assert_eq!(sessions[0].metadata.vt_engine, vt::default_vt_engine_kind());
    assert_eq!(sessions[1].metadata.vt_engine, VtEngineKind::Passthrough);
    assert_eq!(sessions[1].metadata.cmd.as_deref(), Some("zsh"));
}

#[test]
fn list_sessions_defaults_missing_vt_engine_for_older_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));
    std::fs::create_dir_all(layout.root().join("legacy")).expect("create session dir");
    std::fs::write(layout.root().join("legacy").join("meta.json"), r#"{"id":"legacy","name":"legacy","cwd":null,"cmd":null}"#)
        .expect("write legacy metadata");

    let sessions = layout.list_sessions().expect("list sessions");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].metadata.id, "legacy");
    assert_eq!(sessions[0].metadata.vt_engine, vt::default_vt_engine_kind());
}
