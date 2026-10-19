#![allow(dead_code)]
// The discovery testkit no longer reaches core, so its fakes share this test
// build's trait identities and are used directly.
pub(crate) use flotilla_discovery_testkit as discovery;
// The in-process extension implements a trait for core's own daemon type, so it
// compiles against this unit-test build of core.
#[path = "../../flotilla-orchestration-testkit/src/in_process.rs"]
pub(crate) mod in_process_ext;
#[path = "../../flotilla-replay-testkit/src/lib.rs"]
pub(crate) mod replay;
