use bollard::vt::{passthrough::PassthroughVtEngine, VtEngine};

#[test]
fn passthrough_engine_accepts_bytes_and_reports_no_replay_support() {
    let mut engine = PassthroughVtEngine::new(80, 24);
    engine.feed(b"\x1b[31mhello\x1b[0m").expect("feed bytes");

    assert_eq!(engine.bytes_seen(), 14);
    assert!(!engine.supports_replay());
    assert_eq!(engine.replay_payload().expect("replay payload"), None);
}

#[test]
fn passthrough_engine_tracks_resize_without_generating_restore_payload() {
    let mut engine = PassthroughVtEngine::new(80, 24);
    engine.resize(132, 40).expect("resize");

    assert_eq!(engine.size(), (132, 40));
    assert_eq!(engine.replay_payload().expect("replay payload"), None);
}
