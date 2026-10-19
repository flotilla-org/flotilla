// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
#[path = "../common/mod.rs"]
mod common;
mod convoy_reconcile;
mod in_process_daemon;
mod liveness_contract;
mod provisioning_in_memory;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;

// Reuse the lower crate's backend contracts and fixtures without reversing its build graph.
