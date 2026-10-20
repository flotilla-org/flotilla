//! The hand-written HTTP front end for the main socket's sniffed connections. It renders the
//! shared [`resource_api`](super::http::resource_api) replies and goes once the main socket
//! serves the router (rigging#21; ADR 0065).

use flotilla_store::{DynamicResourceWatch, ResourceBackend};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use super::http::resource_api::{self, Reply};

const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;

#[cfg(test)]
pub(super) async fn serve_resource_http(stream: UnixStream, first_byte: u8, backend: ResourceBackend) -> Result<(), String> {
    serve_resource_http_with_daemon(stream, first_byte, backend, None).await
}

pub(super) async fn serve_resource_http_with_daemon(
    mut stream: UnixStream,
    first_byte: u8,
    backend: ResourceBackend,
    daemon: Option<std::sync::Arc<flotilla_core::in_process::InProcessDaemon>>,
) -> Result<(), String> {
    let mut request = vec![first_byte];
    while !request.ends_with(b"\r\n\r\n") {
        if request.len() >= MAX_REQUEST_HEADER_BYTES {
            return write_error(&mut stream, 431, "request headers too large").await;
        }
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await.map_err(|error| format!("read resource HTTP request: {error}"))?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }

    let request = std::str::from_utf8(&request).map_err(|error| format!("resource HTTP request is not UTF-8: {error}"))?;
    let request_line = request.lines().next().ok_or_else(|| "resource HTTP request has no request line".to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default();
    let target = request_parts.next().unwrap_or_default();
    match resource_api::respond(method, target, backend, daemon.as_ref()).await {
        Reply::Json { status, body } => write_json(&mut stream, status, &body).await,
        Reply::Watch(watch) => stream_watch(&mut stream, watch).await,
        Reply::ChangeStream { service, after, clock, once } => super::change_stream::serve(&mut stream, service, after, clock, once).await,
        Reply::Record { service, kind, name, now } => super::change_stream::record(&mut stream, service, &kind, &name, now).await,
    }
}

async fn stream_watch(stream: &mut UnixStream, mut watch: DynamicResourceWatch) -> Result<(), String> {
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n")
        .await
        .map_err(|error| format!("write resource watch headers: {error}"))?;
    for event in watch.initial {
        write_watch_event(stream, &event).await?;
    }
    while let Some(event) = watch.stream.next().await {
        let event = event.map_err(|error| error.to_string())?;
        write_watch_event(stream, &event).await?;
    }
    Ok(())
}

async fn write_watch_event(stream: &mut UnixStream, event: &serde_json::Value) -> Result<(), String> {
    let encoded = resource_api::watch_line(event)?;
    stream.write_all(&encoded).await.map_err(|error| format!("write resource watch event: {error}"))
}

async fn write_json(stream: &mut UnixStream, status: u16, value: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| format!("encode resource HTTP response: {error}"))?;
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(|error| format!("write resource HTTP headers: {error}"))?;
    stream.write_all(&body).await.map_err(|error| format!("write resource HTTP body: {error}"))
}

async fn write_error(stream: &mut UnixStream, status: u16, message: &str) -> Result<(), String> {
    write_json(stream, status, &serde_json::json!({"message": message})).await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        422 => "Unprocessable Entity",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;
    use crate::server::http::test_front_end::FrontEnd;
    use flotilla_resources::{Convoy, ConvoySpec, InputMeta};
    use flotilla_store_memory::InMemoryBackend;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The in-process UnixStream pair stands in for the daemon's HTTP connection,
    // exercising its real request parser and query dispatch without a live host.
    // Both front ends must answer with the same status line and body.
    async fn request(backend: ResourceBackend, parameters: &str) -> String {
        let hand_written = request_from(FrontEnd::HandWritten, backend.clone(), parameters).await;
        let router = request_from(FrontEnd::Router, backend, parameters).await;
        let summary = |response: &str| {
            let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
            (head.lines().next().expect("status line").to_owned(), body.to_owned())
        };
        assert_eq!(summary(&hand_written), summary(&router), "{parameters}");
        hand_written
    }

    async fn request_from(front_end: FrontEnd, backend: ResourceBackend, parameters: &str) -> String {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            let first = server.read_u8().await.unwrap();
            front_end.serve(server, first, backend, None).await.unwrap();
        });
        client
            .write_all(
                format!(
                    "GET /apis/flotilla.work/v1/namespaces/flotilla/convoys?{parameters} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        task.await.unwrap();
        response
    }
    // The server honors selectors instead of returning the entire collection;
    // malformed and undeclared field queries fail rather than silently broadening.
    #[tokio::test]
    async fn resource_http_honors_declared_selectors() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        for (name, label) in [("selected", "target"), ("excluded", "other"), ("punctuation()!", "a+b")] {
            let meta = InputMeta::builder()
                .name(name.to_owned())
                .labels(std::collections::BTreeMap::from([("team".into(), label.into())]))
                .build();
            backend
                .using::<Convoy>("flotilla")
                .create(&meta, &ConvoySpec::builder().workflow_ref("workflow".into()).build())
                .await
                .unwrap();
        }
        for parameters in ["labelSelector=team%3Dtarget", "fieldSelector=metadata.name%3Dselected"] {
            let response = request(backend.clone(), parameters).await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let body: serde_json::Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["items"].as_array().unwrap().len(), 1);
            assert_eq!(body["items"][0]["metadata"]["name"], "selected");
        }
        for (parameter, selector) in [("labelSelector", "team=a+b"), ("fieldSelector", "metadata.name=punctuation()!")] {
            let parameters = url::form_urlencoded::Serializer::new(String::new()).append_pair(parameter, selector).finish();
            let response = request(backend.clone(), &parameters).await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let body: serde_json::Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["items"].as_array().unwrap().len(), 1);
            assert_eq!(body["items"][0]["metadata"]["name"], "punctuation()!");
        }
        for parameters in [
            "labelSelector=invalid",
            "fieldSelector=spec.unknown%3Dx",
            "labelSelector=a!%3Db",
            "labelSelector=a%20in%20(x)",
            "labelSelector=a%3D%3Db",
        ] {
            assert!(request(backend.clone(), parameters).await.starts_with("HTTP/1.1 400"));
        }
        let query = flotilla_store::StoreQuery::labels(&std::collections::BTreeMap::from([("team".into(), "target".into())]), false, true);
        let parameters: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("storeQuery", &serde_json::to_string(&query).unwrap())
            .append_pair("labelSelector", "team=target")
            .finish();
        assert!(request(backend.clone(), &parameters).await.starts_with("HTTP/1.1 400"), "duplicate merged selector key is refused");
        let parameters: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("storeQuery", &serde_json::to_string(&query).unwrap())
            .append_pair("fieldSelector", "metadata.name=selected")
            .finish();
        let response = request(backend, &parameters).await;
        assert!(response.starts_with("HTTP/1.1 200"), "distinct query and selector predicates are combined");
        let body: serde_json::Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
    }
}
