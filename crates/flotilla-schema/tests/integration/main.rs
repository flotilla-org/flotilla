// Keep integration coverage in one executable to avoid linking the crate graph per module.
mod addresses;
mod aliases;
mod compile_fail;
mod description;
mod edges;
mod event;
mod inline;
mod tables;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod view;
