// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

pub mod artifact;
pub mod blob_store;
mod charter_delegation;
pub mod cli;
mod dispatch_reconciler;
mod environment_orphans;
mod environment_tools;
mod event_relay;
mod fulfilment_probe;
mod image_build;
pub mod peer;
mod resource_limits;
pub mod resource_manifest;
mod restart_history;
pub mod runtime;
pub mod server;
mod sleep_inhibitor;
mod startup;
pub mod supervisor;

pub(crate) const DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH: &str = "run/socket-path";

mod image_distribution;

pub use flotilla_credentials::validate_frozen_vessel_skills;
