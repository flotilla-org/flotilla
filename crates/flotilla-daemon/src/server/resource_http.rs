use std::collections::BTreeMap;

use flotilla_resources::{
    list_resource_kind, list_resource_kind_including_replicas, watch_resource_kind, watch_resource_kind_from,
    watch_resource_kind_including_replicas, DynamicResourceWatch, ResourceBackend, ResourceError, WatchStart,
};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;

pub(super) async fn serve_resource_http(mut stream: UnixStream, first_byte: u8, backend: ResourceBackend) -> Result<(), String> {
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
    if method != "GET" {
        return write_error(&mut stream, 405, "resource API is read-only").await;
    }

    let (path, raw_query) = target.split_once('?').map_or((target, ""), |parts| parts);
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let ["apis", "flotilla.work", "v1", "namespaces", namespace, kind] = segments.as_slice() else {
        return write_error(&mut stream, 404, "unknown resource API path").await;
    };
    let query = parse_query(raw_query);
    let include_replicas =
        ["includeReplicas", "include-replicas", "include_replicas"].iter().filter_map(|key| query.get(*key)).any(|value| value == "true");
    let watch = query.get("watch").is_some_and(|value| value == "true");

    if !watch {
        let listed = if include_replicas {
            list_resource_kind_including_replicas(&backend, namespace, kind).await
        } else {
            list_resource_kind(&backend, namespace, kind).await
        };
        return match listed {
            Ok(listed) => write_json(&mut stream, 200, &listed.value).await,
            Err(error) => write_resource_error(&mut stream, error).await,
        };
    }

    let watched = if include_replicas {
        if query.contains_key("resourceVersion") {
            Err(ResourceError::invalid("include-replicas watches do not support resourceVersion resume"))
        } else {
            watch_resource_kind_including_replicas(&backend, namespace, kind).await
        }
    } else if let Some(resource_version) = query.get("resourceVersion") {
        let start = match query.get("generation") {
            Some(generation) => {
                WatchStart::FromVersionInGeneration { generation: generation.clone(), resource_version: resource_version.clone() }
            }
            None => WatchStart::FromVersion(resource_version.clone()),
        };
        watch_resource_kind_from(&backend, namespace, kind, start).await
    } else {
        watch_resource_kind(&backend, namespace, kind).await
    };

    match watched {
        Ok(watch) => stream_watch(&mut stream, watch).await,
        Err(error) => write_resource_error(&mut stream, error).await,
    }
}

fn parse_query(raw_query: &str) -> BTreeMap<String, String> {
    raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
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
    let mut encoded = serde_json::to_vec(event).map_err(|error| format!("encode resource watch event: {error}"))?;
    encoded.push(b'\n');
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

async fn write_resource_error(stream: &mut UnixStream, error: ResourceError) -> Result<(), String> {
    let status = match error {
        ResourceError::WatchExpired { .. } => 410,
        ResourceError::Invalid { .. } => 400,
        ResourceError::NotFound { .. } => 404,
        ResourceError::Conflict { .. } => 409,
        ResourceError::Unauthorized { .. } => 403,
        ResourceError::Other { .. } => 500,
    };
    write_error(stream, status, &error.to_string()).await
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
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}
