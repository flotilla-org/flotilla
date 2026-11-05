use super::*;

async fn hello_result(
    protocol_version: u32,
    daemon_build: &str,
    daemon_fingerprint: &str,
    policy: WireGenerationPolicy,
) -> Result<Option<String>, String> {
    let (client, server) = flotilla_transport::message::message_session_pair();
    let display_name = flotilla_protocol::hello_display_name("daemon", daemon_build, daemon_fingerprint);
    let server_task = tokio::spawn(async move {
        // Diagnostic identity travels in Hello; protocol compatibility remains independent.
        let Some(Message::Hello { display_name: client_name, .. }) = server.read().await.expect("read client hello") else {
            panic!("expected client Hello");
        };
        let info = flotilla_protocol::hello_build_info(&client_name).expect("client build metadata");
        assert_eq!(info.build_id, build_id());
        assert_eq!(info.protocol_fingerprint, PROTOCOL_FINGERPRINT);
        server
            .write(Message::Hello {
                protocol_version,
                node_id: NodeId::new("daemon"),
                display_name,
                session_id: uuid::Uuid::nil(),
                connection_role: Some(ConnectionRole::Client),
                surface: None,
                crew_claims: Default::default(),
            })
            .await
            .expect("write daemon hello");
    });
    let result = do_client_hello_with_surface(&client, None, policy).await;
    server_task.await.expect("join hello server");
    result
}

#[tokio::test]
async fn normal_hello_gates_all_protocol_version_and_fingerprint_combinations() {
    assert_eq!(
        hello_result(PROTOCOL_VERSION, "different-build", PROTOCOL_FINGERPRINT, WireGenerationPolicy::RequireMatch)
            .await
            .expect("matching version and fingerprint"),
        Some("different-build".to_string()),
        "build identity must not gate compatibility"
    );

    let fingerprint_error = hello_result(PROTOCOL_VERSION, "daemon-build", "different-fingerprint", WireGenerationPolicy::RequireMatch)
        .await
        .expect_err("matching version with mismatched fingerprint must fail");
    assert!(fingerprint_error.contains("wire generation mismatch"), "unexpected error: {fingerprint_error}");
    assert!(fingerprint_error.contains(PROTOCOL_FINGERPRINT), "missing client fingerprint: {fingerprint_error}");
    assert!(fingerprint_error.contains("different-fingerprint"), "missing daemon fingerprint: {fingerprint_error}");
    assert!(fingerprint_error.contains(build_id()), "missing client build: {fingerprint_error}");
    assert!(fingerprint_error.contains("daemon-build"), "missing daemon build: {fingerprint_error}");

    let version_error = hello_result(PROTOCOL_VERSION + 1, "daemon-build", PROTOCOL_FINGERPRINT, WireGenerationPolicy::RequireMatch)
        .await
        .expect_err("mismatched version with matching fingerprint must fail");
    assert!(version_error.contains("protocol version mismatch"), "unexpected error: {version_error}");

    let both_error = hello_result(PROTOCOL_VERSION + 1, "daemon-build", "different-fingerprint", WireGenerationPolicy::RequireMatch)
        .await
        .expect_err("mismatched version and fingerprint must fail");
    assert!(both_error.contains("protocol version mismatch"), "unexpected error: {both_error}");
    assert!(both_error.contains(PROTOCOL_FINGERPRINT), "missing client fingerprint: {both_error}");
    assert!(both_error.contains("different-fingerprint"), "missing daemon fingerprint: {both_error}");
    assert!(both_error.contains(build_id()), "missing client build: {both_error}");
    assert!(both_error.contains("daemon-build"), "missing daemon build: {both_error}");
    assert!(both_error.contains(&PROTOCOL_VERSION.to_string()), "missing client protocol version: {both_error}");
    assert!(both_error.contains(&(PROTOCOL_VERSION + 1).to_string()), "missing daemon protocol version: {both_error}");
}

