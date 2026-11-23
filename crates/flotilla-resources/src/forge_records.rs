//! Forges as the engine holds them: the port every Forge reader uses, served over the decided
//! store.
//!
//! A reader asks for the forge a record names, by its id or by its service URL, and is never
//! handed the declared set to search. There is no listing.

use futures::stream::BoxStream;

use crate::ForgeSpec;

/// The Forge port. Each read is keyed; a name several homes declare reads as this home's
/// declaration, else the lexically first home's.
#[async_trait::async_trait]
pub trait ForgeRecords: Send + Sync {
    /// The forge declared as `name` (its `forge_id`).
    async fn forge(&self, name: &str) -> Result<Option<ForgeSpec>, String>;
    /// The forge whose service URL (its `https_url`, without a trailing slash) is `service_url`.
    async fn at_service_url(&self, service_url: &str) -> Result<Option<ForgeSpec>, String>;
    /// Each forge declared, changed or withdrawn from now on, as it last read: a withdrawn forge
    /// carries the declaration it had. A read that fails is an item, and the follower is told.
    async fn changes(&self) -> Result<BoxStream<'_, Result<ForgeSpec, String>>, String>;
}
