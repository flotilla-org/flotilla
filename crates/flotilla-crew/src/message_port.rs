//! The Message records port (#3317). Convoy and crew code reads and admits
//! Messages only through these methods, from the engine store's replica in
//! the old shape.

use flotilla_messaging::{MessageAdmission, SentMessage};
use flotilla_resources::{InputMeta, MessageSpec, ResourceError};

use crate::crew_ops::CrewService;

impl CrewService {
    pub(crate) fn message_primitives(&self) -> Result<std::sync::Arc<dyn flotilla_messaging::EngineMessages>, ResourceError> {
        crate::ports::installed_messages(&self.message_path)
    }

    /// Where the Message `intent` replies to stands, read as its admission reads it.
    pub(crate) async fn resolve_in_reply_to(&self, namespace: &str, intent: &MessageSpec) -> flotilla_messaging::MessageResolution {
        match self.message_primitives() {
            Ok(primitives) => primitives.resolve_in_reply_to(namespace, intent).await,
            Err(error) => flotilla_messaging::MessageResolution::Unknown(error.to_string()),
        }
    }

    /// The Message `name` in `receiver`'s inbox, local or replicated: where it stands, with its body.
    pub(crate) async fn message_to(&self, namespace: &str, receiver: &str, name: &str) -> Result<SentMessage, ResourceError> {
        let named = [(receiver.to_owned(), name.to_owned())];
        self.message_primitives()?.sent_messages(namespace, &named).await?.pop().ok_or_else(|| ResourceError::not_found(name))
    }

    /// Whether `receiver`'s inbox holds the Message `name`, local or replicated.
    pub(crate) async fn holds_message(&self, namespace: &str, receiver: &str, name: &str) -> Result<bool, ResourceError> {
        self.message_primitives()?.holds(namespace, receiver, name).await
    }

    /// Where the Message `name` in the inbox `intent` is admitted to stands, local or replicated;
    /// `None` when none is admitted.
    pub(crate) async fn admitted_message(
        &self,
        namespace: &str,
        intent: &MessageSpec,
        name: &str,
    ) -> Result<Option<flotilla_messaging::AdmittedMessage>, ResourceError> {
        self.message_primitives()?.admitted(namespace, intent, name).await
    }

    /// The delivered Message in the inbox `intent` is admitted to that suppresses `intent`, if
    /// one does: its name.
    pub(crate) async fn open_superseded(&self, namespace: &str, intent: &MessageSpec) -> Result<Option<String>, ResourceError> {
        self.message_primitives()?.open_superseded(namespace, intent).await
    }

    /// The last Message from `intent`'s sender to its receiver still waiting for delivery, local or
    /// replicated.
    pub(crate) async fn undelivered_from(&self, namespace: &str, intent: &MessageSpec) -> Result<Option<SentMessage>, ResourceError> {
        self.message_primitives()?.undelivered_from(namespace, intent).await
    }

    /// The last open Message from `intent`'s sender to its receiver expecting a reply about one of
    /// what `intent` refers to, local or replicated: its name.
    pub(crate) async fn awaited_reply_from(&self, namespace: &str, intent: &MessageSpec) -> Result<Option<String>, ResourceError> {
        self.message_primitives()?.awaited_reply_from(namespace, intent).await
    }

    /// The Messages to `seats` still waiting for delivery that a supervisor sent and no fleet role
    /// did, local or replicated, in admission order.
    pub(crate) async fn queued_briefs(&self, namespace: &str, seats: &[String]) -> Result<Vec<SentMessage>, ResourceError> {
        self.message_primitives()?.queued_briefs(namespace, seats).await
    }

    /// The Messages with these names, each a (receiver, name) pair read in its receiver's inbox,
    /// local or replicated, where each stands with its body; one not held is left out.
    pub(crate) async fn sent_messages(
        &self,
        namespace: &str,
        named: &[(String, String)],
    ) -> Result<Vec<flotilla_messaging::SentMessage>, ResourceError> {
        self.message_primitives()?.sent_messages(namespace, named).await
    }

    /// Admits a Message at this home, the receiver's. A message's name is
    /// its producer key's digest, so a repeated name is the message already
    /// admitted; admission answers with another message when a delivered one
    /// about the same subject suppresses it (ADR 0055).
    pub(crate) async fn admit_message(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &MessageSpec,
    ) -> Result<MessageAdmission, ResourceError> {
        let admitted = self.message_primitives()?.admit(namespace, &meta.name, spec).await?;
        Ok(if admitted.metadata.name == meta.name {
            MessageAdmission::Accepted(admitted)
        } else {
            MessageAdmission::Suppressed { predecessor: admitted }
        })
    }
}