#[tokio::test]
async fn shutdown_hello_allows_only_same_protocol_version() {
    assert_eq!(
        hello_result(PROTOCOL_VERSION, "different-build", "different-fingerprint", WireGenerationPolicy::AllowMismatchForShutdown)
            .await
            .expect("same-version shutdown hello"),
        Some("different-build".to_string())
    );

    let version_error =
        hello_result(PROTOCOL_VERSION + 1, "different-build", "different-fingerprint", WireGenerationPolicy::AllowMismatchForShutdown)
            .await
            .expect_err("shutdown must reject a protocol-version mismatch");
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
    // #3244: selected recovery preserves the known cursor and does not invent
    // interest for a query this client has not subscribed to.
    assert_eq!(query_cursor(&subscriptions, &sequences, &query), Some(QueryCursor { query, since: Some(3) }));
    assert_eq!(query_cursor(&subscriptions, &sequences, &QueryId::Independents { scope: None }), None);
}

// #3244: the socket reader repairs only the gapped query and keeps sibling
// delta delivery alive. This peer stands in for the daemon message boundary.
#[tokio::test]
async fn socket_gap_recovery_requests_one_query_and_keeps_sibling_deltas() {
    use flotilla_protocol::result_set::{QueryChanges, ResultDelta, ResultSet, Rows};

    let (client, server) = flotilla_transport::message::message_session_pair();
    let daemon = SocketDaemon::from_session(client).expect("socket client");
    let mut events = daemon.subscribe();
    let convoy = QueryId::Convoys { scope: None };
    let sibling = QueryId::StandingRoles { scope: None };
    let cursors = vec![QueryCursor { query: convoy.clone(), since: None }, QueryCursor { query: sibling, since: None }];
    let (ready_tx, ready_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let Some(Message::Request { id, request: Request::SubscribeQueries { queries } }) = server.read().await.expect("read setup") else {
            panic!("subscription setup");
        };
        assert_eq!(queries.len(), 2);
        let set = |seq, rows| DaemonEvent::ResultSet(Box::new(ResultSet { seq, rows, state: Default::default() }));
        server
            .write(Message::ok_response(
                id,
                Response::SubscribeQueries(vec![
                    set(3, Rows::Convoys { scope: None, rows: vec![] }),
                    set(5, Rows::StandingRoles { scope: None, rows: vec![] }),
                ]),
            ))
            .await
            .expect("seed cursors");
        ready_rx.await.expect("subscription seeded");
        server
            .write(Message::Event {
                event: Box::new(DaemonEvent::ResultDelta(Box::new(ResultDelta {
                    seq: 9,
                    changes: QueryChanges::Convoys { scope: None, changed: vec![], removed: vec![] },
                    state: None,
                }))),
            })
            .await
            .expect("send gap");
        let Some(Message::Request { id, request: Request::RecoverQueries { queries } }) = server.read().await.expect("read recovery")
        else {
            panic!("targeted recovery request");
        };
        assert_eq!(queries, vec![QueryCursor { query: QueryId::Convoys { scope: None }, since: Some(3) }]);
        server
            .write(Message::ok_response(id, Response::RecoverQueries(vec![set(9, Rows::Convoys { scope: None, rows: vec![] })])))
            .await
            .expect("recover snapshot");
        server
            .write(Message::Event {
                event: Box::new(DaemonEvent::ResultDelta(Box::new(ResultDelta {
                    seq: 6,
                    changes: QueryChanges::StandingRoles { scope: None, changed: vec![], removed: vec![] },
                    state: None,
                }))),
            })
            .await
            .expect("sibling delta");
        done_rx.await.expect("events consumed before peer disconnects");
    });
    daemon.subscribe_queries(uuid::Uuid::nil(), &cursors).await.expect("subscribe");
    ready_tx.send(()).expect("ready");
    let mut recovered = false;
    let mut sibling_delta = false;
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.expect("event timeout").expect("event");
        match event {
            DaemonEvent::ResultSet(set) => {
                assert_eq!(set.seq, 9);
                recovered = true;
            }
            DaemonEvent::ResultDelta(delta) => {
                assert_eq!(delta.seq, 6);
                sibling_delta = true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(recovered && sibling_delta);
    done_tx.send(()).expect("done");
    peer.await.expect("peer");
}
