pub mod backend;
pub mod error;
pub mod http;
pub mod in_memory;
pub mod resource;
pub mod watch;

pub use backend::{ResourceBackend, TypedResolver};
pub use error::ResourceError;
pub use http::{ensure_crd, ensure_namespace, HttpBackend};
pub use in_memory::InMemoryBackend;
pub use resource::{ApiPaths, InputMeta, ObjectMeta, Resource, ResourceObject};
pub use watch::{ResourceList, WatchEvent, WatchStart};
