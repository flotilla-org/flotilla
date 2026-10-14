//! Provider-backed command execution composed through host-owned ports.
#![allow(clippy::double_must_use)]
pub mod action_events;
pub mod action_namespace;
pub mod action_support;
mod data;
pub mod environment_manager;
pub mod executor;
pub mod executor_actions;
mod provider_data;
pub mod step;

#[cfg(test)]
mod testkits {
    pub(crate) use flotilla_discovery_testkit as discovery;
    pub(crate) use flotilla_replay_testkit as replay;
}
