// HTTP audit (#1512): HttpBackend CRUD/list/watch, bootstrap namespace/CRD calls,
// and kubeconfig authentication target the owned resource-store/Kubernetes wire
// protocol. These wire tests and provisioning_http_wire cover request encoding;
// no external forge header contract or live recorded identity applies. Bootstrap
// and TLS identity setup are not claimed to have external service contract coverage.

use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use common::{convoy_meta, convoy_spec, convoy_status};
use flotilla_resources::{Convoy, ConvoyPhase, HttpBackend, ResourceBackend, ResourceError, WatchEvent, WatchStart};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    time::timeout,
};

use crate::common;

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
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
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
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let listed = resolver.list().await.expect("list should succeed");
    assert_eq!(listed.resource_version, "7");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "alpha");
    assert_eq!(listed.items[0].spec.workflow_ref, "review");
    assert_eq!(listed.items[0].status.as_ref().expect("status").phase, ConvoyPhase::Active);
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn replica_point_get_requests_read_view_and_decodes_provenance() {
    let body = serde_json::json!({
        "apiVersion": "flotilla.work/v1",
        "kind": "Convoy",
        "metadata": {
            "name": "alpha",
            "namespace": "flotilla",
            "resourceVersion": "7",
            "labels": {},
            "annotations": {
                "flotilla.work/origin-root": "remote-root",
                "flotilla.work/last-synced-at": "2026-04-13T12:00:00Z"
            },
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
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));

    let object = backend.including_replicas::<Convoy>("flotilla").get("alpha").await.expect("get replica read view");

    assert!(matches!(
        object.provenance,
        flotilla_resources::ResourceProvenance::Replica { ref origin_root, .. } if origin_root.as_str() == "remote-root"
    ));
    let request = request_rx.await.expect("captured request");
    assert!(request.starts_with("GET /apis/flotilla.work/v1/namespaces/flotilla/convoys/alpha?includeReplicas=true HTTP/1.1"));
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn provenance_point_get_requests_only_one_name() {
    let body = serde_json::json!({
        "apiVersion": "flotilla.work/v1",
        "kind": "ConvoyList",
        "metadata": { "resourceVersion": "overlay" },
        "items": [{
            "apiVersion": "flotilla.work/v1",
            "kind": "Convoy",
            "metadata": {
                "name": "alpha",
                "namespace": "flotilla",
                "resourceVersion": "7",
                "labels": {},
                "annotations": {
                    "flotilla.work/origin-root": "remote-root",
                    "flotilla.work/last-synced-at": "2026-04-13T12:00:00Z"
                },
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
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let copies = backend.including_replicas::<Convoy>("flotilla").get_all("alpha").await.expect("read named provenances");
    assert_eq!(copies.items.len(), 1);
    assert!(matches!(copies.items[0].provenance, flotilla_resources::ResourceProvenance::Replica { .. }));
    let request = request_rx.await.expect("captured request");
    assert!(request.starts_with("GET /apis/flotilla.work/v1/namespaces/flotilla/convoys/alpha?allProvenances=true HTTP/1.1"));
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
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
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let updated = resolver.update_status("alpha", "7", &convoy_status(ConvoyPhase::Active)).await.expect("status update should succeed");
    assert_eq!(updated.metadata.resource_version, "8");

    let request = request_rx.await.expect("captured request");
    assert!(request.starts_with("PUT /apis/flotilla.work/v1/namespaces/flotilla/convoys/alpha/status HTTP/1.1"));
    assert!(request.contains("\"resourceVersion\":\"7\""));
    assert!(request.contains("\"phase\":\"Active\""));
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn watch_decodes_kubernetes_watch_events() {
    let body = concat!(
        "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Convoy\",\"metadata\":{\"name\":\"alpha\",\"namespace\":\"flotilla\",\"resourceVersion\":\"7\",\"labels\":{},\"annotations\":{},\"creationTimestamp\":\"2026-04-13T12:00:00Z\"},\"spec\":{\"workflow_ref\":\"review\",\"inputs\":{},\"placement_policy\":\"laptop-docker\"},\"status\":{\"phase\":\"Pending\"}}}\n",
        "{\"type\":\"DELETED\",\"object\":{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Convoy\",\"metadata\":{\"name\":\"alpha\",\"namespace\":\"flotilla\",\"resourceVersion\":\"8\",\"labels\":{},\"annotations\":{},\"creationTimestamp\":\"2026-04-13T12:00:00Z\"},\"spec\":{\"workflow_ref\":\"review\",\"inputs\":{},\"placement_policy\":\"laptop-docker\"},\"status\":{\"phase\":\"Pending\"}}}\n"
    );
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
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
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn stale_watch_maps_http_gone_to_typed_expiry() {
    let body = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Status",
        "status": "Failure",
        "message": "too old resource version: 6",
        "reason": "Expired",
        "code": 410
    })
    .to_string();
    let (base_url, request_rx) = spawn_one_shot_server(response("410 Gone", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let error = resolver.watch(WatchStart::FromVersion("6".to_string())).await.expect_err("stale watch should expire");

    assert_eq!(error, ResourceError::WatchExpired { requested_version: "6".to_string(), compacted_through: None });
    let request = request_rx.await.expect("captured request");
    assert!(request.contains("watch=true&resourceVersion=6"));
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn stale_watch_maps_http_gone_before_reading_a_truncated_body() {
    let response = "HTTP/1.1 410 Gone\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{";
    let (base_url, _request_rx) = spawn_one_shot_server(response.to_string()).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let error = resolver.watch(WatchStart::FromVersion("6".to_string())).await.expect_err("stale watch should expire");

    assert_eq!(error, ResourceError::WatchExpired { requested_version: "6".to_string(), compacted_through: None });
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn watch_decodes_crlf_terminated_events() {
    let body =
        "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Convoy\",\"metadata\":{\"name\":\"alpha\",\"namespace\":\"flotilla\",\"resourceVersion\":\"7\",\"labels\":{},\"annotations\":{},\"creationTimestamp\":\"2026-04-13T12:00:00Z\"},\"spec\":{\"workflow_ref\":\"review\",\"inputs\":{},\"placement_policy\":\"laptop-docker\"},\"status\":{\"phase\":\"Pending\"}}}\r\n";
    let (base_url, _request_rx) = spawn_one_shot_server(response("200 OK", body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
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
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn status_errors_map_to_resource_errors() {
    let body = serde_json::json!({ "message": "resource version conflict" }).to_string();
    let (base_url, _request_rx) = spawn_one_shot_server(response("409 Conflict", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
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
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn not_found_errors_preserve_requested_name() {
    let body = serde_json::json!({ "message": "convoys.flotilla.work \"alpha\" not found" }).to_string();
    let (base_url, _request_rx) = spawn_one_shot_server(response("404 Not Found", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let err = resolver.get("alpha").await.expect_err("get should fail");
    match err {
        ResourceError::NotFound { name } => assert_eq!(name, "alpha"),
        other => panic!("expected not found, got {other}"),
    }
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn server_disconnect_ends_watch_stream_cleanly() {
    let (base_url, _request_rx) = spawn_one_shot_server(response("200 OK", "")).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");

    let mut watch = resolver.watch(WatchStart::Now).await.expect("watch should succeed");
    let next = timeout(Duration::from_millis(200), watch.next()).await.expect("watch poll should finish");
    assert!(next.is_none(), "watch stream should end when server closes with no events");
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn filtered_list_encodes_exact_match_label_selector() {
    let body = serde_json::json!({
        "metadata": { "resourceVersion": "9" },
        "items": []
    })
    .to_string();
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");
    let selector = BTreeMap::from([
        ("flotilla.work/convoy".to_string(), "convoy-a".to_string()),
        ("flotilla.work/vessel".to_string(), "implement".to_string()),
    ]);

    let listed = resolver.list_matching_labels(&selector).await.expect("filtered list should succeed");

    assert!(listed.items.is_empty());
    let request = request_rx.await.expect("captured request");
    assert!(
        request.contains("/apis/flotilla.work/v1/namespaces/flotilla/convoys?labelSelector=flotilla.work%2Fconvoy%3Dconvoy-a%2Cflotilla.work%2Fvessel%3Dimplement"),
        "unexpected request: {request}"
    );
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn renamed_label_selector_reads_legacy_http_metadata() {
    // #588 / ADR 0047: HTTP must fetch legacy labels too, then apply all
    // selector requirements locally, excluding unrelated resources.
    let mut matching = convoy_meta("legacy");
    matching.labels.insert("flotilla.work/vessel_ref".to_string(), "work".to_string());
    let body = serde_json::json!({
        "metadata": {"resourceVersion": "9"},
        "items": [
            {"apiVersion": "flotilla.work/v1", "kind": "Convoy", "metadata": {
                "name": matching.name, "namespace": "flotilla", "resourceVersion": "8", "labels": matching.labels, "creationTimestamp": "2026-04-13T12:00:00Z"
            }, "spec": convoy_spec("template")},
            {"apiVersion": "flotilla.work/v1", "kind": "Convoy", "metadata": {
                "name": "unrelated", "namespace": "flotilla", "resourceVersion": "7", "creationTimestamp": "2026-04-13T12:00:00Z"
            }, "spec": convoy_spec("template")}
        ]
    })
    .to_string();
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), base_url));
    let listed = backend
        .using::<Convoy>("flotilla")
        .list_matching_labels(&BTreeMap::from([(flotilla_resources::VESSEL_REF_LABEL.to_string(), "work".to_string())]))
        .await
        .expect("dual-read HTTP list");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "legacy");
    let request = request_rx.await.expect("request");
    assert!(!request.contains("labelSelector"), "equality selector would hide legacy labels: {request}");
}

// HTTP position reads use the collection endpoint and accept opaque versions
// and generations without decoding unrelated (even invalid) resource specs.
#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "requires socket bind")]
async fn current_position_reads_only_collection_metadata() {
    let body = r#"{"metadata":{"resourceVersion":"opaque-7","generation":"epoch-2"},"items":[{"spec":"invalid"}]}"#;
    let (url, request) = spawn_one_shot_server(response("200 OK", body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), url));
    let position = backend.using::<Convoy>("flotilla").current_position().await.expect("position");
    assert_eq!(position.resource_version, "opaque-7");
    assert_eq!(position.generation.as_deref(), Some("epoch-2"));
    let request = request.await.expect("request");
    assert!(request.starts_with("GET /apis/flotilla.work/v1/namespaces/flotilla/convoys HTTP/1.1"));
}

// API-server resource versions can be opaque or exceed a fixed integer width.
// Admission preserves the CAS token verbatim and falls back to creation order.
#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "requires socket bind")]
async fn message_admission_accepts_opaque_and_large_kubernetes_versions() {
    use flotilla_resources::{InputMeta, MessageAdmission, MessageExpectation, MessageInbox, MessageRelation, MessageSpec};
    for version in ["opaque-create-token", "184467440737095516160000"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind Kubernetes stand-in");
        let address = listener.local_addr().expect("server address");
        let intent = MessageSpec::builder()
            .sender("system:checks".into())
            .receiver("flotilla/governor".into())
            .relation(MessageRelation::System)
            .body("settled checks".into())
            .expectation(MessageExpectation::Reply)
            .build();
        let object = serde_json::json!({"apiVersion":"flotilla.work/v1","kind":"Message",
            "metadata":{"name":"partial","namespace":"flotilla","resourceVersion":version,"creationTimestamp":"2026-04-13T12:00:00Z"},"spec":intent});
        // HTTP boundary stand-in enforces the point-read, collection-read and
        // status-CAS requests used to recover a created-but-unadmitted record.
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut socket, _) = listener.accept().await.expect("accept admission request");
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let count = socket.read(&mut buffer).await.expect("read request headers");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|length| length.trim().parse::<usize>().expect("content length"))
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let count = socket.read(&mut buffer).await.expect("read request body");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8(bytes).expect("UTF-8 request");
                let body = match index {
                    0 => {
                        assert!(request.starts_with("GET /apis/flotilla.work/v1/namespaces/flotilla/messages/partial "));
                        object.clone()
                    }
                    1 => {
                        assert!(request.starts_with("GET /apis/flotilla.work/v1/namespaces/flotilla/messages "));
                        serde_json::json!({"metadata":{"resourceVersion":version},"items":[object]})
                    }
                    _ => {
                        assert!(request.starts_with("PUT /apis/flotilla.work/v1/namespaces/flotilla/messages/partial/status "));
                        let patch: serde_json::Value = serde_json::from_str(&request[header_end..]).expect("status body");
                        assert_eq!(patch["metadata"]["resourceVersion"], version, "CAS must retain the opaque token");
                        let mut result = object.clone();
                        result["status"] = patch["status"].clone();
                        result["metadata"]["resourceVersion"] = serde_json::json!("after-status-token");
                        result
                    }
                };
                socket.write_all(response("200 OK", &body.to_string()).as_bytes()).await.expect("write API response");
                socket.shutdown().await.expect("close connection");
                requests.push(request);
            }
            requests
        });
        let backend = ResourceBackend::Http(HttpBackend::new(flotilla_resources::tls::client(), format!("http://{address}")));
        let admission = MessageInbox::new(backend, "flotilla")
            .accept(&InputMeta::builder().name("partial".into()).build(), &intent, chrono::Utc::now())
            .await
            .expect("opaque-version admission");
        let MessageAdmission::Accepted(message) = admission else { panic!("admitted record") };
        assert!(message.status.expect("admission status").accepted_sequence.is_none());
        assert_eq!(timeout(Duration::from_secs(5), server).await.expect("server completed").expect("HTTP contract").len(), 3);
    }
}
