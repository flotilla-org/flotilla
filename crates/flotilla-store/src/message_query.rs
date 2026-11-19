// Indexed inbox reads: history is excluded before decoding resource bodies.
use crate::StoreExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Message, ReadResourceObject, ResourceBackend, ResourceError, ResourceObject, TypedResolver};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageQuery {
    Active { receiver: Option<String> },
    ReplyTo { name: String },
    AuditBefore { before: chrono::DateTime<chrono::Utc> },
}

impl MessageQuery {
    pub fn active() -> Self {
        Self::Active { receiver: None }
    }

    /// The receiver whose open messages the query selects, which an inbox index answers.
    pub(crate) fn open_receiver(&self) -> Option<&str> {
        match self {
            Self::Active { receiver } => receiver.as_deref(),
            _ => None,
        }
    }

    /// Whether `message` is one the query selects: the stores' message indexes, as a predicate
    /// over one message, reading only the paths the queries test.
    pub(crate) fn selects<T: crate::Resource>(&self, message: &ResourceObject<T>) -> Result<bool, ResourceError> {
        let texts = crate::object_index::path_texts(
            message,
            &["spec.receiver", "spec.in_reply_to", "status.phase", "spec.body_digest", "status.since"],
        )?;
        let [receiver, in_reply_to, phase, body_digest, since] = <[Vec<String>; 5]>::try_from(texts).expect("one list per path");
        fn text(texts: &[String]) -> Option<&str> {
            texts.first().map(String::as_str)
        }
        let active = !text(&phase)
            .and_then(|phase| {
                flotilla_resources::MessagePhase::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(phase)).ok()
            })
            .is_some_and(flotilla_resources::MessagePhase::is_terminal);
        Ok(match self {
            Self::Active { receiver: wanted } => active && wanted.as_deref().is_none_or(|wanted| text(&receiver) == Some(wanted)),
            Self::ReplyTo { name } => text(&in_reply_to) == Some(name.as_str()),
            Self::AuditBefore { before } => {
                !active
                    && body_digest.is_empty()
                    && text(&since)
                        .and_then(|since| chrono::DateTime::parse_from_rfc3339(since).ok())
                        .is_some_and(|since| since.with_timezone(&chrono::Utc) < *before)
            }
        })
    }
}

impl TypedResolver<Message> {
    /// This home's messages `query` selects.
    pub async fn query(&self, query: &MessageQuery) -> Result<Vec<ResourceObject<Message>>, ResourceError> {
        if self.backend.engine_serves(<Message as flotilla_resources::Resource>::API_PATHS.kind) {
            let query = crate::StoreQuery { message: Some(query.clone()), ..Default::default() };
            return Ok(self.query_declared(&query).await?.items.into_iter().map(|item| item.object).collect());
        }
        { self.backend.store().query_messages_typed(&self.namespace, query, false).await }
            .map(|items| items.into_iter().map(|item| item.object).collect())
    }
}

impl ResourceBackend {
    pub async fn query_messages(&self, namespace: &str, query: &MessageQuery) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self
            .query_declared::<Message>(
                namespace,
                &crate::StoreQuery {
                    message: Some(query.clone()),
                    include_replicas: true,
                    suppress_self_origin: true,
                    ..Default::default()
                },
            )
            .await?
            .items)
    }
}

pub fn message_query_document(items: &[ReadResourceObject<Message>]) -> Result<Value, ResourceError> {
    let items = items.iter().map(crate::registry::read_object_value).collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"MessageList", "metadata":{"resourceVersion":"0"}, "items":items}))
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{MessagePhase, MessageRelation, MessageSpec, MessageStatus, ObjectMeta};

    use super::*;

    fn at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).expect("timestamp")
    }

    fn message(phase: Option<MessagePhase>, body_digest: Option<&str>, in_reply_to: Option<&str>) -> ResourceObject<Message> {
        ResourceObject {
            metadata: ObjectMeta {
                name: "m".into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: Default::default(),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: at(1),
                merge: None,
            },
            spec: MessageSpec::builder()
                .sender("project:flotilla/convoy:a/role:coder".into())
                .receiver("bob".into())
                .relation(MessageRelation::Peer)
                .body("hello".into())
                .maybe_body_digest(body_digest.map(Into::into))
                .maybe_in_reply_to(in_reply_to.map(Into::into))
                .build(),
            status: phase.map(|phase| MessageStatus::builder().phase(phase).since(at(100)).build()),
        }
    }

    // Behaviour: a message query selects a message by its receiver, the message it replies to,
    // whether its phase is terminal, its body digest and the time it entered its phase.
    #[test]
    fn a_query_selects_messages_by_what_it_tests() {
        let open = message(Some(MessagePhase::Deliverable), None, Some("asked"));
        let unsent = message(None, None, None);
        let done = message(Some(MessagePhase::Satisfied), None, None);
        let kept = message(Some(MessagePhase::Satisfied), Some("digest"), None);
        let to = |receiver: &str| MessageQuery::Active { receiver: Some(receiver.into()) };
        let reply_to = |name: &str| MessageQuery::ReplyTo { name: name.into() };
        let audit = |seconds| MessageQuery::AuditBefore { before: at(seconds) };
        let cases = [
            (MessageQuery::active(), &open, true),
            (MessageQuery::active(), &unsent, true),
            (MessageQuery::active(), &done, false),
            (to("bob"), &open, true),
            (to("ann"), &open, false),
            (reply_to("asked"), &open, true),
            (reply_to("other"), &open, false),
            (reply_to("asked"), &done, false),
            (audit(101), &done, true),
            (audit(100), &done, false),
            (audit(101), &kept, false),
            (audit(101), &open, false),
            (audit(101), &unsent, false),
        ];
        for (query, message, selected) in cases {
            assert_eq!(query.selects(message).expect("selects"), selected, "{query:?} {:?}", message.status);
        }
    }
}
