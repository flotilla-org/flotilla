use std::collections::BTreeMap;

use flotilla_resources::{
    get_resource_kind, get_resource_kind_all_provenances, get_resource_kind_including_replicas, list_resource_kind,
    list_resource_kind_including_replicas, list_resource_kind_replica_sources, registered_resource_namespaces, watch_resource_kind,
    watch_resource_kind_from, watch_resource_kind_including_replicas, watch_resource_kind_replica_sources, DynamicResourceWatch,
    ResourceBackend, ResourceError, WatchStart, REGISTERED_RESOURCE_KINDS,
};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

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
    if method != "GET" {
        return write_error(&mut stream, 405, "resource API is read-only").await;
    }

    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let observed_path = path == "/observed" || path.starts_with("/observed/");
    let additional_stores = if daemon.is_some() && !observed_path { vec!["/observed"] } else { Vec::new() };
    let (path, backend) = if observed_path {
        let path = path.strip_prefix("/observed").expect("observed store prefix");
        let Some(daemon) = &daemon else {
            return write_error(&mut stream, 404, "observed resource store unavailable").await;
        };
        (path, daemon.observed_resource_backend())
    } else {
        (path, backend)
    };
    if path == "/apis/flotilla.work/v1" {
        let namespaces = match registered_resource_namespaces(&backend).await {
            Ok(namespaces) => namespaces,
            Err(error) => return write_resource_error(&mut stream, error).await,
        };
        return write_json(
            &mut stream,
            200,
            &serde_json::json!({"kinds": REGISTERED_RESOURCE_KINDS.iter().map(|kind| kind.plural).collect::<Vec<_>>(), "namespaces": namespaces, "stores": additional_stores }),
        )
        .await;
    }
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let (namespace, kind, name) = match segments.as_slice() {
        ["apis", "flotilla.work", "v1", "namespaces", namespace, kind] => (*namespace, *kind, None),
        ["apis", "flotilla.work", "v1", "namespaces", namespace, kind, name] => (*namespace, *kind, Some(*name)),
        _ => return write_error(&mut stream, 404, "unknown resource API path").await,
    };
    // Administrative read-only inventory, alongside kind discovery on the
    // same trusted local resource socket. Return raw bytes so only the candidate
    // applies its parser. The optional daemon is solely a resource-handler test
    // seam; production always supplies it.
    if kind == "charterinputs" && name.is_none() {
        let Some(daemon) = daemon else {
            return write_error(&mut stream, 404, "charter inventory unavailable").await;
        };
        return match daemon
            .charter_input_inventory(namespace)
            .await
            .and_then(|inventory| serde_json::to_value(inventory).map_err(|error| error.to_string()))
        {
            Ok(inventory) => write_json(&mut stream, 200, &inventory).await,
            Err(error) => write_error(&mut stream, 422, &error).await,
        };
    }
    if kind == "operationalentries" && name.is_none() {
        let Some(daemon) = daemon else {
            return write_error(&mut stream, 404, "ops inventory unavailable").await;
        };
        return match daemon
            .project_operational_entry_inventory(namespace)
            .await
            .and_then(|inventory| serde_json::to_value(inventory).map_err(|error| format!("serialize ops inventory: {error}")))
        {
            Ok(inventory) => write_json(&mut stream, 200, &inventory).await,
            Err(error) => write_error(&mut stream, 422, &error).await,
        };
    }
    let query = parse_query(raw_query);
    if let Some(mode) = query.get("digest") {
        if name.is_some() {
            return write_error(&mut stream, 400, "digests require a collection").await;
        }
        let expected_root = query.get("expectedRoot").cloned().unwrap_or_default();
        let query = match mode.as_str() {
            "root" => flotilla_resources::DigestQuery::Root,
            "children" if !expected_root.is_empty() => flotilla_resources::DigestQuery::Children { expected_root },
            "snapshot" if !expected_root.is_empty() => {
                let Some(bucket) = query.get("bucket").and_then(|value| value.parse::<u8>().ok()) else {
                    return write_error(&mut stream, 400, "invalid digest bucket").await;
                };
                flotilla_resources::DigestQuery::Snapshot { expected_root, bucket }
            }
            _ => return write_error(&mut stream, 400, "invalid digest query").await,
        };
        return match flotilla_resources::digest_resource_kind(&backend, namespace, kind, &query).await {
            Ok(digest) => write_json(&mut stream, 200, &serde_json::to_value(digest).map_err(|error| error.to_string())?).await,
            Err(error) => write_resource_error(&mut stream, error).await,
        };
    }
    let include_replicas = query_flag(&query, &["includeReplicas", "include-replicas", "include_replicas"]);
    let replica_sources = query_flag(&query, &["replicaSources", "replica-sources", "replica_sources"]);
    let all_provenances = query_flag(&query, &["allProvenances", "all-provenances", "all_provenances"]);
    let watch = query.get("watch").is_some_and(|value| value == "true");

    if let Some(name) = name {
        if watch || replica_sources || (all_provenances && include_replicas) {
            return write_error(&mut stream, 400, "unsupported resource point read options").await;
        }
        if all_provenances {
            return match get_resource_kind_all_provenances(&backend, namespace, kind, name).await {
                Ok(listed) => write_json(&mut stream, 200, &listed.value).await,
                Err(error) => write_resource_error(&mut stream, error).await,
            };
        }
        let object = if include_replicas {
            get_resource_kind_including_replicas(&backend, namespace, kind, name).await
        } else {
            get_resource_kind(&backend, namespace, kind, name).await
        };
        return match object {
            Ok(object) => write_json(&mut stream, 200, &object.value).await,
            Err(error) => write_resource_error(&mut stream, error).await,
        };
    }

    if all_provenances {
        return write_error(&mut stream, 400, "allProvenances requires a resource name").await;
    }

    if let Some(encoded) = query.get("messageQuery") {
        if kind != "messages" || watch || replica_sources {
            return write_error(&mut stream, 400, "messageQuery requires a Message collection read").await;
        }
        let filter: flotilla_resources::MessageQuery = match serde_json::from_str(encoded) {
            Ok(filter) => filter,
            Err(error) => return write_error(&mut stream, 400, &format!("invalid Message query: {error}")).await,
        };
        let items = if include_replicas {
            backend.query_messages(namespace, &filter).await
        } else {
            backend.using::<flotilla_resources::Message>(namespace).query(&filter).await.map(|items| {
                items
                    .into_iter()
                    .map(|object| flotilla_resources::ReadResourceObject {
                        object,
                        provenance: flotilla_resources::ResourceProvenance::Local,
                    })
                    .collect()
            })
        };
        return match items {
            Ok(items) => {
                let document = flotilla_resources::message_query_document(&items).map_err(|error| error.to_string())?;
                write_json(&mut stream, 200, &document).await
            }
            Err(error) => write_resource_error(&mut stream, error).await,
        };
    }

    if !watch {
        let listed = if replica_sources {
            list_resource_kind_replica_sources(&backend, namespace, kind).await
        } else if include_replicas {
            list_resource_kind_including_replicas(&backend, namespace, kind).await
        } else {
            list_resource_kind(&backend, namespace, kind).await
        };
        return match listed {
            Ok(listed) => write_json(&mut stream, 200, &listed.value).await,
            Err(error) => write_resource_error(&mut stream, error).await,
        };
    }

    let watched = if replica_sources {
        if query.contains_key("resourceVersion") {
            Err(ResourceError::invalid("replica-source watches do not support resourceVersion resume"))
        } else {
            watch_resource_kind_replica_sources(&backend, namespace, kind).await
        }
    } else if include_replicas {
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

fn query_flag(query: &BTreeMap<String, String>, names: &[&str]) -> bool {
    names.iter().filter_map(|name| query.get(*name)).any(|value| value == "true")
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
        ResourceError::FieldOwnership { .. } => 409,
        ResourceError::Unauthorized { .. } => 403,
        // FinalizerPending is consumed by the controller loop and cannot reach this API.
        ResourceError::Other { .. } | ResourceError::FinalizerPending | ResourceError::FinalizerWait { .. } => 500,
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
        422 => "Unprocessable Entity",
        _ => "Internal Server Error",
    }
}
