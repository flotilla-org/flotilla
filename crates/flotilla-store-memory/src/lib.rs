use flotilla_resources::*;
use flotilla_resources::{error, field_ownership, resource, retention};
use flotilla_store::*;
use flotilla_store::{replica, watch};
mod in_memory;
pub use in_memory::*;

/// A resource backend that is the engine's home (#3108): the engine holds every registered kind,
/// each refused until its port is installed, so the store under it holds nothing and is not
/// durable.
pub fn engine_home_backend() -> ResourceBackend {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.compose_as_engine_home();
    backend
}
