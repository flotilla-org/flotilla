//! Resource storage, typed resolvers, replica/watch machinery and controller runtime.
// async_trait's generated future annotation is redundant on Rust 1.99.
#![allow(clippy::double_must_use)]

use flotilla_resources::*;
use flotilla_resources::{error, field_ownership, host, labels, resource, retention};

mod account_records;
pub use account_records::{OldStoreAccounts, SwitchedAccounts};
mod artifact;
pub use artifact::*;
mod backend;
pub use backend::*;
pub mod controller;
mod convoy;
pub mod load_metrics;
pub use convoy::*;
mod crew_image_baseline;
pub use crew_image_baseline::*;
mod decoded;
pub use decoded::*;
mod definition;
mod manifest_roots;
pub use definition::*;
pub use manifest_roots::{OldStoreManifestRoots, SwitchedManifestRoots};
mod event;
pub use event::*;
mod terminal_lifecycle;
pub use terminal_lifecycle::*;
mod image_build;
pub use image_build::*;
pub mod message_query;
pub use message_query::*;
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
mod read_retry;
pub use read_retry::{retry_store_read, retry_store_read_for};
mod registry;
pub use registry::*;
pub mod replica;
mod repository;
pub use repository::*;
mod role_cascade;
pub use role_cascade::*;
mod status_patch;
pub use status_patch::*;
mod view;
pub mod watch;
pub use view::*;
mod view_snapshot;
pub use view_snapshot::*;

pub mod query;
pub use query::*;
mod store;
pub use store::*;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../flotilla-store-memory/src/in_memory.rs"]
mod in_memory;
#[cfg(test)]
mod test_store;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../flotilla-store-sqlite/src/sqlite.rs"]
mod sqlite;
#[cfg(test)]
use flotilla_resources::digest;

mod serialized_watch;
pub use serialized_watch::*;

mod projection_helpers;

pub use projection_helpers::{read_definition_catalog, RequestSnapshot};
