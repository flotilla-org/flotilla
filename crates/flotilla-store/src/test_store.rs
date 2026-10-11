//! Shared adapter sources retain the unit-test library's Store trait identity.
use crate::{in_memory::InMemoryBackend, ResourceBackend};
pub fn memory_backend() -> ResourceBackend {
    ResourceBackend::new(InMemoryBackend::default())
}
pub fn sqlite_backend() -> Result<ResourceBackend, crate::ResourceError> {
    Ok(ResourceBackend::new(crate::sqlite::SqliteBackend::open_in_memory()?))
}
