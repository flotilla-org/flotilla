//! Crew operations, crew actions and capability cards, the leaf engine, the
//! decision log, charter notifications, and the Vessel, TerminalSession and
//! Convoy reconcilers.

pub mod charter_notifications;
pub mod convoy;
pub mod crew_actions;
pub mod crew_capabilities;
pub mod crew_ops;
pub mod decision_log;
pub mod leaf_engine;
mod message_port;
pub mod ports;
pub mod reconcilers;
pub mod review_bundle;

/// Readings that serve the engine's checkouts with none held, as a daemon's readings always serve
/// them (#3108). One store serves every unit test: it is only read.
#[cfg(test)]
pub(crate) fn no_held_checkouts() -> std::sync::Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads> {
    static READS: std::sync::OnceLock<std::sync::Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(READS.get_or_init(|| flotilla_provisioning_testkit::CheckoutFixture::default().reads))
}
