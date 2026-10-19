// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

mod charter_notifications;
pub mod checkout_integration;
pub mod cleat_roll;
pub mod command_target;
pub mod convert;
pub mod crew_capabilities;
pub mod decision_log;
pub mod demand_lifecycle;
pub mod host_identity;
pub mod in_process;
pub mod leaf_engine;
pub mod model;
pub(crate) mod repo_state;
pub mod repository_inspection;
pub mod resolve;

// Re-export shared infrastructure and host types for convenience.
pub use flotilla_protocol::HostName;

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
