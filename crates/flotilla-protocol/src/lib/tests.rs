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
fn wire_generation_match_requires_equal_known_stamps() {
    assert!(wire_generations_match("build-a", "build-a"));
    assert!(!wire_generations_match("build-a", "build-b"));
    assert!(!wire_generations_match("unknown", "unknown"));
}
