// Keep integration coverage in one executable to avoid linking the crate graph per module.
mod codec;
mod compile_fail;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
