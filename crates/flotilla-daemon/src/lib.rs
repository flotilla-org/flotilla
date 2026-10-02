// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

mod agent_material;
mod aggregator;
pub mod artifact;
pub mod blob_store;
mod codex_central;
mod credential;
mod dispatch_reconciler;
mod environment_tools;
mod event_relay;
mod fulfilment_probe;
mod issue_materializer;
mod resource_limits;
pub mod resource_manifest;
mod restart_history;
mod sleep_inhibitor;
mod startup;
pub mod vessel_config;
pub use aggregator::{Aggregator, AggregatorResolvers};

pub mod cli;
pub mod peer;
pub mod runtime;
pub mod server;
pub mod supervisor;

pub(crate) const DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH: &str = "run/socket-path";
