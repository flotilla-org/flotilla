// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

pub mod admission;
pub mod agent_adapter;
pub mod agent_process;
pub mod agents;
pub mod aggregator_projection;
pub mod attachable;
pub mod awareness_projection;
mod branch_lookup_observer;
pub mod change_request_observer;
mod charter_notifications;
pub mod charter_store;
pub mod checkout_integration;
pub mod cleat_roll;
pub mod command_target;
pub mod config;
pub mod convert;
pub mod convoy_branch_refresh;
pub mod convoy_ensure;
pub mod crew_capabilities;
pub mod data;
pub mod decision_log;
pub mod demand_lifecycle;
pub mod dispatch_missions;
mod dispatch_ready;
pub mod environment_manager;
pub mod event_sink;
pub mod executor;
pub(crate) mod fleet;
pub mod fleet_health;
pub mod hop_chain;
pub mod host_identity;
pub(crate) mod host_registry;
pub mod host_resolution;
pub mod host_summary;
pub mod image_build;
pub mod in_process;
pub mod issue_observer;
pub mod leaf_engine;
pub mod log_file;
pub mod model;
pub(crate) mod observed_resources;
pub mod ops_entry;
pub mod placement_policy;
pub mod probe;
pub mod project_declaration;
mod project_repositories;
pub mod provider_data;
pub mod providers;
pub mod query_registry;
pub mod regard_lifecycle;
pub(crate) mod repo_state;
pub(crate) mod repository_addressing;
pub mod repository_inspection;
pub mod resolve;
mod resource_explain;
pub mod salience;
mod scoped_store;
mod standing_roles;
pub mod step;
pub mod terminal_health;
pub mod vcs;
mod verdict_queue;

// Re-export shared infrastructure and host types for convenience.
pub use flotilla_protocol::HostName;

pub mod forge_observation;

pub mod forge_budget;

// Unit tests compile this library under cfg(test), which gives its traits a
// different Rust identity from the normal library used by external testkits.
// Compile the shared adapters against that identity; integration tests use the
// testkit crates directly. No helper is included in the production library.
#[cfg(test)]
extern crate self as flotilla_core;
pub mod discovery_api;
pub mod provider_config;
#[cfg(test)]
mod testkits;
