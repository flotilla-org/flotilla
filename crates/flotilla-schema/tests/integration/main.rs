// Keep integration coverage in one executable to avoid linking the crate graph per module.
mod addresses;
mod aliases;
mod codec;
mod compile_fail;
mod edges;
mod event;
mod indexes;
mod inline;
mod states;
mod tables;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod view;
