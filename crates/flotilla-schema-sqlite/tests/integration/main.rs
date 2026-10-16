// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod bench;
mod change_log;
mod checks;
mod ddl;
mod edges;
mod fixtures;
mod interning;
mod ownership;
mod plans;
mod queries;
mod registry;
mod renames;
mod representation;
mod round_trip;
mod states;
mod tables;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod versions;
