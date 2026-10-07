//! Indexed inbox reads: history is excluded before decoding resource bodies.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Message, ReadResourceObject, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, TypedResolver};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageQuery {
    Active { receiver: Option<String> },
    ReplyTo { name: String },
    Batch { id: String },
    AuditBefore { before: chrono::DateTime<chrono::Utc> },
}

impl MessageQuery {
    pub fn active() -> Self {
        Self::Active { receiver: None }
    }
    pub(crate) fn sql_index(&self, replicas: bool) -> &'static str {
        match (replicas, self) {
            (false, Self::Active { receiver: None }) => "resource_objects_message_inbox",
            (true, Self::Active { receiver: None }) => "replica_objects_message_inbox",
            (false, Self::Active { receiver: Some(_) }) => "resource_objects_message_active",
            (true, Self::Active { receiver: Some(_) }) => "replica_objects_message_active",
            (false, Self::ReplyTo { .. }) => "resource_objects_message_reply",
            (true, Self::ReplyTo { .. }) => "replica_objects_message_reply",
            (false, Self::Batch { .. }) => "resource_objects_message_batch",
            (true, Self::Batch { .. }) => "replica_objects_message_batch",
            (false, Self::AuditBefore { .. }) => "message_audit_age",
            (true, Self::AuditBefore { .. }) => "replica_message_audit_age",
        }
    }

    pub(crate) fn sql(&self) -> (&'static str, Option<String>) {
        match self {
            Self::Active { receiver: None } => ("COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered')", None),
            Self::Active { receiver: Some(receiver) } => ("COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered') AND json_extract(body_json, '$.spec.receiver') = ?5", Some(receiver.clone())),
            Self::ReplyTo { name } => ("json_extract(body_json, '$.spec.in_reply_to') = ?5", Some(name.clone())),
            Self::AuditBefore { before } => ("COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered') AND json_extract(body_json, '$.spec.body_digest') IS NULL AND julianday(json_extract(body_json, '$.status.since')) < julianday(?5)", Some(before.to_rfc3339())),
            Self::Batch { id } => ("json_extract(body_json, '$.status.submission.batch_id') = ?5", Some(id.clone())),
        }
    }
}

type MessageIndexKeys = (Option<String>, Option<String>, Option<String>);

#[derive(Debug, Default)]
pub(crate) struct MessageIndex {
    active: BTreeSet<String>,
    audit: BTreeMap<chrono::DateTime<chrono::Utc>, BTreeSet<String>>,
    audit_keys: HashMap<String, chrono::DateTime<chrono::Utc>>,
    receivers: BTreeMap<String, BTreeSet<String>>,
    replies: BTreeMap<String, BTreeSet<String>>,
    batches: BTreeMap<String, BTreeSet<String>>,
    keys: HashMap<String, MessageIndexKeys>,
}

impl MessageIndex {
    pub(crate) fn update(&mut self, name: &str, value: Option<&Value>) {
        self.active.remove(name);
        if let Some(at) = self.audit_keys.remove(name) {
            if let Some(names) = self.audit.get_mut(&at) {
                names.remove(name);
                if names.is_empty() {
                    self.audit.remove(&at);
                }
            }
        }
        if let Some((receiver, reply, batch)) = self.keys.remove(name) {
            for (map, key) in [(&mut self.receivers, receiver), (&mut self.replies, reply), (&mut self.batches, batch)] {
                if let Some(key) = key {
                    if let Some(names) = map.get_mut(&key) {
                        names.remove(name);
                        if names.is_empty() {
                            map.remove(&key);
                        }
                    }
                }
            }
        }
        let Some(value) = value.filter(|value| value["kind"] == "Message") else { return };
        let active = !matches!(
            value.pointer("/status/phase").and_then(Value::as_str),
            Some("satisfied" | "answered" | "outcome_met" | "expired" | "superseded" | "dead_lettered")
        );
        let receiver = active.then(|| value.pointer("/spec/receiver").and_then(Value::as_str).map(str::to_owned)).flatten();
        let reply = value.pointer("/spec/in_reply_to").and_then(Value::as_str).map(str::to_owned);
        let batch = value.pointer("/status/submission/batch_id").and_then(Value::as_str).map(str::to_owned);
        if active {
            self.active.insert(name.into());
        } else if value.pointer("/spec/body_digest").is_none_or(Value::is_null) {
            if let Some(at) = value
                .pointer("/status/since")
                .and_then(Value::as_str)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&chrono::Utc))
            {
                self.audit.entry(at).or_default().insert(name.into());
                self.audit_keys.insert(name.into(), at);
            }
        }
        for (map, key) in [(&mut self.receivers, &receiver), (&mut self.replies, &reply), (&mut self.batches, &batch)] {
            if let Some(key) = key {
                map.entry(key.clone()).or_default().insert(name.into());
            }
        }
        self.keys.insert(name.into(), (receiver, reply, batch));
    }
    pub(crate) fn names(&self, query: &MessageQuery) -> Vec<String> {
        let names = match query {
            MessageQuery::Active { receiver: None } => Some(&self.active),
            MessageQuery::Active { receiver: Some(receiver) } => self.receivers.get(receiver),
            MessageQuery::ReplyTo { name } => self.replies.get(name),
            MessageQuery::AuditBefore { before } => {
                return self.audit.range(..*before).flat_map(|(_, names)| names.iter().cloned()).collect()
            }
            MessageQuery::Batch { id } => self.batches.get(id),
        };
        names.into_iter().flatten().cloned().collect()
    }
}

impl TypedResolver<Message> {
    pub async fn query(&self, query: &MessageQuery) -> Result<Vec<ResourceObject<Message>>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.query_messages(&self.namespace, query, false).await,
            ResourceBackend::Sqlite(backend) => backend.query_messages(&self.namespace, query, false).await,
            ResourceBackend::Http(backend) => backend.query_messages(&self.namespace, query, false).await,
        }
        .map(|items| items.into_iter().map(|item| item.object).collect())
    }
}

impl ResourceBackend {
    pub async fn query_messages(&self, namespace: &str, query: &MessageQuery) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        let items = match self {
            Self::InMemory(backend) => backend.query_messages(namespace, query, true).await?,
            Self::Sqlite(backend) => backend.query_messages(namespace, query, true).await?,
            Self::Http(backend) => return backend.query_messages(namespace, query, true).await,
        };
        let local_root = self.local_root()?;
        let mut visible = Vec::new();
        for item in items {
            if let ResourceProvenance::Replica { origin_root, .. } = &item.provenance {
                if origin_root == &local_root {
                    match self.using::<Message>(namespace).get(&item.object.metadata.name).await {
                        Ok(_) => continue,
                        Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            visible.push(item);
        }
        Ok(visible)
    }
}

pub fn message_query_document(items: &[ReadResourceObject<Message>]) -> Result<Value, ResourceError> {
    let items = items.iter().map(crate::registry::read_object_value).collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"MessageList", "metadata":{"resourceVersion":"0"}, "items":items}))
}
