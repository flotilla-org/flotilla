use crate::MessageQuery;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
type MessageIndexKeys = (Option<String>, Option<String>, Option<String>);

#[derive(Debug, Default)]
pub(super) struct MessageIndex {
    active: BTreeSet<String>,
    audit: BTreeMap<chrono::DateTime<chrono::Utc>, BTreeSet<String>>,
    audit_keys: HashMap<String, chrono::DateTime<chrono::Utc>>,
    receivers: BTreeMap<String, BTreeSet<String>>,
    replies: BTreeMap<String, BTreeSet<String>>,
    batches: BTreeMap<String, BTreeSet<String>>,
    keys: HashMap<String, MessageIndexKeys>,
}

impl MessageIndex {
    pub(super) fn update(&mut self, name: &str, value: Option<&Value>) {
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
        let active = !value
            .pointer("/status/phase")
            .and_then(|phase| serde_json::from_value::<flotilla_resources::MessagePhase>(phase.clone()).ok())
            .is_some_and(flotilla_resources::MessagePhase::is_terminal);
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
    pub(super) fn names(&self, query: &MessageQuery) -> Vec<String> {
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
