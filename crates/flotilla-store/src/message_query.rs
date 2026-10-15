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
}

impl TypedResolver<Message> {
    pub async fn query(&self, query: &MessageQuery) -> Result<Vec<ResourceObject<Message>>, ResourceError> {
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
