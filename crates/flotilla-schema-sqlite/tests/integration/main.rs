// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod bench;
mod checks;
mod ddl;
mod domains;
mod edges;
mod fixtures;
mod history;
mod homes;
mod interning;
mod log;
mod ownership;
mod plans;
mod queries;
mod registry;
mod renames;
mod representation;
mod round_trip;
mod states;
mod stream;
mod stream_schema;
mod tables;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod versions;
