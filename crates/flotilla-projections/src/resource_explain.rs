//! Resource watch and explain projections.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use flotilla_protocol::{
    CommandValue, DaemonEvent, EvidenceFreshness, ExplainedCondition, NodeId, RepoIdentity, ResourceCursor, ResourceReadEnvelope,
    ResourceReadRecord, ResourceRecordProvenance, ResourceRecordType, StepStatus,
};
use flotilla_resources::{ConditionValue, IntegrationCondition, ResourceError, ResourceProvenance, WatchStart};
use flotilla_store::{
    watch_resource_kind, watch_resource_kind_from, watch_resource_kind_including_replicas, watch_resource_kind_replica_sources,
    ResourceBackend,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use flotilla_daemon_api::event_sink::EventSink;

#[derive(bon::Builder)]
pub struct ResourceWatchCommandContext {
    backend: ResourceBackend,
    namespace: String,
    kind: String,
    name: Option<String>,
    include_replicas: bool,
    replica_sources: bool,
    cursor: Option<ResourceCursor>,
    command_id: u64,
    node_id: NodeId,
    repo_identity: RepoIdentity,
    event_sink: Arc<dyn EventSink>,
    token: CancellationToken,
}

pub async fn run_resource_watch_command(context: ResourceWatchCommandContext) -> CommandValue {
    let resuming = context.cursor.is_some();
    let start = match context.cursor.as_ref().map(ResourceCursor::position).transpose() {
        Ok(position) => position.map(|(resource_version, generation)| match generation {
            Some(generation) => WatchStart::FromVersionInGeneration { generation, resource_version },
            None => WatchStart::FromVersion(resource_version),
        }),
        Err(message) => return CommandValue::Error { message },
    };
    // This routed continuous consumer owns retry; raw registry APIs keep their
    // bounded read deadline. Cancellation below also drops any partial setup.
    let setup = flotilla_store::retry_store_read(|| async {
        match (context.replica_sources, context.include_replicas, start.clone()) {
            (true, _, Some(_)) => Err(ResourceError::invalid("replica-source watches do not support cursor resume")),
            (true, _, None) => watch_resource_kind_replica_sources(&context.backend, &context.namespace, &context.kind).await,
            (false, true, Some(_)) => Err(ResourceError::invalid("include-replicas watches do not support cursor resume")),
            (false, true, None) => watch_resource_kind_including_replicas(&context.backend, &context.namespace, &context.kind).await,
            (false, false, Some(start)) => watch_resource_kind_from(&context.backend, &context.namespace, &context.kind, start).await,
            (false, false, None) => watch_resource_kind(&context.backend, &context.namespace, &context.kind).await,
        }
    });
    let result = tokio::select! {
        biased;
        _ = context.token.cancelled() => return CommandValue::Cancelled,
        result = setup => result,
    };
    let watch = match result {
        Ok(watch) => watch,
        Err(error) => return CommandValue::Error { message: error.to_string() },
    };

    let resource_kind = watch.kind;
    let plural = watch.plural;
    let namespace = watch.namespace;
    let initial_resource_version = watch.resource_version;
    let generation = watch.generation;
    let initial_cursor = ResourceCursor::from_position(initial_resource_version.clone(), generation.clone());
    if !resuming {
        let initial = watch
            .initial
            .into_iter()
            .filter_map(|event| resource_watch_record(event, &context.node_id).transpose())
            .collect::<Result<Vec<_>, _>>();
        let initial = match initial {
            Ok(initial) => initial.into_iter().filter(|record| resource_record_matches_name(record, context.name.as_deref())).collect(),
            Err(message) => return CommandValue::Error { message },
        };
        if context.token.is_cancelled() {
            return CommandValue::Cancelled;
        }
        send_resource_watch_event(
            &context.event_sink,
            context.command_id,
            &context.node_id,
            &context.repo_identity,
            resource_read_envelope(resource_kind.clone(), plural.clone(), namespace.clone(), initial_cursor.clone(), initial),
        );
    }
    send_resource_watch_event(
        &context.event_sink,
        context.command_id,
        &context.node_id,
        &context.repo_identity,
        resource_read_envelope(
            resource_kind.clone(),
            plural.clone(),
            namespace.clone(),
            initial_cursor,
            vec![ResourceReadRecord {
                record_type: ResourceRecordType::Bookmark,
                provenance: ResourceRecordProvenance::Local { node_id: context.node_id.clone() },
                object: None,
            }],
        ),
    );

    let mut stream = watch.stream;
    loop {
        tokio::select! {
            _ = context.token.cancelled() => return CommandValue::Cancelled,
            event = stream.next() => {
                match event {
                    Some(Ok(event)) => {
                        let resource_version = event["object"]["metadata"]["resourceVersion"]
                            .as_str()
                            .unwrap_or(&initial_resource_version)
                            .to_string();
                        let record = match resource_watch_record(event, &context.node_id) {
                            Ok(Some(record)) => record,
                            Ok(None) => continue,
                            Err(message) => return CommandValue::Error { message },
                        };
                        if resource_record_matches_name(&record, context.name.as_deref()) {
                            send_resource_watch_event(
                                &context.event_sink,
                                context.command_id,
                                &context.node_id,
                                &context.repo_identity,
                                resource_read_envelope(
                                    resource_kind.clone(),
                                    plural.clone(),
                                    namespace.clone(),
                                    ResourceCursor::from_position(resource_version, generation.clone()),
                                    vec![record],
                                ),
                            );
                        }
                    }
                    Some(Err(error)) => return CommandValue::Error { message: error.to_string() },
                    None => return CommandValue::Ok,
                }
            }
        }
    }
}

fn send_resource_watch_event(
    event_sink: &Arc<dyn EventSink>,
    command_id: u64,
    node_id: &NodeId,
    repo_identity: &RepoIdentity,
    response: ResourceReadEnvelope,
) {
    let description = format!(
        "{} {}",
        response.records.first().map(|record| format!("{:?}", record.record_type)).unwrap_or_else(|| "CURRENT".to_string()),
        response.resource_kind
    );
    event_sink.emit(DaemonEvent::CommandStepUpdate {
        command_id,
        node_id: node_id.clone(),
        repo_identity: repo_identity.clone(),
        repo: None,
        step_index: 0,
        step_count: 1,
        description,
        status: StepStatus::Produced { value: Box::new(CommandValue::ResourceWatchEvent(Box::new(response))) },
    });
}

pub fn resource_read_envelope(
    resource_kind: String,
    plural: String,
    namespace: String,
    cursor: ResourceCursor,
    records: Vec<ResourceReadRecord>,
) -> ResourceReadEnvelope {
    ResourceReadEnvelope { api_version: "flotilla.work/v1".to_string(), resource_kind, plural, namespace, cursor, records }
}

pub fn resource_record(record_type: ResourceRecordType, object: serde_json::Value, local_node_id: &NodeId) -> ResourceReadRecord {
    let annotations = object.get("metadata").and_then(|metadata| metadata.get("annotations"));
    let origin_root = annotations.and_then(|annotations| annotations.get("flotilla.work/origin-root")).and_then(|value| value.as_str());
    let last_synced_at =
        annotations.and_then(|annotations| annotations.get("flotilla.work/last-synced-at")).and_then(|value| value.as_str());
    let provenance = match (origin_root, last_synced_at) {
        (Some(origin_root), Some(last_synced_at)) => {
            ResourceRecordProvenance::Replica { origin_root: NodeId::new(origin_root), last_synced_at: last_synced_at.to_string() }
        }
        _ => ResourceRecordProvenance::Local { node_id: local_node_id.clone() },
    };
    ResourceReadRecord { record_type, provenance, object: Some(object) }
}

pub fn explained_provenance(provenance: &ResourceProvenance, local_node_id: &NodeId) -> ResourceRecordProvenance {
    match provenance {
        ResourceProvenance::Local => ResourceRecordProvenance::Local { node_id: local_node_id.clone() },
        ResourceProvenance::Replica { origin_root, last_synced_at } => {
            ResourceRecordProvenance::Replica { origin_root: origin_root.clone(), last_synced_at: last_synced_at.to_rfc3339() }
        }
    }
}

pub fn observed_freshness(observed_at: Option<DateTime<Utc>>, now: DateTime<Utc>, ttl: Duration) -> EvidenceFreshness {
    match observed_at.and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok()) {
        Some(age) if age < ttl => EvidenceFreshness::Fresh,
        Some(_) => EvidenceFreshness::Stale,
        None => EvidenceFreshness::Missing,
    }
}

