mod support;
pub use support::*;
mod clock;
pub use clock::{PausedClock, VirtualClock};
pub mod fixtures;
mod read_counts;
pub use read_counts::{
    assert_flat_reads, measure_reads, ReadBudget, ReadCountsBackendExt, ReadOperation, ReadSnapshot, ReadTally, ScopedReads, ZeroReadGuard,
};
mod attentions;
pub use attentions::NoAttentions;
mod message;
pub use message::LegacyMessageFixture;
mod old_store_accounts;
pub use old_store_accounts::OldStoreAccounts;
mod old_store_manifest_roots;
pub use old_store_manifest_roots::OldStoreManifestRoots;

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

/// Inject a synchronization-boundary read failure while retaining the real store and reader.
pub fn poison_mutex<T: Send + 'static>(mutex: std::sync::Arc<std::sync::Mutex<T>>) {
    assert!(std::thread::spawn(move || {
        let _guard = mutex.lock().expect("healthy test mutex");
        panic!("injected mutex poisoning");
    })
    .join()
    .is_err());
}
