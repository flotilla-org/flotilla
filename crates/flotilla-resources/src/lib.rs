mod backend;
mod error;
mod http;
mod in_memory;
mod resource;
mod watch;

pub use backend::{ResourceBackend, TypedResolver};
pub use error::ResourceError;
pub use http::{ensure_crd, ensure_namespace, HttpBackend};
pub use in_memory::InMemoryBackend;
pub use resource::{ApiPaths, InputMeta, ObjectMeta, Resource, ResourceObject};
pub use watch::{ResourceList, WatchEvent, WatchStart};
