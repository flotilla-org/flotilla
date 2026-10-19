//! HTTP adapter for the listener-independent ADR 0064 stream. Paths and framed CBOR bodies
//! are independent of Unix sockets, SSH forwarding, and the future axum listener.

use std::{path::PathBuf, sync::Arc, time::Duration};

use flotilla_schema::{Code, KindCode, Timestamp};
use flotilla_schema_sqlite::{
    stream::{self, Follower, Frame, Service, FRAMING, HEADER_LEN},
    StoreError,
};
use futures::StreamExt;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

const CADENCE: Duration = Duration::from_secs(5);
const IO_DEADLINE: Duration = Duration::from_secs(15);
const QUEUED_FRAMES: usize = 2;

fn transport(error: impl std::fmt::Display) -> StoreError {
    StoreError::Stream(error.to_string())
}

/// Listener seam: write only requires AsyncWrite; the service supplies no listener or clock.
pub(super) async fn serve<W: AsyncWrite + Unpin>(
    writer: &mut W,
    service: Arc<Service>,
    after: Option<u64>,
    clock: Arc<dyn flotilla_resources::Clock>,
    once: bool,
) -> Result<(), String> {
    writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.flotilla.log+cbor\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.map_err(|e| e.to_string())?;
    let mut cursor = after;
    loop {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(QUEUED_FRAMES);
        let source = Arc::clone(&service);
        let at = Timestamp::from_datetime(&clock.now());
        let task =
            tokio::task::spawn_blocking(move || source.serve(cursor, at, |frame| tx.blocking_send(frame.to_vec()).map_err(transport)));
        // A writer error drops rx. The blocking worker may finish its current read/encode,
        // then exits when blocking_send observes the closed, two-frame bounded channel.
        while let Some(frame) = rx.recv().await {
            let chunk = async {
                writer.write_all(format!("{:x}\r\n", frame.len()).as_bytes()).await?;
                writer.write_all(&frame).await?;
                writer.write_all(b"\r\n").await?;
                writer.flush().await
            };
            tokio::time::timeout(IO_DEADLINE, chunk)
                .await
                .map_err(|_| "stream consumer stalled".to_owned())?
                .map_err(|error| error.to_string())?;
        }
        cursor = Some(task.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?);
        if once {
            writer.write_all(b"0\r\n\r\n").await.map_err(|error| error.to_string())?;
            return Ok(());
        }
        tokio::time::sleep(CADENCE).await;
    }
}

pub(super) async fn record<W: AsyncWrite + Unpin>(
    writer: &mut W,
    service: Arc<Service>,
    kind: &str,
    name: &str,
    now: Timestamp,
) -> Result<(), String> {
    let kind = KindCode(Code::parse(kind).map_err(|e| e.to_string())?);
    let name = name.to_owned();
    let bytes = tokio::task::spawn_blocking(move || service.query(kind, &name, now))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    tokio::time::timeout(IO_DEADLINE, async {
        writer
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.flotilla.log+cbor\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )
                .as_bytes(),
            )
            .await
            .map_err(|e| e.to_string())?;
        writer.write_all(&bytes).await.map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "record consumer stalled".to_owned())?
}

