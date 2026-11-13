//! Old-store kinds served by the engine behind `[primitives]` (#3824 layers 2b-ii and 3).
//!
//! [`EngineRecords`] is one engine port as the old store's typed paths see it: each record as an
//! old object's name, labels, annotations, spec and status, with the home that wrote it. The
//! store installs ports on its backend by the kinds they serve, and its resolvers, queries and
//! watches serve those kinds from them; each port maps the old shape onto its kind's records
//! (an origin read from a definition's stamps, a repository's ops data from its annotations).

use std::collections::BTreeMap;

use futures::stream::BoxStream;

/// One record as the old store's objects carry it: its spec and status as JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineObject {
    pub name: String,
    /// The home that wrote it, its only writer.
    pub home: String,
    /// The record's version at its home; it moves with each write.
    pub version: u64,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub spec: serde_json::Value,
    pub status: Option<serde_json::Value>,
}

/// An engine port serving old-store kinds, one kind at a time by its resource kind name. A name
/// several homes hold reads as this home's record, else the lexically first home's; writes are
/// this home's alone.
#[async_trait::async_trait]
pub trait EngineRecords: Send + Sync + std::fmt::Debug {
    /// The resource kinds this port serves.
    fn kinds(&self) -> &'static [&'static str];
    /// Whether `kind`'s records are read through this port; a kind it serves only to refuse
    /// holds none as resources, so lists in no namespace.
    fn reads(&self, _kind: &str) -> bool {
        true
    }
    /// This home: the writer of the records it holds.
    fn home(&self) -> String;
    /// `kind`'s record `name`, as several homes' resolve.
    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String>;
    /// Every name of `kind`, each resolved as [`EngineRecords::get`].
    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String>;
    /// Every home's record of every name of `kind`.
    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String>;
    /// Every home's record `name` of `kind`: those [`EngineRecords::sources`] lists, unless the
    /// port can read one name alone.
    async fn copies(&self, kind: &str, name: &str) -> Result<Vec<EngineObject>, String> {
        Ok(self.sources(kind).await?.into_iter().filter(|record| record.name == name).collect())
    }
    /// Every home's open record of `kind` addressed to `receiver`, through the port's own index;
    /// `None` when the port keeps none for `kind`, and readers filter [`EngineRecords::sources`].
    async fn open_to(&self, _kind: &str, _receiver: &str) -> Option<Result<Vec<EngineObject>, String>> {
        None
    }
    /// Writes this home's record `name` of `kind` from an old object's metadata and spec: created,
    /// or replaced, keeping its status. Stamps the kind does not keep are not kept.
    async fn apply(
        &self,
        kind: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: serde_json::Value,
    ) -> Result<EngineObject, String>;
    /// Replaces this home's record's status; refused for a kind with none.
    async fn set_status(&self, kind: &str, name: &str, status: serde_json::Value) -> Result<EngineObject, String>;
    /// Deletes this home's record `name` of `kind`; absent is done. Another home's copy still reads.
    async fn delete(&self, kind: &str, name: &str) -> Result<(), String>;
    /// A tick for each pass that found any home's records of these kinds changed since the pass
    /// before, from the moment it is called; watchers read the records again on each.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String>;
}

/// Forge observations served by the engine: each written only by its source's observer, at its
/// own home, when a value changes. A failed read is recorded once, so readers age the values from
/// the last time they were confirmed.
#[async_trait::async_trait]
pub trait ObservationFailures: Send + Sync + std::fmt::Debug {
    /// Marks this home's record `name` of `kind` as failing, unless it already is; absent is done.
    async fn failing(&self, kind: &str, name: &str) -> Result<(), String>;
}

/// Which forge sources this home observes: a source's observer is the single writer of its
/// change requests' and issues' records (#3108), so a write from any other home is refused.
#[async_trait::async_trait]
pub trait SourceObservers: Send + Sync + std::fmt::Debug {
    /// Whether this home observes `(service, scope)`, as the observers judge it; true when no
    /// home is known to observe it.
    async fn observes(&self, service: &str, scope: &str) -> Result<bool, String>;
}

/// [`SourceObservers`] installed once the daemon that judges them is composed, after the records
/// that consult them are opened. Before then every write is refused: no home is known to observe.
#[derive(Debug, Default)]
pub struct LateSourceObservers(std::sync::OnceLock<std::sync::Arc<dyn SourceObservers>>);

impl LateSourceObservers {
    pub fn install(&self, observers: std::sync::Arc<dyn SourceObservers>) -> Result<(), String> {
        self.0.set(observers).map_err(|_| "the source observers are already installed".to_owned())
    }
}

#[async_trait::async_trait]
impl SourceObservers for LateSourceObservers {
    async fn observes(&self, service: &str, scope: &str) -> Result<bool, String> {
        match self.0.get() {
            Some(observers) => observers.observes(service, scope).await,
            None => Err(format!("the observers of {service} {scope} are not known yet")),
        }
    }
}

/// A change request the forge observer found merging from a branch of its own repository
/// (#3108). A fork's branch of the same name is another branch: the observer records none for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChangeRequest {
    /// The record's name, the change request address's record name.
    pub name: String,
    /// The home whose observer found it.
    pub home: String,
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub branch: String,
    /// Its state when found; none when the forge reported none.
    pub state: Option<crate::ObservedChangeRequestState>,
}

/// The repository branches the forge observers have found change requests from (#3108): a
/// branch-observation request's answer is written here at the observer's home, and branch
/// validation reads every home's through the records' `by_branch` index.
#[async_trait::async_trait]
pub trait ChangeRequestBranches: Send + Sync + std::fmt::Debug {
    /// Records at this home that `found.number` merges from `found.branch` of its own repository,
    /// in `found.state`. Writes only what changed; the record's other observed values stay.
    async fn found_on_branch(&self, found: &BranchChangeRequest) -> Result<(), String>;
    /// Every home's change requests in `(service, scope)` found merging from `branch`.
    async fn on_branch(&self, service: &str, scope: &str, branch: &str) -> Result<Vec<BranchChangeRequest>, String>;
}
