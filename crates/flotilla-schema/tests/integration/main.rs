// Keep integration coverage in one executable to avoid linking the crate graph per module.
mod change;
mod codec;
mod compile_fail;
mod edges;
mod indexes;
mod inline;
mod states;
mod tables;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod view;