/// Surface point read: one local lookup, then one query through the home's forwarded HTTP
/// socket when absent. Gate callers use the service's local watermark and never call this API.
pub async fn read_record(
    service: Arc<Service>,
    identity: flotilla_schema::Identity,
    socket: &std::path::Path,
    now: impl Fn() -> Timestamp,
) -> Result<stream::ReadResult, String> {
    let lookup = identity.clone();
    let local = tokio::task::spawn_blocking(move || {
        service.with_store(|store| {
            let mut read = store.read()?;
            read.resolve(lookup.kind, &lookup.name, &lookup.home)?.map(|id| read.load_snapshot(id)).transpose().map(Option::flatten)
        })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    if let Some(value) = local {
        return Ok(stream::ReadResult { value: Some(value), remote: false, age_micros: 0 });
    }
    let client = rigging_tls::client_builder()
        .unix_socket(socket)
        .connect_timeout(IO_DEADLINE)
        .timeout(IO_DEADLINE)
        .build()
        .map_err(|error| error.to_string())?;
    let mut url = reqwest::Url::parse(&format!("http://flotilla.local{}", stream::RECORD_PATH)).map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("kind", identity.kind.as_str()).append_pair("name", &identity.name);
    let response =
        client.get(url).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?;
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > HEADER_LEN + FRAMING.limits.max_message {
            return Err("record response exceeds frame limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    match Frame::decode(&bytes).map_err(|error| error.to_string())? {
        Frame::Read { home, value, served_at } => {
            if home != identity.home {
                return Err("record response is from another home".into());
            }
            if value.as_ref().is_some_and(|value| value.home != identity.home || value.kind != identity.kind || value.name != identity.name)
            {
                return Err("record response is from another identity/home".into());
            }
            Ok(stream::ReadResult { value, remote: true, age_micros: now().since(served_at).max(0) })
        }
        _ => Err("home returned a non-read frame".into()),
    }
}

/// Every direct home is followed over the existing forwarded resource socket. Dropping the
/// returned task cancels this generation; failures retry from the last committed prefix.
pub(super) fn follow(service: Arc<Service>, home: String, socket: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // ZERO alone cannot prove a completed empty snapshot after a restart.
        let mut baseline = Baseline::Unknown;
        let mut failures: u64 = 0;
        loop {
            let result = follow_connection(Arc::clone(&service), &home, &socket, baseline).await;
            if result.is_ok() {
                baseline = Baseline::Known;
                failures = 0;
            }
            if let Err(error) = result {
                failures = failures.saturating_add(1);
                if failures == 3 || failures.is_multiple_of(12) {
                    tracing::warn!(%home, %error, failures, "decided stream repeatedly disconnected; resuming");
                } else {
                    tracing::debug!(%home, %error, "decided stream disconnected; resuming");
                }
            }
            tokio::time::sleep(CADENCE).await;
        }
    })
}

#[derive(Clone, Copy)]
enum Baseline {
    Unknown,
    Known,
}

async fn follow_connection(service: Arc<Service>, home: &str, socket: &std::path::Path, baseline: Baseline) -> Result<(), String> {
    let after = service.watermark(home).map_err(|e| e.to_string())?;
    let client = rigging_tls::client_builder()
        .unix_socket(socket)
        .connect_timeout(IO_DEADLINE)
        .read_timeout(IO_DEADLINE)
        .build()
        .map_err(|e| e.to_string())?;
    let target = if after == stream::Watermark::ZERO && matches!(baseline, Baseline::Unknown) {
        format!("http://flotilla.local{}?once=true", stream::PATH)
    } else {
        format!("http://flotilla.local{}?once=true&after={}", stream::PATH, after.home_seq)
    };
    let response = client.get(target).send().await.map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?;
    // Stage a complete round on disk before acquiring the writer. This keeps frame memory
    // bounded and preserves atomic snapshot install without holding a lock over network waits.
    let mut spool = tokio::fs::File::from_std(tempfile::tempfile().map_err(|e| e.to_string())?);
    let mut body = response.bytes_stream();
    let mut pending = Vec::new();
    let mut expected = HEADER_LEN;
    let mut complete = false;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        let mut remaining = chunk.as_ref();
        while !remaining.is_empty() {
            if complete {
                return Err("frames after round keepalive".to_owned());
            }
            let take = (expected - pending.len()).min(remaining.len());
            pending.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if pending.len() == HEADER_LEN && expected == HEADER_LEN {
                let header: &[u8; HEADER_LEN] = pending.as_slice().try_into().map_err(|_| "invalid header length")?;
                expected = HEADER_LEN + FRAMING.body_len(header).map_err(|e| e.to_string())?;
                if expected == HEADER_LEN {
                    return Err("empty CBOR frame".to_owned());
                }
            }
            if pending.len() == expected {
                complete = matches!(Frame::decode(&pending).map_err(|e| e.to_string())?, Frame::Keepalive { .. });
                spool.write_all(&pending).await.map_err(|e| e.to_string())?;
                pending.clear();
                expected = HEADER_LEN;
            }
        }
    }
    if !pending.is_empty() || !complete {
        return Err("truncated home stream".to_owned());
    }
    spool.flush().await.map_err(|e| e.to_string())?;
    let mut spool = spool.into_std().await;
    let home = home.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek};
        spool.rewind().map_err(transport)?;
        service.with_store(|store| {
            Follower::new(store).receive(&home, |sink| loop {
                let mut header = [0; HEADER_LEN];
                spool.read_exact(&mut header).map_err(transport)?;
                let length = FRAMING.body_len(&header).map_err(transport)?;
                let mut frame = header.to_vec();
                frame.resize(HEADER_LEN + length, 0);
                spool.read_exact(&mut frame[HEADER_LEN..]).map_err(transport)?;
                let end = matches!(Frame::decode(&frame)?, Frame::Keepalive { .. });
                sink(&frame)?;
                if end {
                    return Ok(());
                }
            })
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_schema::KindRegistry;
    use flotilla_schema_sqlite::stream::Watermark;
    use flotilla_test_support::TestSocketDir;
    use tokio::io::AsyncReadExt;

    // The real Unix HTTP client talks to an in-process stand-in that enforces its method/path
    // and delivers deliberately fragmented ADR 0063 frames. No TCP listener or external daemon.
    #[tokio::test]
    async fn http_client_obeys_route_and_applies_fragmented_frames() {
        for baseline in [Baseline::Unknown, Baseline::Known] {
            let dir = TestSocketDir::new();
            let socket = dir.socket_path("stream.sock");
            let listener = match tokio::net::UnixListener::bind(&socket) {
                Ok(listener) => listener,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
                Err(error) => panic!("bind: {error}"),
            };
            let store_dir = tempfile::tempdir().expect("dir");
            let service = Arc::new(Service::open(&store_dir.path().join("store.sqlite"), "replica", KindRegistry::new()).expect("store"));
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.expect("request");
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).expect("UTF-8");
                assert_eq!(
                    request.lines().next(),
                    Some(match baseline {
                        Baseline::Unknown => "GET /apis/flotilla.work/v1/change-stream?once=true HTTP/1.1",
                        Baseline::Known => "GET /apis/flotilla.work/v1/change-stream?once=true&after=0 HTTP/1.1",
                    })
                );
                let frames = [
                    Frame::Begin { home: "home".into(), home_seq: 0 },
                    Frame::End { home: "home".into(), home_seq: 0 },
                    Frame::Keepalive { home: "home".into(), watermark: Watermark { home_seq: 0, applied_through: Timestamp(7) } },
                ];
                let bytes: Vec<u8> = frames.iter().flat_map(|frame| frame.encode().expect("encode")).collect();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/vnd.flotilla.log+cbor\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).as_bytes()).await.expect("headers");
                for chunk in bytes.chunks(3) {
                    socket.write_all(chunk).await.expect("chunk");
                    tokio::task::yield_now().await;
                }
            });
            follow_connection(Arc::clone(&service), "home", &socket, baseline).await.expect("follow HTTP");
            peer.await.expect("stand-in");
            assert_eq!(service.with_store(|store| store.read()?.watermark("home")).expect("mark").applied_through, Timestamp(7));
        }
    }

    // A peer pauses after Begin. Its incomplete download must not lock out gates, another
    // home's apply transaction, or this host's own serving. The old implementation held
    // with_store across this pause; the timeout would fail until the peer was released.
    #[tokio::test]
    async fn stalled_download_leaves_store_and_watermark_available() {
        let dir = TestSocketDir::new();
        let socket = dir.socket_path("stall.sock");
        let listener = match tokio::net::UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind: {error}"),
        };
        let store_dir = tempfile::tempdir().expect("dir");
        let service = Arc::new(Service::open(&store_dir.path().join("store.sqlite"), "replica", KindRegistry::new()).expect("store"));
        let (sent, first) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.expect("request");
                request.push(byte[0]);
            }
            let begin = Frame::Begin { home: "home".into(), home_seq: 0 }.encode().expect("begin");
            let rest: Vec<u8> = [
                Frame::End { home: "home".into(), home_seq: 0 },
                Frame::Keepalive { home: "home".into(), watermark: Watermark { home_seq: 0, applied_through: Timestamp(7) } },
            ]
            .iter()
            .flat_map(|frame| frame.encode().expect("frame"))
            .collect();
            socket
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", begin.len() + rest.len()).as_bytes(),
                )
                .await
                .expect("headers");
            socket.write_all(&begin).await.expect("begin");
            sent.send(()).expect("sent");
            wait.await.expect("release");
            socket.write_all(&rest).await.expect("rest");
        });
        let target = Arc::clone(&service);
        let follow = tokio::spawn(async move { follow_connection(target, "home", &socket, Baseline::Unknown).await });
        first.await.expect("first frame sent");
        // Let the client consume Begin and block on the deliberately withheld next frame.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let other = Arc::clone(&service);
        let concurrent = tokio::task::spawn_blocking(move || {
            assert_eq!(other.watermark("home")?, Watermark::ZERO);
            other.with_store(|store| {
                Follower::new(store).receive("other", |sink| {
                    sink(
                        &Frame::Keepalive { home: "other".into(), watermark: Watermark { home_seq: 0, applied_through: Timestamp(3) } }
                            .encode()?,
                    )
                })
            })?;
            other.serve(Some(0), Timestamp(4), |_| Ok(()))?;
            Ok::<_, StoreError>(())
        });
        let progress = tokio::time::timeout(Duration::from_secs(2), concurrent).await;
        release.send(()).expect("release peer");
        peer.await.expect("peer");
        follow.await.expect("follow task").expect("complete round");
        progress.expect("network wait must not hold writer").expect("concurrent task").expect("local operations");
        assert_eq!(service.watermark("home").expect("mark").applied_through, Timestamp(7));
        assert_eq!(service.watermark("other").expect("other mark").applied_through, Timestamp(3));
    }

    // The listener adapter emits standard HTTP chunking around the unchanged stream body.
    // Duplex I/O proves it has no dependency on a Unix or TCP listener.
    #[tokio::test]
    async fn listener_seam_sends_snapshot_and_keepalive() {
        let store_dir = tempfile::tempdir().expect("dir");
        let service = Arc::new(Service::open(&store_dir.path().join("store.sqlite"), "home", KindRegistry::new()).expect("store"));
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move { serve(&mut writer, service, None, Arc::new(flotilla_resources::SystemClock), false).await });
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            reader.read_exact(&mut byte).await.expect("header");
            header.push(byte[0]);
        }
        assert!(String::from_utf8(header).expect("header").contains("Transfer-Encoding: chunked"));
        let mut kinds = Vec::new();
        for _ in 0..3 {
            let mut length = Vec::new();
            while !length.ends_with(b"\r\n") {
                let mut byte = [0];
                reader.read_exact(&mut byte).await.expect("length");
                length.push(byte[0]);
            }
            let len = usize::from_str_radix(std::str::from_utf8(&length[..length.len() - 2]).expect("length"), 16).expect("hex");
            let mut body = vec![0; len];
            reader.read_exact(&mut body).await.expect("body");
            let mut end = [0; 2];
            reader.read_exact(&mut end).await.expect("chunk end");
            assert_eq!(&end, b"\r\n");
            kinds.push(Frame::decode(&body).expect("frame"));
        }
        assert!(matches!(kinds.as_slice(), [Frame::Begin { .. }, Frame::End { .. }, Frame::Keepalive { .. }]));
        task.abort();
    }
    // A routed miss is exactly one encoded point query to the home. Even an absent result
    // retains remote provenance and the age of the home time that served it.
    #[tokio::test]
    async fn routed_record_miss_uses_one_home_query_with_age() {
        for exists in [false, true] {
            let dir = TestSocketDir::new();
            let socket = dir.socket_path("read.sock");
            let listener = match tokio::net::UnixListener::bind(&socket) {
                Ok(listener) => listener,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
                Err(error) => panic!("bind: {error}"),
            };
            let store_dir = tempfile::tempdir().expect("dir");

            let mut kinds = KindRegistry::new();
            kinds
                .register_table_set(flotilla_schema::TableSet {
                    kind: "TestRecord".into(),
                    code: KindCode::new("ms"),
                    indexes: vec![],
                    edges: vec![],
                    edge_names: vec![],
                    tables: vec![flotilla_schema::Table {
                        name: "test_record".into(),
                        role: flotilla_schema::TableRole::Root,
                        parent: None,
                        field: String::new(),
                        key: vec![],
                        own_key: 0,
                        columns: vec![],
                        guards: vec![],
                        enums: vec![],
                        codes: vec![],
                        fields: vec![],
                        field_aliases: vec![],
                        aliases: vec![],
                    }],
                })
                .expect("test descriptor");
            let service = Arc::new(Service::open(&store_dir.path().join("registered.sqlite"), "replica", kinds).expect("registered store"));
            let value = exists.then(|| flotilla_schema_sqlite::Snapshot {
                kind: KindCode::new("ms"),
                name: "a&b".into(),
                home: "home".into(),
                version: 1,
                rows: flotilla_schema::Rows {
                    tables: vec![vec![flotilla_schema::Row { key: vec![], values: vec![] }]],
                    ..Default::default()
                },
            });
            let remote = value.clone();
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.expect("request");
                    request.push(byte[0]);
                }
                assert_eq!(
                    String::from_utf8(request).expect("UTF-8").lines().next(),
                    Some("GET /apis/flotilla.work/v1/record?kind=ms&name=a%26b HTTP/1.1")
                );
                let bytes = Frame::Read { home: "home".into(), value: remote, served_at: Timestamp(9) }.encode().expect("frame");
                socket
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes())
                    .await
                    .expect("headers");
                socket.write_all(&bytes).await.expect("body");
            });
            let identity = flotilla_schema::Identity { kind: KindCode::new("ms"), name: "a&b".into(), home: "home".into() };
            let result = read_record(Arc::clone(&service), identity.clone(), &socket, || Timestamp(12)).await.expect("routed read");
            peer.await.expect("strict stand-in");
            assert!(result.remote);
            assert_eq!(result.age_micros, 3);
            assert_eq!(result.value, value);
            if let Some(value) = value {
                service
                    .with_store(|store| {
                        let mut write = store.write()?;
                        write.restore(&value)?;
                        write.commit()
                    })
                    .expect("seed local replica");
                // The listener is gone: a local hit must not attempt a home query.
                let local = read_record(Arc::clone(&service), identity, &socket, || Timestamp(15)).await.expect("local hit");
                assert!(!local.remote);
                assert_eq!(local.age_micros, 0);
                assert_eq!(local.value, Some(value));
            }
        }
    }
}
