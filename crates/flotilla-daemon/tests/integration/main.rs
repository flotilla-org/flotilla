// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod aggregator;
mod convergence_property;
mod convoy_create_cli;
mod crew_skills;
mod dummy_fleet;
mod dummy_fleet_writes;
mod engine_import;
mod engine_only;
mod overlay_replication;
mod peer_connect_flow;
mod project_cli;
mod reconciled_remote_turn;
mod terminal_message_history;
mod test_daemon;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;

/// The engine's environments `daemon` reads (#3108), on its decided store, stepped as the
/// Environment controller would step them.
fn engine_environments(daemon: &flotilla_core::in_process::InProcessDaemon) -> flotilla_provisioning_testkit::EnvironmentFixture {
    let store = daemon.decided_store().expect("decided store").store();
    let home = flotilla_engine_store::StoreLog::new(std::sync::Arc::clone(&store)).home().to_owned();
    let fixture = flotilla_provisioning_testkit::EnvironmentFixture::over(&home, store);
    daemon.install_lifecycle_reads(std::sync::Arc::clone(&fixture.reads)).expect("install the engine's environments");
    fixture
}
