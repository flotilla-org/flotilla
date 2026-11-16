//! The engine port's reads, each counted for its caller by shape and the records it returned
//! ([`flotilla_schema::reads`]). A port may answer from what it holds, or read the store, whose
//! own reads count under the stored kind's name; so the port's count under the kind's name with
//! [`PORT`] after it, and the two are never summed.

use flotilla_resources::engine_records::{EngineObject, EngineRecords};
use flotilla_schema::reads::{self, ReadShape};

/// What follows a kind's name in the port's counts.
pub(crate) const PORT: &str = " port";

fn counted(kind: &str, shape: ReadShape, read: Result<Vec<EngineObject>, String>) -> Result<Vec<EngineObject>, String> {
    let kind = format!("{kind}{PORT}");
    reads::read(&kind, shape);
    if let Ok(records) = &read {
        reads::rows(&kind, records.len());
    }
    read
}

/// The record `name` of `kind`, as the port chooses it among the homes holding one.
pub(crate) async fn get(records: &dyn EngineRecords, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
    let record = records.get(kind, name).await;
    let kind = format!("{kind}{PORT}");
    reads::read(&kind, ReadShape::Keyed);
    if let Ok(Some(_)) = &record {
        reads::rows(&kind, 1);
    }
    record
}

/// One record of each name of `kind`.
pub(crate) async fn list(records: &dyn EngineRecords, kind: &str) -> Result<Vec<EngineObject>, String> {
    counted(kind, ReadShape::WholeKind, records.list(kind).await)
}

/// Every home's records of `kind`.
pub(crate) async fn sources(records: &dyn EngineRecords, kind: &str) -> Result<Vec<EngineObject>, String> {
    counted(kind, ReadShape::WholeKind, records.sources(kind).await)
}

/// Every home's record `name` of `kind`.
pub(crate) async fn copies(records: &dyn EngineRecords, kind: &str, name: &str) -> Result<Vec<EngineObject>, String> {
    counted(kind, ReadShape::Keyed, records.copies(kind, name).await)
}

/// The open records of `kind` addressed to `receiver`, where the port keeps them indexed.
pub(crate) async fn open_to(records: &dyn EngineRecords, kind: &str, receiver: &str) -> Option<Result<Vec<EngineObject>, String>> {
    let open = records.open_to(kind, receiver).await?;
    Some(counted(kind, ReadShape::Index, open))
}
