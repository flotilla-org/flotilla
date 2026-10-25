//! The resource API's request handling, independent of how the response is written; the
//! router renders each [`Reply`] as an axum response.

use std::{collections::BTreeMap, sync::Arc};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_resources::{ResourceError, WatchStart};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{
    get_resource_kind, get_resource_kind_all_provenances, get_resource_kind_including_replicas, list_resource_kind,
    list_resource_kind_including_replicas, list_resource_kind_replica_sources, registered_resource_namespaces, watch_resource_kind,
    watch_resource_kind_from, watch_resource_kind_including_replicas, watch_resource_kind_replica_sources, DynamicResourceWatch,
    ResourceBackend, REGISTERED_RESOURCE_KINDS,
};

/// What the resource API answers, before it is written to a connection.
pub(in crate::server) enum Reply {
    Json {
        status: u16,
        body: serde_json::Value,
    },
    /// Newline-delimited JSON watch events until the watch ends.
    Watch(DynamicResourceWatch),
    /// One encoded point read.
    Record {
        service: Arc<Service>,
        kind: String,
        name: String,
        now: flotilla_schema::Timestamp,
    },
}

impl Reply {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self::Json { status, body }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, serde_json::json!({ "message": message }))
    }

    fn serialized(value: Result<serde_json::Value, String>) -> Self {
        match value {
            Ok(body) => Self::json(200, body),
            Err(error) => Self::error(500, &error),
        }
    }

    fn resource_error(error: ResourceError) -> Self {
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
        Self::error(status, &error.to_string())
    }
}

