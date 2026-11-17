//! Resource storage, typed resolvers, replica/watch machinery and controller runtime.
// async_trait's generated future annotation is redundant on Rust 1.99.
#![allow(clippy::double_must_use)]

use flotilla_resources::*;
use flotilla_resources::{error, host, labels, resource, retention};

mod attentions;
mod backend;
pub use backend::*;
pub mod controller;
mod convoy;
pub mod load_metrics;
pub mod occupancy;
pub use convoy::*;
mod decoded;
pub use decoded::*;
mod definition;
mod engine_feed;
mod engine_port;
mod port_reads;
pub use definition::*;
pub use engine_port::EnginePort;
mod convoy_views;
mod held_listing;
mod lifecycle_views;
mod listed;
pub use listed::Listed;
mod terminal_views;
mod vessel_views;
pub use convoy_views::ConvoyViews;
pub use lifecycle_views::LifecycleViews;
pub use terminal_views::{EngineVesselSeats, TerminalViews};
pub use vessel_views::VesselViews;
pub mod message_query;
pub use message_query::*;
mod off_frame;
pub use off_frame::off_frame;
mod placement_image;
pub use placement_image::*;
mod prepared_snapshot;
pub use prepared_snapshot::*;
mod project;
pub use project::*;
mod project_hierarchy;
pub use project_hierarchy::*;
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
mod object_index;
pub use object_index::object_index_keys;
mod store;
pub use store::*;
mod uncomposed_records;

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
