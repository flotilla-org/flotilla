//! The Message records port (#3317). Convoy and crew code reads and admits
//! Messages only through these methods, from the engine store's replica in
//! the old shape.

use flotilla_messaging::MessageAdmission;
use flotilla_resources::{InputMeta, Message, MessageSpec, ReadResourceObject, ResourceError};

use crate::crew_ops::CrewService;

impl CrewService {
    pub(crate) fn message_primitives(&self) -> Result<std::sync::Arc<dyn flotilla_messaging::EngineMessages>, ResourceError> {
        crate::ports::installed_messages(&self.message_path)
    }

    /// An intent with its sender and receiver as Messages read back spell them,
    /// for comparing parties.
    pub(crate) async fn spelled_message_intent(&self, namespace: &str, intent: &MessageSpec) -> Result<MessageSpec, ResourceError> {
        self.message_primitives()?.spelled(namespace, intent).await
    }

    /// Where the Message `intent` replies to stands, read as its admission reads it.
    pub(crate) async fn resolve_in_reply_to(&self, namespace: &str, intent: &MessageSpec) -> flotilla_messaging::MessageResolution {
        match self.message_primitives() {
            Ok(primitives) => primitives.resolve_in_reply_to(namespace, intent).await,
            Err(error) => flotilla_messaging::MessageResolution::Unknown(error.to_string()),
        }
    }

    /// One Message by name, local or replicated.
    pub(crate) async fn message_record(&self, namespace: &str, name: &str) -> Result<ReadResourceObject<Message>, ResourceError> {
        self.message_primitives()?.view(namespace, name).await?.ok_or_else(|| ResourceError::not_found(name))
    }

    /// The open Messages, to `receiver` when given, local or replicated.
    pub(crate) async fn active_message_records(
        &self,
        namespace: &str,
        receiver: Option<&str>,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self
            .message_primitives()?
            .views(namespace)
            .await?
            .into_iter()
            .filter(|record| record.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .filter(|record| receiver.is_none_or(|receiver| record.object.spec.receiver == receiver))
            .collect())
    }

    /// The open Messages to receivers under `prefix` (a convoy's `project/convoy/`).
    pub(crate) async fn active_message_records_under(
        &self,
        namespace: &str,
        prefix: &str,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self
            .active_message_records(namespace, None)
            .await?
            .into_iter()
            .filter(|record| record.object.spec.receiver.starts_with(prefix))
            .collect())
    }

    /// The Messages with these names, local or replicated.
    pub(crate) async fn named_message_records(
        &self,
        namespace: &str,
        names: &[String],
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self
            .message_primitives()?
            .views(namespace)
            .await?
            .into_iter()
            .filter(|record| names.contains(&record.object.metadata.name))
            .collect())
    }

    /// Admits a Message at this home, the receiver's. A message's name is
    /// its producer key's digest, so a repeated name is the message already
    /// admitted.
    pub(crate) async fn admit_message(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &MessageSpec,
    ) -> Result<MessageAdmission, ResourceError> {
        Ok(MessageAdmission::Accepted(self.message_primitives()?.admit(namespace, &meta.name, spec).await?))
    }
}
