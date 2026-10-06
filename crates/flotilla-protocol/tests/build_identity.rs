// The final executable build script uses this std-only helper. Include it here so its tests
// run in the workspace CI suite as well as in standalone rustc invocations.
#[allow(dead_code)]
#[path = "../../build_identity.rs"]
mod build_identity;
