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

/// The `[primitives]` switch's Message path, as crew code reads and admits
/// Messages (#3317). Crew code never reaches the engine: it sees engine
/// messages in the old shape through this port, which the composition root
/// implements over the decided store. Absent, every read and admission is
/// the old store's, unchanged.
#[async_trait]
pub trait EngineMessages: Send + Sync {
    /// One message by its old record name, local or replicated.
    async fn view(
        &self,
        namespace: &str,
        record: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError>;
    /// Every message in this home's replica.
    async fn views(
        &self,
        namespace: &str,
    ) -> Result<Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError>;
    /// Admits an intent at this home, its receiver's; a repeated name is the message already admitted.
    async fn admit(
        &self,
        namespace: &str,
        name: &str,
        intent: &flotilla_resources::MessageSpec,
    ) -> Result<flotilla_resources::ResourceObject<flotilla_resources::Message>, flotilla_resources::ResourceError>;
    /// An intent with its sender and receiver as this path's views spell
    /// them, for comparing parties with messages read back: a person reads
    /// back as the role they send in (the principal goes to the trace), a
    /// convoy role as the vessel that holds it.
    async fn spelled(
        &self,
        namespace: &str,
        intent: &flotilla_resources::MessageSpec,
    ) -> Result<flotilla_resources::MessageSpec, flotilla_resources::ResourceError>;
    /// Where the message `intent` replies to stands, read as its admission
    /// reads it: in this home's replica, or at its home on a miss. With no
    /// `in_reply_to` it is absent.
    async fn resolve_in_reply_to(&self, namespace: &str, intent: &flotilla_resources::MessageSpec) -> MessageResolution;
    /// An operator withdraws a queued message homed here, on the convoy's authority.
    async fn withdraw(&self, namespace: &str, record: &str, convoy: &str) -> Result<(), String>;
    /// The home this path admits at: this daemon's.
    fn home(&self) -> String;
    /// Records the sender's half of a send expecting a reply or an outcome,
    /// once it is admitted at `home` (#3587 layer 7): an AwaitingReply record
    /// of `kind` at this home, the sender's, about `about` when given.
    async fn await_reply(
        &self,
        namespace: &str,
        name: &str,
        intent: &flotilla_resources::MessageSpec,
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
    /// Where charter notifications are admitted on this path.
    fn charter_notices(self: std::sync::Arc<Self>, namespace: &str) -> Box<dyn crate::CharterNotices>;
}
