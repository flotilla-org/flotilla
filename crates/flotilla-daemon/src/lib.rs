// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

mod agent_attention;
pub mod artifact;
pub mod blob_store;
mod charter_delegation;
pub mod cli;
mod dispatch_reconciler;
pub mod dummy_fleet;
pub mod engine_import;
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
mod stdlib_image_layers;
pub mod supervisor;

pub(crate) const DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH: &str = "run/socket-path";

pub use flotilla_credentials::{validate_frozen_vessel_skills, SkillTrees};

#[cfg(test)]
extern crate self as flotilla_daemon;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../flotilla-daemon-testkit/src/lib.rs"]
mod testkits;

/// Compose the default review-bundle byte adapter from host configuration.
pub fn review_bundle_store(
    config: &flotilla_resources::ReviewBundleStoreConfig,
    credential: &flotilla_resources::ReviewBundleWriteCredential,
) -> Result<flotilla_crew::review_bundle::ReviewBundleStore, flotilla_crew::review_bundle::ReviewBundleStoreError> {
    flotilla_review_bundle_object_store::connect_review_bundle(config, credential)
}
