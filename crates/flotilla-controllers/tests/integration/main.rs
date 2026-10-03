// Keep integration coverage in one executable to avoid linking the crate graph per module.
mod actuators;
mod checkout_reconciler;
mod clone_reconciler;
#[path = "../common/mod.rs"]
mod common;
mod environment_reconciler;
mod liveness_contract;
mod presentation_reconciler;
mod provisioning_in_memory;
mod repository_reconciler;
mod terminal_session_reconciler;
mod vessel_placement;
mod vessel_reconciler;
