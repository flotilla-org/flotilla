//! Resource storage, typed resolvers, replica/watch machinery and controller runtime.
// async_trait's generated future annotation is redundant on Rust 1.99.
#![allow(clippy::double_must_use)]

use flotilla_resources::*;
use flotilla_resources::{digest, error, field_ownership, host, labels, message, resource, retention};

mod artifact;
pub use artifact::*;
mod backend;
pub use backend::*;
pub mod controller;
mod convoy;
pub use convoy::*;
mod crew_image_baseline;
pub use crew_image_baseline::*;
mod definition;
pub use definition::*;
mod event;
pub use event::*;
mod http;
pub use http::*;
mod image_build;
pub use image_build::*;
mod in_memory;
pub use in_memory::*;
mod message_conditions;
mod message_delivery;
pub use message_delivery::*;
mod message_inbox;
pub use message_inbox::*;
mod message_query;
pub use message_query::*;
mod message_retention;
mod owner_gc;
pub use owner_gc::*;
mod placement_policy;
pub use placement_policy::*;
mod prepared_snapshot;
pub use prepared_snapshot::*;
mod principal_attention;
pub use principal_attention::*;
mod project;
pub use project::*;
mod project_hierarchy;
pub use project_hierarchy::*;
mod registry;
pub use registry::*;
mod replica;
mod repository;
pub use repository::*;
mod review_bundle;
pub use review_bundle::*;
mod role_cascade;
pub use role_cascade::*;
mod role_routing;
pub use role_routing::*;
mod sqlite;
pub use sqlite::*;
mod status_patch;
pub use status_patch::*;
mod watch;

#[cfg(test)]
mod change_request_tests;
#[cfg(test)]
mod crew_defaults_tests;
#[cfg(test)]
mod digest_tests;
#[cfg(test)]
mod registry_watch_tests;
#[cfg(test)]
mod role_cascade_tests;
#[cfg(test)]
mod usage_tests;
