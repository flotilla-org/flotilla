// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
// Controllers' fixtures stay with the crew and provisioning tests that also use them.
#[path = "../../../flotilla-controllers/tests/common/mod.rs"]
mod common;
mod convoy_engine_path;
mod convoy_in_memory;
mod placement_tiebreak_capacity_wait;
mod review_bundle;
mod review_bundle_aggregator;
mod terminal_actuator;
mod terminal_session_reconciler;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod vessel_finalization;
mod vessel_reconciler;
