//! The Message records port (#3317). Convoy and crew code reads and admits
//! Messages only through these methods, from the engine store's replica in
//! the old shape.

use flotilla_address::Address;
use flotilla_messaging::{Admitted, Intent, SentMessage};
use flotilla_resources::{MessageSpec, ResourceError};

use crate::crew_ops::{CrewService, Party};

impl CrewService {
    pub(crate) fn message_primitives(&self) -> Result<std::sync::Arc<dyn flotilla_messaging::EngineMessages>, ResourceError> {
        crate::ports::installed_messages(&self.message_path)
    }

    /// An old-shaped intent in the engine's terms, as admission takes it.
    // Goes once every producer builds its intent in the engine's terms (#3108).
    pub(crate) async fn intent_of(&self, namespace: &str, old: &MessageSpec) -> Result<Intent, ResourceError> {
        self.message_primitives()?.intent_of(namespace, old).await
    }

    /// Where the Message `intent` replies to stands, read as its admission reads it.
    pub(crate) async fn resolve_in_reply_to(&self, intent: &Intent) -> flotilla_messaging::MessageResolution {
        match self.message_primitives() {
            Ok(primitives) => primitives.resolve_in_reply_to(intent).await,
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

    /// Where the Message `name` in `intent`'s inbox stands, local or replicated; `None` when none
    /// is admitted.
    pub(crate) async fn admitted_message(
        &self,
        intent: &Intent,
        name: &str,
    ) -> Result<Option<flotilla_messaging::AdmittedMessage>, ResourceError> {
        self.message_primitives()?.admitted(&intent.inbox, name).await
    }

    /// The delivered Message in `intent`'s inbox that suppresses `intent`, if one does: its name.
    pub(crate) async fn open_superseded(&self, intent: &Intent) -> Result<Option<String>, ResourceError> {
        self.message_primitives()?.open_superseded(intent).await
    }

    /// The last Message from `intent`'s sender to its inbox still waiting for delivery, local or
    /// replicated.
    pub(crate) async fn undelivered_from(&self, intent: &Intent) -> Result<Option<SentMessage>, ResourceError> {
        self.message_primitives()?.undelivered_from(intent).await
    }

    /// The last open Message from `sender` to `inbox` expecting a reply about one of `about`, at
    /// any revision, local or replicated: its name.
    pub(crate) async fn awaited_reply_from(
        &self,
        sender: &Address,
        inbox: &Address,
        about: &[Address],
    ) -> Result<Option<String>, ResourceError> {
        self.message_primitives()?.awaited_reply_from(sender, inbox, about).await
    }

    /// The inbox `party` sends and receives at; a person's is the role they hold in `project`.
    pub(crate) async fn party_inbox(&self, namespace: &str, party: &Party, project: &str) -> Result<Address, String> {
        match party {
            Party::Person(principal) => {
                self.message_primitives().map_err(|error| error.to_string())?.person(namespace, principal, Some(project)).await
            }
            Party::Seat { project, convoy, role } => Address::project(project)
                .map_err(|error| error.to_string())
                .and_then(|project| flotilla_resources::convoy::engine::seat_address(&project, convoy, role))
                .map_err(ResourceError::invalid),
        }
        .map_err(|error| error.to_string())
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
    pub(crate) async fn admit_message(&self, name: &str, intent: &Intent) -> Result<Admitted, ResourceError> {
        self.message_primitives()?.admit(name, intent).await
    }

    /// Records the sender's half of a send expecting a reply or an outcome once it is admitted at
    /// `home`: an AwaitingReply record of `kind`, about `about` when given.
    pub(crate) async fn await_reply(
        &self,
        namespace: &str,
        name: &str,
        intent: &Intent,
        home: &str,
        kind: &str,
        about: Option<&str>,
    ) -> Result<bool, String> {
        let engine = self.message_primitives().map_err(|error| error.to_string())?;
        engine.await_reply(namespace, name, intent, home, kind, about).await
    }
}
