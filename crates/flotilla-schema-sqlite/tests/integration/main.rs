// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
mod ddl;
mod fixtures;
mod interning;
mod ownership;
mod registry;
mod representation;
mod round_trip;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod versions;
