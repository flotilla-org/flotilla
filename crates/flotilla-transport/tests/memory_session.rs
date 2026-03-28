use flotilla_protocol::{HostName, Message, Request};
use flotilla_transport::message::message_session_pair;
use flotilla_transport::memory::memory_session_pair;

#[tokio::test]
async fn memory_session_pair_delivers_messages_bidirectionally() {
    let (left, right) = memory_session_pair::<u64>();

    left.writer.send(41).await.expect("left should send");
    right.writer.send(42).await.expect("right should send");

    assert_eq!(right.reader.recv().await.expect("right should receive"), Some(41));
    assert_eq!(left.reader.recv().await.expect("left should receive"), Some(42));
}

#[tokio::test]
async fn dropping_one_endpoint_closes_the_other_reader() {
    let (left, right) = memory_session_pair::<u64>();

    drop(right);

    assert_eq!(left.reader.recv().await.expect("reader should close cleanly"), None);
}

#[tokio::test]
async fn message_session_pair_transfers_protocol_messages() {
    let (left, right) = message_session_pair();

    left.write(Message::Request {
        id: 7,
        request: Request::GetTopology,
    })
    .await
    .expect("write request");

    assert!(matches!(
        right.read().await.expect("read message"),
        Some(Message::Request { id: 7, request: Request::GetTopology })
    ));

    right
        .write(Message::Hello {
            protocol_version: 4,
            host_name: HostName::new("remote"),
            session_id: Default::default(),
            environment_id: None,
        })
        .await
        .expect("write hello");

    assert!(matches!(
        left.read().await.expect("read hello"),
        Some(Message::Hello { ref host_name, .. }) if host_name == &HostName::new("remote")
    ));
}
