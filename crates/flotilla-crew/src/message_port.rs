//! The Message records port (#3317). Convoy and crew code reads and admits
//! Messages only through these methods: from the old store while the
//! `[primitives]` switch is off, and from the engine store's replica, in
//! the old shape, while it is on. Turning the switch off changes nothing
//! here: every method is then exactly the old-store read it replaced.

use flotilla_messaging::MessageAdmission;
use flotilla_resources::{InputMeta, Message, MessageSpec, ReadResourceObject, ResourceError};
use flotilla_store::StoreQuery;

use crate::crew_ops::CrewService;

impl CrewService {
    pub(crate) fn message_primitives(&self) -> Option<std::sync::Arc<dyn crate::ports::EngineMessages>> {
        self.message_path.get().cloned()
    }

    /// Whether an admitted Message is the intent a producer repeats. On the
    /// engine path a name is its producer key's digest, so the same name is
    /// the same intent; the old store compares the specs.
    pub(crate) fn same_message_intent(&self, admitted: &MessageSpec, intent: &MessageSpec) -> bool {
        self.message_primitives().is_some() || admitted.same_intent(intent)
    }

    /// One Message by name, local or replicated.
    pub(crate) async fn message_record(&self, namespace: &str, name: &str) -> Result<ReadResourceObject<Message>, ResourceError> {
        match self.message_primitives() {
            Some(primitives) => primitives.view(namespace, name).await?.ok_or_else(|| ResourceError::not_found(name)),
            None => self.resource_backend.including_replicas::<Message>(namespace).get(name).await,
        }
    }

    /// The open Messages, to `receiver` when given, local or replicated.
    pub(crate) async fn active_message_records(
        &self,
        namespace: &str,
        receiver: Option<&str>,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        match self.message_primitives() {
            Some(primitives) => Ok(primitives
                .views(namespace)
                .await?
                .into_iter()
                .filter(|record| record.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
                .filter(|record| receiver.is_none_or(|receiver| record.object.spec.receiver == receiver))
                .collect()),
            None => Ok(self
                .resource_backend
                .query_declared::<Message>(
                    namespace,
                    &StoreQuery {
                        message: Some(flotilla_store::MessageQuery::Active { receiver: receiver.map(str::to_owned) }),
                        ..Default::default()
                    }
                    .with_replicas(),
                )
                .await?
                .items),
        }
    }

    /// The open Messages to `receiver`, without this home's own replica
    /// echoes (`flotilla_store::ResourceBackend::query_messages`).
    pub(crate) async fn open_messages_to(
        &self,
        namespace: &str,
        receiver: &str,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        match self.message_primitives() {
            Some(_) => self.active_message_records(namespace, Some(receiver)).await,
            None => {
                self.resource_backend
                    .query_messages(namespace, &flotilla_store::MessageQuery::Active { receiver: Some(receiver.to_owned()) })
                    .await
            }
        }
    }

    /// The open Messages to receivers under `prefix` (a convoy's `project/convoy/`).
    pub(crate) async fn active_message_records_under(
        &self,
        namespace: &str,
        prefix: &str,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        match self.message_primitives() {
            Some(_) => Ok(self
                .active_message_records(namespace, None)
                .await?
                .into_iter()
                .filter(|record| record.object.spec.receiver.starts_with(prefix))
                .collect()),
            None => Ok(self
                .resource_backend
                .query_declared::<Message>(
                    namespace,
                    &StoreQuery {
                        message: Some(flotilla_store::MessageQuery::active()),
                        field_prefixes: std::collections::BTreeMap::from([("spec.receiver".into(), prefix.to_owned())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await?
                .items),
        }
    }

    /// The Messages with these names, local or replicated.
    pub(crate) async fn named_message_records(
        &self,
        namespace: &str,
        names: &[String],
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        match self.message_primitives() {
            Some(primitives) => {
                Ok(primitives.views(namespace).await?.into_iter().filter(|record| names.contains(&record.object.metadata.name)).collect())
            }
            None => Ok(self
                .resource_backend
                .query_declared::<Message>(
                    namespace,
                    &StoreQuery {
                        field_values: std::collections::BTreeMap::from([("metadata.name".into(), names.to_vec())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await?
                .items),
        }
    }

    /// Admits a Message at this home, the receiver's. On the engine path a
    /// repeated name is the message already admitted.
    pub(crate) async fn admit_message(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &MessageSpec,
    ) -> Result<MessageAdmission, ResourceError> {
        match self.message_primitives() {
            Some(primitives) => Ok(MessageAdmission::Accepted(primitives.admit(namespace, &meta.name, spec).await?)),
            None => self.message_inbox(namespace).await.accept(meta, spec, self.clock.now()).await,
        }
    }
}
