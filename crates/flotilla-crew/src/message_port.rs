//! The Message records port (#3317). Convoy and crew code reads and admits
//! Messages only through these methods, from the engine store's replica in
//! the old shape.

use flotilla_messaging::MessageAdmission;
use flotilla_resources::{InputMeta, Message, MessageSpec, ReadResourceObject, ResourceError};

use crate::crew_ops::CrewService;

/// The crew members' addresses a convoy's frozen workflow declares: the receivers its crew
/// Messages are sent to.
pub(crate) fn convoy_seats(namespace: &str, convoy: &flotilla_resources::ResourceObject<flotilla_resources::Convoy>) -> Vec<String> {
    let project = convoy.spec.project_ref.as_deref().unwrap_or(namespace);
    convoy
        .status
        .iter()
        .flat_map(|status| &status.workflow_snapshot)
        .flat_map(|workflow| &workflow.vessels)
        .flat_map(|vessel| {
            vessel
                .crew
                .iter()
                .map(|member| flotilla_resources::crew_role_address(project, &convoy.metadata.name, &vessel.name, &member.role))
        })
        .collect()
}

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

    /// The Message `name` in `receiver`'s inbox, local or replicated.
    pub(crate) async fn message_to(
        &self,
        namespace: &str,
        receiver: &str,
        name: &str,
    ) -> Result<ReadResourceObject<Message>, ResourceError> {
        self.message_primitives()?.view_to(namespace, receiver, name).await?.ok_or_else(|| ResourceError::not_found(name))
    }

    /// The Message `name` in the inbox `intent` is admitted to, local or replicated.
    pub(crate) async fn message_for(
        &self,
        namespace: &str,
        intent: &MessageSpec,
        name: &str,
    ) -> Result<ReadResourceObject<Message>, ResourceError> {
        self.message_primitives()?.view_for(namespace, intent, name).await?.ok_or_else(|| ResourceError::not_found(name))
    }

    /// The open Messages to `receiver`, local or replicated.
    pub(crate) async fn active_message_records(
        &self,
        namespace: &str,
        receiver: &str,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self
            .message_primitives()?
            .to_receivers(namespace, &[receiver.to_owned()])
            .await?
            .into_iter()
            .filter(|record| record.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .collect())
    }

    /// The Messages still waiting for delivery to `receivers`, local or replicated.
    pub(crate) async fn pending_message_records(
        &self,
        namespace: &str,
        receivers: &[String],
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        self.message_primitives()?.pending_to(namespace, receivers).await
    }

    /// The Messages with these names, local or replicated; one with no view is left out.
    pub(crate) async fn named_message_records(
        &self,
        namespace: &str,
        names: &[String],
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        let primitives = self.message_primitives()?;
        let mut records = Vec::new();
        for name in names {
            match primitives.view(namespace, name).await {
                Ok(Some(record)) => records.push(record),
                Ok(None) => {}
                Err(ResourceError::Invalid { message }) => tracing::debug!(%name, %message, "engine message has no old-shaped view"),
                Err(error) => return Err(error),
            }
        }
        Ok(records)
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
