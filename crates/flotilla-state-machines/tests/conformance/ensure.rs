//! Ensure against `conformance/ensure.qnt`, over the engine's memory log. The adaptor (event and
//! state mapping) is shared with the decided store's run: `flotilla_quint_testkit::ensure`.

use flotilla_quint_testkit::ensure::{EnsureConformance, MemoryLogs};

use crate::graph::{self, Conformance, Report};

// Every reachable Ensure state is reached in Rust by the spec's path, and
// every probe agrees: the same move, or the same refusal with nothing
// recorded.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn ensure_follows_the_full_graph() {
    let mut report = Report::new("ensure");
    graph::check::<EnsureConformance<MemoryLogs>>(&graph::load(EnsureConformance::<MemoryLogs>::MACHINE), &mut report);
    report.finish();
}
