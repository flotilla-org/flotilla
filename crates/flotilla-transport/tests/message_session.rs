use std::time::Duration;

use flotilla_protocol::{Message, Request};
use flotilla_transport::message::{message_session_pair, stream_message_session, stream_message_session_with_prefix, MessageSession};
use tokio::io::{duplex, split, AsyncReadExt, AsyncWriteExt};

// Behaviour: every transport preserves message ordering in both directions,
// flushes each write, and reports EOF after the opposite endpoint is dropped.
async fn session_contract(pair: (MessageSession, MessageSession)) {
    tokio::time::timeout(Duration::from_secs(5), run_session_contract(pair)).await.expect("session contract timed out");
}

async fn run_session_contract((left, right): (MessageSession, MessageSession)) {
    for id in [0, 1, u64::MAX] {
        left.write(Message::Request { id, request: Request::GetTopology }).await.expect("send request");
        right.write(Message::Request { id, request: Request::GetTopology }).await.expect("send response");
    }
    for id in [0, 1, u64::MAX] {
        for session in [&left, &right] {
            assert!(
                matches!(session.read().await.expect("receive"), Some(Message::Request { id: received, request: Request::GetTopology }) if received == id)
            );
        }
    }
    drop(right);
    assert!(left.read().await.expect("clean EOF").is_none());
    // Stream implementations may observe closure after a buffered write.
    for _ in 0..32 {
        if left.write(Message::Request { id: 2, request: Request::GetTopology }).await.is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("writes must eventually observe the closed peer");
}

#[tokio::test]
async fn memory_contract() {
    session_contract(message_session_pair()).await;
}

#[tokio::test]
async fn duplex_contract() {
    let (left, right) = duplex(4096);
    let (lr, lw) = split(left);
    let (rr, rw) = split(right);
    session_contract((stream_message_session(lr, lw), stream_message_session(rr, rw))).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unix_contract() {
    use flotilla_transport::message::unix_message_session;
    let (left, right) = tokio::net::UnixStream::pair().expect("socket pair");
    session_contract((unix_message_session(left), unix_message_session(right))).await;
}

// Behaviour: consumed detection bytes are replayed even across a partial line;
// malformed JSON remains a protocol error and the wire is still one JSON line.
#[tokio::test]
async fn prefix_and_wire_contract() {
    tokio::time::timeout(Duration::from_secs(5), run_prefix_and_wire_contract()).await.expect("prefix contract timed out");
}

async fn run_prefix_and_wire_contract() {
    let message = Message::Request { id: 7, request: Request::GetTopology };
    let mut bytes = serde_json::to_vec(&message).expect("serialize");
    bytes.push(b'\n');
    for prefix_len in [0, 1, bytes.len() - 1, bytes.len()] {
        let (local, mut remote) = duplex(4096);
        let (reader, writer) = split(local);
        let session = stream_message_session_with_prefix(reader, writer, bytes[..prefix_len].to_vec());
        remote.write_all(&bytes[prefix_len..]).await.expect("write remainder");
        assert!(matches!(session.read().await.expect("prefixed message"), Some(Message::Request { id: 7, .. })));
        remote.write_all(b"invalid\n").await.expect("write malformed message");
        assert!(session.read().await.expect_err("invalid JSON").contains("failed to parse message"));
        session.write(message.clone()).await.expect("write message");
        let mut actual = vec![0; bytes.len()];
        remote.read_exact(&mut actual).await.expect("read wire bytes");
        assert_eq!(actual, bytes);
    }
}
