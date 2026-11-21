// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod aggregator;
mod convoy_create_cli;
mod crew_skills;
mod dummy_fleet;
mod dummy_fleet_writes;
mod engine_only;
mod operator_commands;
mod overlay_replication;
mod peer_connect_flow;
mod planner_hosts;
mod project_cli;
mod reconciled_remote_turn;
mod test_daemon;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;

use flotilla_daemon_testkit::engine::{admit_convoy, engine, engine_environments};
