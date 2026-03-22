pub mod agents;
pub mod attachable;
pub mod config;
pub mod convert;
pub mod daemon;
pub mod data;
pub mod delta;
pub mod executor;
pub(crate) mod host_registry;
pub mod host_summary;
pub mod in_process;
pub mod issue_cache;
pub mod merge;
pub mod model;
pub mod provider_data;
pub mod providers;
pub mod refresh;
pub(crate) mod repo_state;
pub mod resolve;
pub mod step;
pub mod template;
pub mod terminal_manager;

// Re-export host types from protocol for convenience.
pub use flotilla_protocol::HostName;
