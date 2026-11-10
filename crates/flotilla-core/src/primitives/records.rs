//! Message as the old store's typed paths see it (#3108): the generic resource verbs read the
//! engine's messages in their old shape, with each address as the engine stores it, at every home
//! that holds them, and an apply admits one. A message's state moves only
//! through its machine, so a status write or delete through those verbs is refused.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use flotilla_provisioning::engine::watch::EngineWatch;
use flotilla_resources::{
    engine_records::{EngineObject, EngineRecords},
    Message as OldMessage, MessageSpec as OldSpec, ReadResourceObject, ResourceProvenance,
};
use flotilla_schema::Kind as _;
use flotilla_state_machines::{machines::message::Message, Resource};
use futures::stream::BoxStream;

use super::MessagePrimitives;

const MESSAGE_KIND: &str = "Message";

/// The engine's messages served under the daemon's provisioning namespace, the one the old
/// shapes' role addresses are read in.
pub struct MessageRecords {
    primitives: Arc<MessagePrimitives>,
    namespace: Arc<RwLock<String>>,
}

impl MessageRecords {
    pub fn new(primitives: Arc<MessagePrimitives>, namespace: Arc<RwLock<String>>) -> Self {
        Self { primitives, namespace }
    }

    fn namespace(&self) -> String {
        self.namespace.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn object(&self, read: ReadResourceObject<OldMessage>) -> Result<EngineObject, String> {
        let home = match read.provenance {
            ResourceProvenance::Local => self.primitives.home().to_owned(),
            ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
        };
        let metadata = read.object.metadata;
        Ok(EngineObject {
            name: metadata.name,
            home,
            version: metadata.resource_version.parse().map_err(|error| format!("message version: {error}"))?,
            labels: metadata.labels,
            annotations: metadata.annotations,
            spec: serde_json::to_value(&read.object.spec).map_err(|error| error.to_string())?,
            status: read.object.status.map(|status| serde_json::to_value(&status)).transpose().map_err(|error| error.to_string())?,
        })
    }
}

impl std::fmt::Debug for MessageRecords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageRecords").field("home", &self.primitives.home()).finish()
    }
}

#[async_trait::async_trait]
impl EngineRecords for MessageRecords {
    fn kinds(&self) -> &'static [&'static str] {
        &[MESSAGE_KIND]
    }

    fn home(&self) -> String {
        self.primitives.home().to_owned()
    }

    async fn get(&self, _kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        let view = self.primitives.stored_view(&self.namespace(), name).map_err(|error| error.to_string())?;
        view.map(|read| self.object(read)).transpose()
    }

    async fn list(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        let views = self.primitives.stored_views(&self.namespace()).map_err(|error| error.to_string())?;
        views.into_iter().map(|read| self.object(read)).collect()
    }

    /// Each message has one home, its receiver's, so every source is its only record.
    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        self.list(kind).await
    }

    async fn apply(
        &self,
        _kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        spec: serde_json::Value,
    ) -> Result<EngineObject, String> {
        let namespace = self.namespace();
        let spec: OldSpec = serde_json::from_value(spec).map_err(|error| format!("Message spec: {error}"))?;
        self.primitives.admit(&namespace, name, &spec).await.map_err(|error| error.to_string())?;
        let view = self.primitives.stored_view(&namespace, name).map_err(|error| error.to_string())?;
        self.object(view.ok_or_else(|| format!("Message {name} is not readable after its admission"))?)
    }

    async fn set_status(&self, _kind: &str, name: &str, _status: serde_json::Value) -> Result<EngineObject, String> {
        Err(format!("Message {name}'s state moves only through delivery and receipts; acknowledge it with `flotilla message acknowledge`"))
    }

    async fn delete(&self, _kind: &str, name: &str) -> Result<(), String> {
        Err(format!("Message {name} is kept by its inbox until retention removes it; acknowledge it with `flotilla message acknowledge`"))
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let watch =
            EngineWatch::start(Arc::clone(self.primitives.service()), [Resource::<Message>::CODE]).map_err(|error| error.to_string())?;
        let stream = futures::stream::unfold(watch, |mut watch| async move {
            match watch.next(|_, _| Ok(())).await {
                Ok(()) => Some(((), watch)),
                Err(error) => {
                    tracing::warn!(%error, "message records watch ended");
                    None
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