/// Answer one resource API request. `target` is the raw request target (path and query).
/// The optional daemon is solely a resource-handler test seam; production always supplies it.
pub(in crate::server) async fn respond(
    method: &str,
    target: &str,
    backend: ResourceBackend,
    daemon: Option<&Arc<InProcessDaemon>>,
) -> Reply {
    if method != "GET" {
        return Reply::error(405, "resource API is read-only");
    }

    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    if path == flotilla_schema_sqlite::stream::RECORD_PATH {
        let Some(daemon) = daemon else {
            return Reply::error(503, "decided store unavailable");
        };
        let service = match daemon.decided_store() {
            Ok(service) => service,
            Err(error) => return Reply::error(503, &error),
        };
        let query = parse_query(raw_query);
        let (Some(kind), Some(name)) = (query.get("kind"), query.get("name")) else {
            return Reply::error(400, "record requires kind and name");
        };
        return Reply::Record {
            service,
            kind: kind.clone(),
            name: name.clone(),
            now: flotilla_schema::Timestamp::from_datetime(&daemon.clock().now()),
        };
    }

    let observed_path = path == "/observed" || path.starts_with("/observed/");
    let additional_stores = if daemon.is_some() && !observed_path { vec!["/observed"] } else { Vec::new() };
    let (path, backend) = if observed_path {
        let path = path.strip_prefix("/observed").expect("observed store prefix");
        let Some(daemon) = daemon else {
            return Reply::error(404, "observed resource store unavailable");
        };
        (path, daemon.observed_resource_backend())
    } else {
        (path, backend)
    };
    if path == "/apis/flotilla.work/v1" {
        let namespaces = match registered_resource_namespaces(&backend).await {
            Ok(namespaces) => namespaces,
            Err(error) => return Reply::resource_error(error),
        };
        return Reply::json(
            200,
            serde_json::json!({"kinds": REGISTERED_RESOURCE_KINDS.iter().map(|kind| kind.plural).collect::<Vec<_>>(), "namespaces": namespaces, "stores": additional_stores }),
        );
    }
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let (namespace, kind, name) = match segments.as_slice() {
        ["apis", "flotilla.work", "v1", "namespaces", namespace, kind] => (*namespace, *kind, None),
        ["apis", "flotilla.work", "v1", "namespaces", namespace, kind, name] => (*namespace, *kind, Some(*name)),
        _ => return Reply::error(404, "unknown resource API path"),
    };
    // Administrative read-only inventory, alongside kind discovery on the
    // same trusted local resource API. Return raw bytes so only the candidate
    // applies its parser.
    if kind == "charterinputs" && name.is_none() {
        let Some(daemon) = daemon else {
            return Reply::error(404, "charter inventory unavailable");
        };
        return match daemon
            .charter_input_inventory(namespace)
            .await
            .and_then(|inventory| serde_json::to_value(inventory).map_err(|error| error.to_string()))
        {
            Ok(inventory) => Reply::json(200, inventory),
            Err(error) => Reply::error(422, &error),
        };
    }
    if kind == "operationalentries" && name.is_none() {
        let Some(daemon) = daemon else {
            return Reply::error(404, "ops inventory unavailable");
        };
        return match daemon
            .project_operational_entry_inventory(namespace)
            .await
            .and_then(|inventory| serde_json::to_value(inventory).map_err(|error| format!("serialize ops inventory: {error}")))
        {
            Ok(inventory) => Reply::json(200, inventory),
            Err(error) => Reply::error(422, &error),
        };
    }
    let query = parse_query(raw_query);
    if let Some(mode) = query.get("digest") {
        if name.is_some() {
            return Reply::error(400, "digests require a collection");
        }
        let expected_root = query.get("expectedRoot").cloned().unwrap_or_default();
        let query = match mode.as_str() {
            "root" => flotilla_resources::DigestQuery::Root,
            "children" if !expected_root.is_empty() => flotilla_resources::DigestQuery::Children { expected_root },
            "snapshot" if !expected_root.is_empty() => {
                let Some(bucket) = query.get("bucket").and_then(|value| value.parse::<u8>().ok()) else {
                    return Reply::error(400, "invalid digest bucket");
                };
                flotilla_resources::DigestQuery::Snapshot { expected_root, bucket }
            }
            _ => return Reply::error(400, "invalid digest query"),
        };
        return match flotilla_store::digest_resource_kind(&backend, namespace, kind, &query).await {
            Ok(digest) => Reply::serialized(serde_json::to_value(digest).map_err(|error| error.to_string())),
            Err(error) => Reply::resource_error(error),
        };
    }
    let include_replicas = query_flag(&query, &["includeReplicas", "include-replicas", "include_replicas"]);
    let replica_sources = query_flag(&query, &["replicaSources", "replica-sources", "replica_sources"]);
    let all_provenances = query_flag(&query, &["allProvenances", "all-provenances", "all_provenances"]);
    let watch = query.get("watch").is_some_and(|value| value == "true");

    if let Some(name) = name {
        if watch
            || replica_sources
            || (all_provenances && include_replicas)
            || ["storeQuery", "labelSelector", "fieldSelector", "messageQuery"].iter().any(|key| query.contains_key(*key))
        {
            return Reply::error(400, "unsupported resource point read options");
        }
        if all_provenances {
            return match get_resource_kind_all_provenances(&backend, namespace, kind, name).await {
                Ok(listed) => Reply::json(200, listed.value),
                Err(error) => Reply::resource_error(error),
            };
        }
        let object = if include_replicas {
            get_resource_kind_including_replicas(&backend, namespace, kind, name).await
        } else {
            get_resource_kind(&backend, namespace, kind, name).await
        };
        return match object {
            Ok(object) => Reply::json(200, object.value),
            Err(error) => Reply::resource_error(error),
        };
    }

    if all_provenances {
        return Reply::error(400, "allProvenances requires a resource name");
    }

    if query.contains_key("storeQuery") || query.contains_key("labelSelector") || query.contains_key("fieldSelector") {
        if watch || replica_sources {
            return Reply::error(400, "declared queries require a collection read");
        }
        let mut filter: flotilla_store::StoreQuery = if let Some(encoded) = query.get("storeQuery") {
            match serde_json::from_str(encoded) {
                Ok(query) => query,
                Err(error) => return Reply::error(400, &format!("invalid store query: {error}")),
            }
        } else {
            flotilla_store::StoreQuery { include_replicas, suppress_self_origin: true, ..Default::default() }
        };
        for (parameter, fields) in [("labelSelector", &mut filter.labels), ("fieldSelector", &mut filter.fields)] {
            if let Some(selector) = query.get(parameter) {
                for expression in selector.split(',').filter(|part| !part.is_empty()) {
                    let Some((key, value)) = expression.split_once('=') else {
                        return Reply::error(400, "selectors require key=value");
                    };
                    if key.is_empty()
                        || !key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
                        || (parameter == "labelSelector" && value.contains(['=', '(', ')', '!']))
                        || fields.insert(key.into(), value.into()).is_some()
                    {
                        return Reply::error(400, "unsupported selector syntax or duplicate selector key");
                    }
                }
            }
        }
        return match flotilla_store::query_resource_kind(&backend, namespace, kind, &filter).await {
            Ok(document) => Reply::json(200, document),
            Err(error) => Reply::resource_error(error),
        };
    }

    if let Some(encoded) = query.get("messageQuery") {
        if kind != "messages" || watch || replica_sources {
            return Reply::error(400, "messageQuery requires a Message collection read");
        }
        let filter: flotilla_store::MessageQuery = match serde_json::from_str(encoded) {
            Ok(filter) => filter,
            Err(error) => return Reply::error(400, &format!("invalid Message query: {error}")),
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
            Ok(items) => Reply::serialized(flotilla_store::message_query_document(&items).map_err(|error| error.to_string())),
            Err(error) => Reply::resource_error(error),
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
            Ok(listed) => Reply::json(200, listed.value),
            Err(error) => Reply::resource_error(error),
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
        Ok(watch) => Reply::Watch(watch),
        Err(error) => Reply::resource_error(error),
    }
}

fn parse_query(raw_query: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(raw_query.as_bytes()).into_owned().collect()
}

fn query_flag(query: &BTreeMap<String, String>, names: &[&str]) -> bool {
    names.iter().filter_map(|name| query.get(*name)).any(|value| value == "true")
}

/// One watch event as its newline-terminated wire line.
pub(in crate::server) fn watch_line(event: &serde_json::Value) -> Result<Vec<u8>, String> {
    let mut encoded = serde_json::to_vec(event).map_err(|error| format!("encode resource watch event: {error}"))?;
    encoded.push(b'\n');
    Ok(encoded)
}
