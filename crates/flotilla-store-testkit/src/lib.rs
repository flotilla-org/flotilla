mod support;
pub use support::*;
mod clock;
pub use clock::VirtualClock;
pub mod fixtures;
mod read_counts;
pub use read_counts::{
    assert_flat_reads, measure_reads, ReadBudget, ReadCountsBackendExt, ReadOperation, ReadSnapshot, ReadTally, ScopedReads, ZeroReadGuard,
};
mod message;
pub use message::LegacyMessageFixture;

/// Fresh logical store for API-crate tests without an upward adapter dependency.
pub fn memory_backend() -> flotilla_store::ResourceBackend {
    flotilla_store::ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default())
}
pub fn sqlite_backend() -> Result<flotilla_store::ResourceBackend, flotilla_resources::ResourceError> {
    Ok(flotilla_store::ResourceBackend::new(flotilla_store_sqlite::SqliteBackend::open_in_memory()?))
}

/// The record name the observer writes for a stored `(service, scope, number)`
/// change request spelling that needs no Forge directory.
pub fn cr_name(service: &str, scope: &str, number: u64) -> String {
    flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_change_request_address(service, scope, number, &[]).expect("change request address"),
    )
    .expect("record name")
}
