//! Host collaborators required by crew operations.
use std::collections::BTreeMap;

use async_trait::async_trait;

/// Stages a Work's credentials into its environment before crew delivery.
#[async_trait]
pub trait WorkCredentialReconciler: Send + Sync {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String>;

    async fn ledger_delivery_environment(&self, _namespace: &str, _environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        Err("credential delivery record is unavailable".to_string())
    }
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
    /// Withdraws this home's open stall records about `seats` (crew role
    /// addresses) once the old stall status shows them not stalled (#3730
    /// review); a seat that shares `stalled`'s record subject keeps its
    /// records. Returns the records withdrawn.
    async fn end_stalls(&self, namespace: &str, seats: &[String], stalled: Option<&str>, reason: &str) -> Result<Vec<String>, String>;
    /// Whether a message to `receiver` (a crew role address) is admitted
    /// at this home and not yet delivered (#3894).
    async fn awaits_delivery(&self, namespace: &str, receiver: &str) -> Result<bool, String>;
    /// Where charter notifications are admitted on this path.
    fn charter_notices(self: std::sync::Arc<Self>, namespace: &str) -> Box<dyn flotilla_messaging::CharterNotices>;
}

/// The composition root's slot for [`EngineMessages`]: set once, at startup,
/// when `[primitives]` is on.
pub type MessagePath = std::sync::Arc<std::sync::OnceLock<std::sync::Arc<dyn EngineMessages>>>;

/// Fresh inspection requested by an explicit convoy deletion before its stored-evidence gate.
#[async_trait]
pub trait CheckoutIntegrationInspector: Send + Sync {
    async fn inspect(
        &self,
        convoy: &flotilla_resources::ResourceObject<flotilla_resources::Convoy>,
        checkout: &flotilla_resources::ResourceObject<flotilla_resources::Checkout>,
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String>;
}
