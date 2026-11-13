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
    Batch { id: String },
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

    /// Whether `message`, a Message as its resource document, is one the query selects: the
    /// stores' message indexes, as a predicate over one message.
    pub(crate) fn selects(&self, message: &Value) -> bool {
        let text = |pointer: &str| message.pointer(pointer).and_then(Value::as_str);
        let active = !message
            .pointer("/status/phase")
            .and_then(|phase| serde_json::from_value::<flotilla_resources::MessagePhase>(phase.clone()).ok())
            .is_some_and(flotilla_resources::MessagePhase::is_terminal);
        match self {
            Self::Active { receiver } => active && receiver.as_deref().is_none_or(|receiver| text("/spec/receiver") == Some(receiver)),
            Self::ReplyTo { name } => text("/spec/in_reply_to") == Some(name.as_str()),
            Self::Batch { id } => text("/status/submission/batch_id") == Some(id.as_str()),
            Self::AuditBefore { before } => {
                !active
                    && message.pointer("/spec/body_digest").is_none_or(Value::is_null)
                    && text("/status/since")
                        .and_then(|since| chrono::DateTime::parse_from_rfc3339(since).ok())
                        .is_some_and(|since| since.with_timezone(&chrono::Utc) < *before)
            }
        }
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
