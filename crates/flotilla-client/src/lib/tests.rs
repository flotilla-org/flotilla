use super::*;

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
