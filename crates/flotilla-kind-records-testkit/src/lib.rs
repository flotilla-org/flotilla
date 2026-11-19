//! A kind's engine records in memory, for the tests of crates below the engine's real ports.
//!
//! [`memory_records`] is the one way those tests reach it. Each kind is served as its real port
//! serves it ([`Profile`]), and [`contract`] holds the in-memory port and every real one to the
//! same behaviour: the real ports run it in the crate that opens them.

pub mod contract;
mod profile;
mod records;
pub mod samples;

use std::sync::Arc;

use flotilla_resources::Resource;

pub use profile::{profile_of, Profile};
pub use records::MemoryRecords;

/// Kind `R`'s engine records at `home`, held in memory and served as its real port serves them.
/// Install it where the engine's port would be (`ResourceBackend::install_engine_kind`); another
/// home's record arrives with [`MemoryRecords::hold`].
pub fn memory_records<R: Resource>(home: &str) -> Arc<MemoryRecords<R>> {
    Arc::new(MemoryRecords::at(home))
}
