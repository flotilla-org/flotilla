mod common;

use std::{net::SocketAddr, time::Duration};

use common::{convoy_meta, convoy_spec, convoy_status};
use flotilla_resources::{Convoy, ConvoyPhase, HttpBackend, ResourceBackend, ResourceError, WatchEvent, WatchStart};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    time::timeout,
};

async fn spawn_one_shot_server(response: String) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let (request_tx, request_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept connection");
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        let mut content_length = 0_usize;
        loop {
            let read = socket.read(&mut buf).await.expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n").map(|idx| idx + 4) {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    for line in headers.lines() {
                        let lower = line.to_ascii_lowercase();
                        if let Some(value) = lower.strip_prefix("content-length:") {
                            content_length = value.trim().parse::<usize>().expect("content length");
                        }
                    }
                    while request.len() < header_end + content_length {
                        let read = socket.read(&mut buf).await.expect("read request body");
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buf[..read]);
                    }
                }
                break;
            }
        }
        socket.write_all(response.as_bytes()).await.expect("write response");
        socket.shutdown().await.expect("shutdown socket");
        let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
    });
    (format!("http://{}", addr), request_rx)
}

fn response(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
}

#[tokio::test]
async fn list_decodes_collection_resource_version() {
    let body = serde_json::json!({
        "metadata": { "resourceVersion": "7" },
        "items": [{
            "apiVersion": "flotilla.work/v1",
            "kind": "Convoy",
            "metadata": {
                "name": "alpha",
                "namespace": "flotilla",
                "resourceVersion": "7",
                "labels": { "app": "flotilla" },
                "annotations": { "note": "test" },
                "creationTimestamp": "2026-04-13T12:00:00Z"
            },
            "spec": {
                "workflow_ref": "review",
                "inputs": {},
                "placement_policy": "laptop-docker"
            },
            "status": { "phase": "Active" }
        }]
    })
    .to_string();
    let (base_url, _request_rx) = spawn_one_shot_server(response("200 OK", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let listed = resolver.list().await.expect("list should succeed");
    assert_eq!(listed.resource_version, "7");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "alpha");
    assert_eq!(listed.items[0].spec.workflow_ref, "review");
    assert_eq!(listed.items[0].status.as_ref().expect("status").phase, ConvoyPhase::Active);
}

#[tokio::test]
async fn update_status_uses_status_subresource_path_and_body() {
    let body = serde_json::json!({
        "apiVersion": "flotilla.work/v1",
        "kind": "Convoy",
        "metadata": {
            "name": "alpha",
            "namespace": "flotilla",
            "resourceVersion": "8",
            "labels": { "app": "flotilla" },
            "annotations": { "note": "test" },
            "creationTimestamp": "2026-04-13T12:00:00Z"
        },
        "spec": {
            "workflow_ref": "review",
            "inputs": {},
            "placement_policy": "laptop-docker"
        },
        "status": { "phase": "Active" }
    })
    .to_string();
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let updated = resolver.update_status("alpha", "7", &convoy_status(ConvoyPhase::Active)).await.expect("status update should succeed");
    assert_eq!(updated.metadata.resource_version, "8");

    let request = request_rx.await.expect("captured request");
    assert!(request.starts_with("PUT /apis/flotilla.work/v1/namespaces/flotilla/convoys/alpha/status HTTP/1.1"));
    assert!(request.contains("\"resourceVersion\":\"7\""));
    assert!(request.contains("\"phase\":\"Active\""));
}

#[tokio::test]
async fn watch_decodes_kubernetes_watch_events() {
    let body = concat!(
        "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Convoy\",\"metadata\":{\"name\":\"alpha\",\"namespace\":\"flotilla\",\"resourceVersion\":\"7\",\"labels\":{},\"annotations\":{},\"creationTimestamp\":\"2026-04-13T12:00:00Z\"},\"spec\":{\"workflow_ref\":\"review\",\"inputs\":{},\"placement_policy\":\"laptop-docker\"},\"status\":{\"phase\":\"Pending\"}}}\n",
        "{\"type\":\"DELETED\",\"object\":{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Convoy\",\"metadata\":{\"name\":\"alpha\",\"namespace\":\"flotilla\",\"resourceVersion\":\"8\",\"labels\":{},\"annotations\":{},\"creationTimestamp\":\"2026-04-13T12:00:00Z\"},\"spec\":{\"workflow_ref\":\"review\",\"inputs\":{},\"placement_policy\":\"laptop-docker\"},\"status\":{\"phase\":\"Pending\"}}}\n"
    );
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let mut watch = resolver.watch(WatchStart::FromVersion("6".to_string())).await.expect("watch should succeed");
    let first = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should yield first event")
        .expect("stream item")
        .expect("event decode");
    match first {
        WatchEvent::Added(object) => assert_eq!(object.metadata.resource_version, "7"),
        _ => panic!("expected added event"),
    }
    let second = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should yield second event")
        .expect("stream item")
        .expect("event decode");
    match second {
        WatchEvent::Deleted(object) => assert_eq!(object.metadata.resource_version, "8"),
        _ => panic!("expected deleted event"),
    }

    let request = request_rx.await.expect("captured request");
    assert!(request.starts_with("GET /apis/flotilla.work/v1/namespaces/flotilla/convoys?watch=true&resourceVersion=6 HTTP/1.1"));
}

#[tokio::test]
async fn watch_decodes_crlf_terminated_events() {
    let body =
        "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Convoy\",\"metadata\":{\"name\":\"alpha\",\"namespace\":\"flotilla\",\"resourceVersion\":\"7\",\"labels\":{},\"annotations\":{},\"creationTimestamp\":\"2026-04-13T12:00:00Z\"},\"spec\":{\"workflow_ref\":\"review\",\"inputs\":{},\"placement_policy\":\"laptop-docker\"},\"status\":{\"phase\":\"Pending\"}}}\r\n";
    let (base_url, _request_rx) = spawn_one_shot_server(response("200 OK", body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let mut watch = resolver.watch(WatchStart::Now).await.expect("watch should succeed");
    let event =
        timeout(Duration::from_secs(1), watch.next()).await.expect("watch should yield event").expect("stream item").expect("event decode");
    match event {
        WatchEvent::Added(object) => assert_eq!(object.metadata.resource_version, "7"),
        _ => panic!("expected added event"),
    }
}

#[tokio::test]
async fn status_errors_map_to_resource_errors() {
    let body = serde_json::json!({ "message": "resource version conflict" }).to_string();
    let (base_url, _request_rx) = spawn_one_shot_server(response("409 Conflict", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let err = resolver.update(&convoy_meta("alpha"), "7", &convoy_spec("review")).await.expect_err("update should conflict");
    match err {
        ResourceError::Conflict { name, message } => {
            assert_eq!(name, "alpha");
            assert!(message.contains("resource version conflict"));
        }
        other => panic!("expected conflict, got {other}"),
    }
}

#[tokio::test]
async fn not_found_errors_preserve_requested_name() {
    let body = serde_json::json!({ "message": "convoys.flotilla.work \"alpha\" not found" }).to_string();
    let (base_url, _request_rx) = spawn_one_shot_server(response("404 Not Found", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let err = resolver.get("alpha").await.expect_err("get should fail");
    match err {
        ResourceError::NotFound { name } => assert_eq!(name, "alpha"),
        other => panic!("expected not found, got {other}"),
    }
}

#[tokio::test]
async fn server_disconnect_ends_watch_stream_cleanly() {
    let (base_url, _request_rx) = spawn_one_shot_server(response("200 OK", "")).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let mut watch = resolver.watch(WatchStart::Now).await.expect("watch should succeed");
    let next = timeout(Duration::from_millis(200), watch.next()).await.expect("watch poll should finish");
    assert!(next.is_none(), "watch stream should end when server closes with no events");
}
