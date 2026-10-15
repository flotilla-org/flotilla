// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

pub mod admission;
pub mod change_request_observer;
mod charter_notifications;
pub mod checkout_integration;
pub mod cleat_roll;
pub mod command_target;
pub mod convert;
pub mod convoy_ensure;
pub mod crew_capabilities;
pub mod decision_log;
pub mod demand_lifecycle;
pub mod dispatch_missions;
pub mod host_identity;
pub mod image_build;
pub mod in_process;
pub mod issue_observer;
pub mod leaf_engine;
pub mod model;
pub(crate) mod observed_resources;
pub mod placement_policy;
pub mod regard_lifecycle;
pub(crate) mod repo_state;
pub(crate) mod repository_addressing;
pub mod repository_inspection;
pub mod resolve;

// Re-export shared infrastructure and host types for convenience.
pub use flotilla_protocol::HostName;

pub mod forge_observation;

// Unit tests compile this library under cfg(test), which gives its traits a
// different Rust identity from the normal library used by external testkits.
// Compile the shared adapters against that identity; integration tests use the
// testkit crates directly. No helper is included in the production library.
#[cfg(test)]
extern crate self as flotilla_core;
#[cfg(test)]
mod testkits;

#[cfg(test)]
mod probe_tests;
