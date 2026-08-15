use super::*;

async fn hello_result(protocol_version: u32, daemon_build: &str, policy: WireGenerationPolicy) -> Result<Option<String>, String> {
    let (client, server) = flotilla_transport::message::message_session_pair();
    let display_name = flotilla_protocol::hello_display_name("daemon", daemon_build);
    let server_task = tokio::spawn(async move {
        assert!(matches!(server.read().await.expect("read client hello"), Some(Message::Hello { .. })));
        server
            .write(Message::Hello {
                protocol_version,
                node_id: NodeId::new("daemon"),
                display_name,
                session_id: uuid::Uuid::nil(),
                connection_role: Some(ConnectionRole::Client),
                surface: None,
            })
            .await
            .expect("write daemon hello");
    });
    let result = do_client_hello_with_surface(&client, None, policy).await;
    server_task.await.expect("join hello server");
    result
}

#[tokio::test]
async fn shutdown_hello_allows_only_same_protocol_build_mismatch() {
    let different_build = "different-build";
    assert_eq!(
        hello_result(PROTOCOL_VERSION, different_build, WireGenerationPolicy::AllowMismatchForShutdown)
            .await
            .expect("same-protocol shutdown hello"),
        Some(different_build.to_string())
    );

    let strict_error = hello_result(PROTOCOL_VERSION, different_build, WireGenerationPolicy::RequireMatch)
        .await
        .expect_err("normal session must reject a build mismatch");
    assert!(strict_error.contains("wire generation mismatch"), "unexpected error: {strict_error}");

    let version_error = hello_result(PROTOCOL_VERSION + 1, different_build, WireGenerationPolicy::AllowMismatchForShutdown)
        .await
        .expect_err("shutdown must reject a protocol mismatch");
    assert!(version_error.contains("protocol version mismatch"), "unexpected error: {version_error}");
}

#[test]
fn replay_cursors_preserve_host_and_query_streams() {
    let host = StreamKey::Host { environment_id: flotilla_protocol::EnvironmentId::new("env-1") };
    let query = StreamKey::Query { query: QueryId::Convoys { scope: None } };
    let seen = HashMap::from([(host.clone(), 4), (query.clone(), 9)]);
    let cursors = encode_replay_cursors(&seen);
    assert!(cursors.contains(&ReplayCursor { stream: host, seq: 4 }));
    assert!(cursors.contains(&ReplayCursor { stream: query, seq: 9 }));
}

#[test]
fn query_cursors_use_known_sequences() {
    let query = QueryId::Convoys { scope: None };
    let subscriptions = Arc::new(std::sync::RwLock::new(HashSet::from([query.clone()])));
    let sequences = Arc::new(std::sync::RwLock::new(HashMap::from([(StreamKey::Query { query: query.clone() }, 3)])));
    assert_eq!(encode_query_cursors(&subscriptions, &sequences), vec![QueryCursor { query, since: Some(3) }]);
}
