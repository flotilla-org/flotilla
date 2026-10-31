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
    /// This home: the writer of the records it holds.
    fn home(&self) -> String;
    /// `kind`'s record `name`, as several homes' resolve.
    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String>;
    /// Every name of `kind`, each resolved as [`EngineRecords::get`].
    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String>;
    /// Every home's record of every name of `kind`.
    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String>;
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
