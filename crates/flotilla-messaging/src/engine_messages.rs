//! The engine's Message path as code above the engine reads and admits Messages (#3317).

use async_trait::async_trait;

/// Where a message named by another stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageResolution {
    /// Held in this home's replica or at its home.
    Found,
    /// Every home answered that it holds no such message.
    Absent,
    /// Unanswered: a home could not be reached, or the read failed.
    Unknown(String),
}

/// One message in a crew seat's inbox as the stall judge reads it (#3108): where it stands, and
/// nothing else of the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatMessage {
    /// The seat (an old crew address) whose inbox holds it, as it was asked for.
    pub seat: String,
    /// Its record name.
    pub name: String,
    pub phase: flotilla_resources::MessagePhase,
    pub reason: Option<String>,
    /// Its place in its inbox's admission order.
    pub accepted_sequence: u64,
    pub created: chrono::DateTime<chrono::Utc>,
    /// When it entered its phase.
    pub since: chrono::DateTime<chrono::Utc>,
    /// When it was delivered, once it was.
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A message read by its exact identity, where it stands with what it says: what a crew member
/// was sent (#3108).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub standing: SeatMessage,
    pub body: String,
    /// The record name of the message it supersedes, if it names one.
    pub supersedes: Option<String>,
}

/// Where a message admitted under a name stands, read by its exact identity (#3108).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedMessage {
    /// The home it is held at: its receiver's, which admitted it.
    pub home: String,
    /// The message that superseded it, if one did.
    pub superseded_by: Option<String>,
}

