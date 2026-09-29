//! Resource watch and explain projections.

use std::time::Duration;

use chrono::{DateTime, Utc};
use flotilla_protocol::{
    CommandValue, DaemonEvent, EvidenceFreshness, ExplainedCondition, ExplainedUnmetExpectation, NodeId, RepoIdentity, ResourceCursor,
    ResourceReadEnvelope, ResourceReadRecord, ResourceRecordProvenance, ResourceRecordType, StepStatus,
};
use flotilla_resources::{
    watch_resource_kind, watch_resource_kind_from, watch_resource_kind_including_replicas, watch_resource_kind_replica_sources,
    ConditionValue, IntegrationCondition, ResourceBackend, ResourceError, ResourceProvenance, UnmetSettlementExpectation, WatchStart,
};
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(bon::Builder)]
pub(crate) struct ResourceWatchCommandContext {
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
    event_tx: broadcast::Sender<DaemonEvent>,
    token: CancellationToken,
}

pub(crate) async fn run_resource_watch_command(context: ResourceWatchCommandContext) -> CommandValue {
    let resuming = context.cursor.is_some();
    let start = match context.cursor.as_ref().map(ResourceCursor::position).transpose() {
        Ok(position) => position.map(|(resource_version, generation)| match generation {
            Some(generation) => WatchStart::FromVersionInGeneration { generation, resource_version },
            None => WatchStart::FromVersion(resource_version),
        }),
        Err(message) => return CommandValue::Error { message },
    };
    let result = match (context.replica_sources, context.include_replicas, start) {
        (true, _, Some(_)) => Err(ResourceError::invalid("replica-source watches do not support cursor resume")),
        (true, _, None) => watch_resource_kind_replica_sources(&context.backend, &context.namespace, &context.kind).await,
        (false, true, Some(_)) => Err(ResourceError::invalid("include-replicas watches do not support cursor resume")),
        (false, true, None) => watch_resource_kind_including_replicas(&context.backend, &context.namespace, &context.kind).await,
        (false, false, Some(start)) => watch_resource_kind_from(&context.backend, &context.namespace, &context.kind, start).await,
        (false, false, None) => watch_resource_kind(&context.backend, &context.namespace, &context.kind).await,
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
            &context.event_tx,
            context.command_id,
            &context.node_id,
            &context.repo_identity,
            resource_read_envelope(resource_kind.clone(), plural.clone(), namespace.clone(), initial_cursor.clone(), initial),
        );
    }
    send_resource_watch_event(
        &context.event_tx,
        context.command_id,
        &context.node_id,
        &context.repo_identity,
        resource_read_envelope(resource_kind.clone(), plural.clone(), namespace.clone(), initial_cursor, vec![ResourceReadRecord {
            record_type: ResourceRecordType::Bookmark,
            provenance: ResourceRecordProvenance::Local { node_id: context.node_id.clone() },
            object: None,
        }]),
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
                                &context.event_tx,
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
    event_tx: &broadcast::Sender<DaemonEvent>,
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
    let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
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

pub(crate) fn resource_read_envelope(
    resource_kind: String,
    plural: String,
    namespace: String,
    cursor: ResourceCursor,
    records: Vec<ResourceReadRecord>,
) -> ResourceReadEnvelope {
    ResourceReadEnvelope { api_version: "flotilla.work/v1".to_string(), resource_kind, plural, namespace, cursor, records }
}

pub(crate) fn resource_record(record_type: ResourceRecordType, object: serde_json::Value, local_node_id: &NodeId) -> ResourceReadRecord {
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

pub(crate) fn explained_provenance(provenance: &ResourceProvenance, local_node_id: &NodeId) -> ResourceRecordProvenance {
    match provenance {
        ResourceProvenance::Local => ResourceRecordProvenance::Local { node_id: local_node_id.clone() },
        ResourceProvenance::Replica { origin_root, last_synced_at } => {
            ResourceRecordProvenance::Replica { origin_root: origin_root.clone(), last_synced_at: last_synced_at.to_rfc3339() }
        }
    }
}

pub(crate) fn observed_freshness(observed_at: Option<DateTime<Utc>>, now: DateTime<Utc>, ttl: Duration) -> EvidenceFreshness {
    match observed_at.and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok()) {
        Some(age) if age < ttl => EvidenceFreshness::Fresh,
        Some(_) => EvidenceFreshness::Stale,
        None => EvidenceFreshness::Missing,
    }
}

pub(crate) fn explain_condition(condition: &IntegrationCondition, now: DateTime<Utc>, ttl: Duration) -> ExplainedCondition {
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

pub(crate) fn explain_unmet_expectation(expectation: UnmetSettlementExpectation) -> ExplainedUnmetExpectation {
    match expectation {
        UnmetSettlementExpectation::CompletionConditionUnsatisfied { subject, field_path, value } => ExplainedUnmetExpectation {
            reason: "completion_condition_unsatisfied".to_string(),
            detail: format!("{field_path} is {}", value.unwrap_or_else(|| "unavailable".to_string())),
            subject,
        },
        UnmetSettlementExpectation::ChangeRequestNotReady { record, detail } => ExplainedUnmetExpectation {
            reason: "change_request_not_ready".to_string(),
            subject: format!("change_request/{record}"),
            detail,
        },
        UnmetSettlementExpectation::InvalidExpectedCheckouts { message } => {
            ExplainedUnmetExpectation { reason: "invalid_expected_checkouts".to_string(), subject: "convoy".to_string(), detail: message }
        }
        UnmetSettlementExpectation::ExitEntryAwaitingBinding { disposition, subject } => ExplainedUnmetExpectation {
            reason: "missing_binding".to_string(),
            subject: format!("exit/{disposition}"),
            detail: format!("entry awaits a bound change request for {subject}"),
        },
        UnmetSettlementExpectation::MissingCheckout { checkout } => ExplainedUnmetExpectation {
            reason: "missing_record".to_string(),
            subject: format!("checkout/{checkout}"),
            detail: "expected checkout has no federated record".to_string(),
        },
        UnmetSettlementExpectation::MissingCheckoutStatus { checkout } => ExplainedUnmetExpectation {
            reason: "missing_status".to_string(),
            subject: format!("checkout/{checkout}"),
            detail: "observed checkout has no status".to_string(),
        },
        UnmetSettlementExpectation::CheckoutConditionFalse { checkout, condition } => ExplainedUnmetExpectation {
            reason: "false_condition".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: format!("{condition} is false"),
        },
        UnmetSettlementExpectation::CheckoutConditionUnknown { checkout, condition } => ExplainedUnmetExpectation {
            reason: "unknown_condition".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: format!("{condition} is unknown"),
        },
        UnmetSettlementExpectation::StaleCheckoutEvidence { checkout, condition, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: observed_at.map_or_else(|| "evidence has no observation time".to_string(), |at| format!("observed at {at}")),
        },
        UnmetSettlementExpectation::MissingChangeRequest { record } => ExplainedUnmetExpectation {
            reason: "missing_record".to_string(),
            subject: format!("change_request/{record}"),
            detail: "expected change request has no federated observation".to_string(),
        },
        UnmetSettlementExpectation::StaleChangeRequest { record, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("change_request/{record}.state"),
            detail: observed_at.map_or_else(|| "state has no observation time".to_string(), |at| format!("observed at {at}")),
        },
        UnmetSettlementExpectation::ChangeRequestConditionFalse { record, value } => ExplainedUnmetExpectation {
            reason: "false_condition".to_string(),
            subject: format!("change_request/{record}.state"),
            detail: value.map_or_else(|| "state is unknown".to_string(), |value| format!("state is {value}")),
        },
        UnmetSettlementExpectation::InvalidCondition { subject, message } => {
            ExplainedUnmetExpectation { reason: "invalid_condition".to_string(), subject, detail: message }
        }
        UnmetSettlementExpectation::MissingObservedRef { reference } => ExplainedUnmetExpectation {
            reason: "missing_observation".to_string(),
            subject: format!("remote_ref/{reference}"),
            detail: "the claimed ref has not been observed through Git transport".to_string(),
        },
        UnmetSettlementExpectation::StaleObservedRef { reference, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("remote_ref/{reference}"),
            detail: format!("observed at {observed_at}"),
        },
        UnmetSettlementExpectation::ObservedDigestMismatch { reference, claimed, observed } => ExplainedUnmetExpectation {
            reason: "digest_mismatch".to_string(),
            subject: format!("remote_ref/{reference}"),
            detail: format!("claim names {claimed}, observed {observed}"),
        },
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
