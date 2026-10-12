mod support;
pub use support::*;
mod clock;
pub use clock::VirtualClock;
pub mod fixtures;
mod read_counts;
pub use read_counts::{
    assert_flat_reads, measure_reads, ReadBudget, ReadCountsBackendExt, ReadOperation, ReadSnapshot, ReadTally, ScopedReads,
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
