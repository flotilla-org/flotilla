// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
#[path = "../common/mod.rs"]
mod common;
mod convoy_engine_path;
mod convoy_reconcile;
mod environment_vessels;
mod liveness_contract;
mod message_reads;
mod placement_tiebreak_capacity_wait;
mod read_costs;
mod review_bundle;
mod review_bundle_aggregator;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
// vessel_finalization deleted: a vessel is now a read view of its launch, with no finalizer to run (#3108).
mod vessel_reconciler;
mod vessel_views;
