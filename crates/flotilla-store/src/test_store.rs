//! Shared adapter sources retain the unit-test library's Store trait identity.
use crate::{in_memory::InMemoryBackend, ResourceBackend};
pub fn memory_backend() -> ResourceBackend {
    ResourceBackend::new(InMemoryBackend::default())
}
