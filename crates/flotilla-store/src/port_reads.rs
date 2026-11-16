//! The engine port's reads, each counted for its caller by shape and the records it returned
//! ([`flotilla_schema::reads`]). A port may answer from what it holds, or read the store, whose
//! own reads count under the stored kind's name; so the port's count under the kind's name with
//! [`PORT`] after it, and the two are never summed.

use flotilla_resources::engine_records::{KindRecords, LogPosition, Record};
use flotilla_schema::reads::{self, ReadShape};

use crate::Resource;

/// What follows a kind's name in the port's counts.
pub(crate) const PORT: &str = " port";

fn port<R: Resource>() -> String {
    format!("{}{PORT}", R::API_PATHS.kind)
}

fn counted<R: Resource>(shape: ReadShape, read: Result<Vec<Record<R>>, String>) -> Result<Vec<Record<R>>, String> {
    let kind = port::<R>();
    reads::read(&kind, shape);
    if let Ok(records) = &read {
        reads::rows(&kind, records.len());
    }
    read
}

/// The record `name`, as the port chooses it among the homes holding one.
pub(crate) async fn get<R: Resource>(records: &dyn KindRecords<R>, name: &str) -> Result<Option<Record<R>>, String> {
    let record = records.get(name).await;
    let kind = port::<R>();
    reads::read(&kind, ReadShape::Keyed);
    if let Ok(Some(_)) = &record {
        reads::rows(&kind, 1);
    }
    record
}

/// One record of each name.
pub(crate) async fn list<R: Resource>(records: &dyn KindRecords<R>) -> Result<Vec<Record<R>>, String> {
    counted(ReadShape::WholeKind, records.list().await)
}

/// Every home's records.
pub(crate) async fn sources<R: Resource>(records: &dyn KindRecords<R>) -> Result<Vec<Record<R>>, String> {
    counted(ReadShape::WholeKind, records.sources().await)
}

/// Every home's records, and the position they were read at.
pub(crate) async fn positioned<R: Resource>(records: &dyn KindRecords<R>) -> Result<(Vec<Record<R>>, LogPosition), String> {
    let read = records.positioned().await;
    let kind = port::<R>();
    reads::read(&kind, ReadShape::WholeKind);
    if let Ok((listed, _)) = &read {
        reads::rows(&kind, listed.len());
    }
    read
}

/// Every home's record `name`.
pub(crate) async fn copies<R: Resource>(records: &dyn KindRecords<R>, name: &str) -> Result<Vec<Record<R>>, String> {
    counted(ReadShape::Keyed, records.copies(name).await)
}

/// Every home's record `name` that the listing holds.
pub(crate) async fn sources_named<R: Resource>(records: &dyn KindRecords<R>, name: &str) -> Result<Vec<Record<R>>, String> {
    counted(ReadShape::Keyed, records.sources_named(name).await)
}

/// The open records addressed to `receiver`, where the port keeps them indexed.
pub(crate) async fn open_to<R: Resource>(records: &dyn KindRecords<R>, receiver: &str) -> Option<Result<Vec<Record<R>>, String>> {
    let open = records.open_to(receiver).await?;
    Some(counted(ReadShape::Index, open))
}