pub fn explain_condition(condition: &IntegrationCondition, now: DateTime<Utc>, ttl: Duration) -> ExplainedCondition {
    let observed_at = condition.observed_at.as_deref().and_then(|value| DateTime::parse_from_rfc3339(value).ok()).map(|at| at.to_utc());
    ExplainedCondition {
        value: match condition.value {
            ConditionValue::True => "true",
            ConditionValue::False => "false",
            ConditionValue::Unknown => "unknown",
        }
        .to_string(),
        observed_at: condition.observed_at.clone(),
        freshness: observed_freshness(observed_at, now, ttl),
        details: condition.details.clone(),
    }
}

fn resource_watch_record(event: serde_json::Value, local_node_id: &NodeId) -> Result<Option<ResourceReadRecord>, String> {
    let Some(event_type) = event.get("type").and_then(|value| value.as_str()) else {
        return Err("resource watch event is missing type".to_string());
    };
    if event_type == "BOOKMARK" {
        return Ok(None);
    }
    let record_type = match event_type {
        "ADDED" => ResourceRecordType::Added,
        "MODIFIED" => ResourceRecordType::Modified,
        "DELETED" => ResourceRecordType::Deleted,
        other => return Err(format!("unknown resource watch event type '{other}'")),
    };
    let object = event.get("object").cloned().ok_or_else(|| "resource watch event is missing object".to_string())?;
    Ok(Some(resource_record(record_type, object, local_node_id)))
}

fn resource_record_matches_name(record: &ResourceReadRecord, name: Option<&str>) -> bool {
    name.is_none_or(|name| {
        record
            .object
            .as_ref()
            .and_then(|object| object.get("metadata"))
            .and_then(|metadata| metadata.get("name"))
            .and_then(|value| value.as_str())
            == Some(name)
    })
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use flotilla_daemon_api::event_sink::BroadcastEventSink;
    use flotilla_store_memory::InMemoryBackend;

    // Cancellation owns setup as well as stream consumption. A cancelled
    // command must return before even an immediately failing registry lookup.
    #[tokio::test]
    async fn cancelled_watch_does_not_start_setup_or_emit_events() {
        let (tx, mut events) = tokio::sync::broadcast::channel(8);
        let token = CancellationToken::new();
        token.cancel();
        let context = ResourceWatchCommandContext::builder()
            .backend(ResourceBackend::new(InMemoryBackend::default()))
            .namespace("flotilla".into())
            .kind("not-a-registered-kind".into())
            .include_replicas(false)
            .replica_sources(false)
            .command_id(1)
            .node_id(NodeId::new("local"))
            .repo_identity(RepoIdentity { authority: String::new(), path: String::new() })
            .event_sink(Arc::new(BroadcastEventSink::new(tx)))
            .token(token)
            .build();
        assert!(matches!(run_resource_watch_command(context).await, CommandValue::Cancelled));
        assert!(events.try_recv().is_err());
    }
}
