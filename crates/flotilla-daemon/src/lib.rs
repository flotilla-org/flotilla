mod agent_material;
mod aggregator;
mod credential;
mod dispatch_reconciler;
mod environment_tools;
mod issue_materializer;
mod material_pool;
mod resource_limits;
pub mod resource_manifest;
mod restart_history;
mod sleep_inhibitor;
pub mod vessel_config;
pub use aggregator::{Aggregator, AggregatorResolvers};

pub mod cli;
pub mod peer;
pub mod runtime;
pub mod server;
pub mod supervisor;

pub(crate) const DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH: &str = "run/socket-path";
