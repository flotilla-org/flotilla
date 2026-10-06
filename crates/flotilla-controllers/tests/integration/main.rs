// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod actuators;
mod checkout_reconciler;
mod clone_reconciler;
#[path = "../common/mod.rs"]
mod common;
mod environment_reconciler;
mod image_build;
mod liveness_contract;
mod presentation_reconciler;
mod provisioning_in_memory;
mod repository_reconciler;
mod terminal_session_reconciler;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod vessel_placement;
mod vessel_reconciler;
