// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod actuators;
mod checkout_reconciler;
mod clone_reconciler;
#[path = "../common/mod.rs"]
mod common;
mod convoy_reconcile;
mod environment_reconciler;
mod image_build;
mod in_process_daemon;
mod liveness_contract;
mod provisioning_in_memory;
mod repository_reconciler;
mod terminal_session_reconciler;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod vessel_finalization;
mod vessel_placement;
mod vessel_reconciler;

// Reuse the lower crate's backend contracts and fixtures without reversing its build graph.
