use super::*;

#[test]
fn list_repos_request_roundtrips() {
    let message = Message::Request { id: 42, request: Request::ListRepos };
    let json = serde_json::to_string(&message).expect("serialize");
    let decoded: Message = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, Message::Request { id: 42, request: Request::ListRepos }));
}

#[test]
fn artifact_bodies_are_base64_strings_on_the_wire() {
    let body = vec![0, 1, 127, 255];
    let request = Request::ArtifactPut {
        kind: "recording".into(),
        subject: "head".into(),
        summary: Default::default(),
        media_type: "application/octet-stream".into(),
        body: body.clone(),
    };
    let encoded = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(encoded["params"]["body"], "AAF//w==");
    assert!(
        matches!(serde_json::from_value::<Request>(encoded).expect("decode request"), Request::ArtifactPut { body: decoded, .. } if decoded == body)
    );

    let response = Response::ArtifactGet { body: body.clone() };
    let encoded = serde_json::to_value(&response).expect("serialize response");
    assert_eq!(encoded["data"]["body"], "AAF//w==");
    assert!(
        matches!(serde_json::from_value::<Response>(encoded).expect("decode response"), Response::ArtifactGet { body: decoded } if decoded == body)
    );
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
