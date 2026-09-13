use super::*;

#[test]
fn list_repos_request_roundtrips() {
    let message = Message::Request { id: 42, request: Request::ListRepos };
    let json = serde_json::to_string(&message).expect("serialize");
    let decoded: Message = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, Message::Request { id: 42, request: Request::ListRepos }));
}

#[test]
fn host_replay_cursor_roundtrips() {
    let cursor = ReplayCursor { stream: StreamKey::Host { environment_id: EnvironmentId::new("env-1") }, seq: 7 };
    let json = serde_json::to_string(&cursor).expect("serialize");
    let decoded: ReplayCursor = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, cursor);
}

#[test]
fn hello_build_info_roundtrips_build_and_protocol_fingerprint() {
    let display_name = hello_display_name("client", "build-a", "fingerprint-a");
    assert_eq!(hello_build_info(&display_name), Some(HelloBuildInfo { build_id: "build-a", protocol_fingerprint: "fingerprint-a" }));
    assert_eq!(hello_build_info("client"), None);
}