/// The `[primitives]` switch's Message path, as crew code reads and admits
/// Messages (#3317). Crew code never reaches the engine: it sees engine
/// messages in the old shape through this port, which the composition root
/// implements over the decided store. Absent, every read and admission is
/// the old store's, unchanged.
#[async_trait]
pub trait EngineMessages: Send + Sync {
    /// Whether `inbox` holds the message `record`, local or replicated, read by its exact identity
    /// without loading it.
    async fn holds(&self, inbox: &flotilla_address::Address, record: &str) -> Result<bool, flotilla_resources::ResourceError>;
    /// An old-shaped intent in the engine's terms, its parties and references resolved as
    /// admission resolved them.
    // Goes once every producer builds its intent in the engine's terms (#3108).
    async fn intent_of(
        &self,
        namespace: &str,
        old: &flotilla_resources::MessageSpec,
    ) -> Result<crate::Intent, flotilla_resources::ResourceError>;
    /// The inbox of fleet role `role` (`fleet:<fleet>/role:<role>`), read on the FleetDesignation
    /// by its name; refused for a role the fleet does not declare.
    async fn fleet_role(&self, namespace: &str, role: &str) -> Result<flotilla_address::Address, flotilla_resources::ResourceError>;
    /// The inbox of the role person `principal` holds in `project`, or under the fleet with no
    /// project: the one role the project's charter says they hold, else the person role named as
    /// they are, else the operator role. Read on the project's declaration by its name.
    async fn person(
        &self,
        namespace: &str,
        principal: &str,
        project: Option<&str>,
    ) -> Result<flotilla_address::Address, flotilla_resources::ResourceError>;
    /// Where the message `record` in `inbox` stands, local or replicated.
    async fn admitted(
        &self,
        inbox: &flotilla_address::Address,
        record: &str,
    ) -> Result<Option<AdmittedMessage>, flotilla_resources::ResourceError>;
    /// The delivered message in `intent`'s inbox that suppresses `intent`, if one does: one from
    /// the same sender about the same subject revision, awaiting a reply or an outcome
    /// (ADR 0055). Its record name.
    async fn open_superseded(&self, intent: &crate::Intent) -> Result<Option<String>, flotilla_resources::ResourceError>;
    /// The last message admitted to `intent`'s inbox from its sender that is still waiting for
    /// delivery, local or replicated.
    async fn undelivered_from(&self, intent: &crate::Intent) -> Result<Option<SentMessage>, flotilla_resources::ResourceError>;
    /// The last open message admitted to `inbox` from `sender` that expects a reply and refers to
    /// one of `about`, at any revision, local or replicated: its record name.
    async fn awaited_reply_from(
        &self,
        sender: &flotilla_address::Address,
        inbox: &flotilla_address::Address,
        about: &[flotilla_address::Address],
    ) -> Result<Option<String>, flotilla_resources::ResourceError>;
    /// The messages to `seats` (old crew addresses) still waiting for delivery that a supervisor
    /// sent and no fleet role did, local or replicated, in admission order.
    async fn queued_briefs(&self, namespace: &str, seats: &[String]) -> Result<Vec<SentMessage>, flotilla_resources::ResourceError>;
    /// What the stall judge reads of crew seats' inboxes, local or replicated: for each of
    /// `receivers` (old crew addresses), its open messages, the last one admitted among those
    /// settled and the one delivered last and not dropped since; then each `named` (receiver,
    /// record) message by its exact identity.
    async fn seat_standings(
        &self,
        namespace: &str,
        receivers: &[String],
        named: &[(String, String)],
    ) -> Result<Vec<SeatMessage>, flotilla_resources::ResourceError>;
    /// Each `named` (receiver, record) message by its exact identity, local or replicated, read
    /// as in the receiver (an old crew address) it was asked for; one not held is left out.
    async fn sent_messages(
        &self,
        namespace: &str,
        named: &[(String, String)],
    ) -> Result<Vec<SentMessage>, flotilla_resources::ResourceError>;
    /// Admits an intent at this home, its receiver's; a repeated name is the message already
    /// admitted, and a delivered message about the same subject may answer instead (ADR 0055).
    async fn admit(&self, name: &str, intent: &crate::Intent) -> Result<crate::Admitted, flotilla_resources::ResourceError>;
    /// Where the message `intent` replies to stands, read as its admission
    /// reads it: in this home's replica, or at its home on a miss. With no
    /// `in_reply_to` it is absent.
    async fn resolve_in_reply_to(&self, intent: &crate::Intent) -> MessageResolution;
    /// An operator withdraws a queued message to `receiver` (an old address) homed here, on the
    /// convoy's authority.
    async fn withdraw(&self, namespace: &str, receiver: &str, record: &str, convoy: &str) -> Result<(), String>;
    /// The home this path admits at: this daemon's.
    fn home(&self) -> String;
    /// Records the sender's half of a send expecting a reply or an outcome,
    /// once it is admitted at `home` (#3587 layer 7): an AwaitingReply record
    /// of `kind` at this home, the sender's, about `about` when given.
    async fn await_reply(
        &self,
        namespace: &str,
        name: &str,
        intent: &crate::Intent,
        home: &str,
        kind: &str,
        about: Option<&str>,
    ) -> Result<bool, String>;
    /// Why delivery waits, when the fleet's FleetDesignation is missing or invalid.
    async fn delivery_wait(&self, namespace: &str) -> Result<Option<String>, String>;
    /// Withdraws this home's open stall records about `seats` (crew role
    /// addresses) once the old stall status shows them not stalled (#3730
    /// review); a seat that shares `stalled`'s record subject keeps its
    /// records. Returns the records withdrawn.
    async fn end_stalls(&self, namespace: &str, seats: &[String], stalled: Option<&str>, reason: &str) -> Result<Vec<String>, String>;
    /// Until when messages to `receiver` (a crew role address), admitted at this home and not
    /// yet delivered, hold its delivery; None when none does (#3894).
    async fn delivery_held_until(&self, namespace: &str, receiver: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, String>;
    /// A tick for each change to the messages after this call, local or replicated, heard from
    /// the store's commits: nothing is listed to hear it.
    fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, flotilla_resources::ResourceError>;
    /// Where charter notifications are admitted on this path.
    fn charter_notices(self: std::sync::Arc<Self>, namespace: &str) -> Box<dyn crate::CharterNotices>;
}
