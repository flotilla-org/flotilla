mod common;

use std::{collections::BTreeMap, net::SocketAddr};

use common::convoy_spec;
use flotilla_resources::{canonicalize_repo_url, clone_key, repo_key, Convoy, HttpBackend, InputMeta, OwnerReference, ResourceBackend};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
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

fn owner_reference(name: &str) -> OwnerReference {
    OwnerReference {
        api_version: "flotilla.work/v1".to_string(),
        kind: "TaskWorkspace".to_string(),
        name: name.to_string(),
        controller: true,
    }
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn http_list_decodes_owner_references() {
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
                "ownerReferences": [{
                    "apiVersion": "flotilla.work/v1",
                    "kind": "TaskWorkspace",
                    "name": "alpha-implement",
                    "controller": true
                }],
                "finalizers": ["flotilla.work/example"],
                "deletionTimestamp": "2026-04-14T12:00:00Z",
                "creationTimestamp": "2026-04-14T11:00:00Z"
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

    assert_eq!(listed.items[0].metadata.owner_references, vec![owner_reference("alpha-implement")]);
    assert_eq!(
        listed.items[0].metadata.deletion_timestamp,
        Some(chrono::DateTime::parse_from_rfc3339("2026-04-14T12:00:00Z").expect("parse deletion timestamp").with_timezone(&chrono::Utc))
    );
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn http_create_serializes_owner_references() {
    let body = serde_json::json!({
        "apiVersion": "flotilla.work/v1",
        "kind": "Convoy",
        "metadata": {
            "name": "alpha",
            "namespace": "flotilla",
            "resourceVersion": "8",
            "labels": { "app": "flotilla" },
            "annotations": { "note": "test" },
            "ownerReferences": [{
                "apiVersion": "flotilla.work/v1",
                "kind": "TaskWorkspace",
                "name": "alpha-implement",
                "controller": true
            }],
            "creationTimestamp": "2026-04-14T11:00:00Z"
        },
        "spec": {
            "workflow_ref": "review",
            "inputs": {},
            "placement_policy": "laptop-docker"
        }
    })
    .to_string();
    let (base_url, request_rx) = spawn_one_shot_server(response("200 OK", &body)).await;
    let backend = ResourceBackend::Http(HttpBackend::new(reqwest::Client::new(), base_url));
    let resolver = backend.using::<Convoy>("flotilla");
    let meta = InputMeta {
        name: "alpha".to_string(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: vec![owner_reference("alpha-implement")],
        finalizers: Vec::new(),
        deletion_timestamp: None,
    };

    resolver.create(&meta, &convoy_spec("review")).await.expect("create should succeed");
    let request = request_rx.await.expect("captured request");

    assert!(request.starts_with("POST /apis/flotilla.work/v1/namespaces/flotilla/convoys HTTP/1.1"));
    assert!(request.contains("\"ownerReferences\":[{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"TaskWorkspace\",\"name\":\"alpha-implement\",\"controller\":true}]"));
}

#[test]
fn canonicalize_repo_url_treats_ssh_and_https_as_same_identity() {
    assert_eq!(
        canonicalize_repo_url("git@github.com:flotilla-org/flotilla.git").expect("canonical SSH"),
        "https://github.com/flotilla-org/flotilla"
    );
    assert_eq!(
        canonicalize_repo_url("https://github.com/flotilla-org/flotilla/").expect("canonical HTTPS"),
        "https://github.com/flotilla-org/flotilla"
    );
}

#[test]
fn repo_and_clone_keys_are_stable_and_dns_safe() {
    let repo = repo_key("https://github.com/flotilla-org/flotilla");
    let clone = clone_key("https://github.com/flotilla-org/flotilla", "host-direct-01HXYZ");

    assert_eq!(repo.len(), 52);
    assert_eq!(clone.len(), 52);
    assert!(repo.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()));
    assert!(clone.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()));
    assert_eq!(repo, repo_key("https://github.com/flotilla-org/flotilla"));
    assert_eq!(clone, clone_key("https://github.com/flotilla-org/flotilla", "host-direct-01HXYZ"));
}
