//! What the reconciles' reads cost (#3108 read-cost audit): a read of one convoy's records reads
//! a number that does not grow with the unrelated records held.

use std::sync::Arc;

use flotilla_engine_convoys_testkit::{convoy, single_agent, EngineConvoysHome};
use flotilla_provisioning_testkit::{EngineFixture, OtherHome};
use flotilla_resources::{vessel_records::VesselRecords, ConvoySpec, SystemClock};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

/// The launches read by one convoy's `of_convoy`, with `others` other convoys each holding a
/// launch of its work.
async fn launches_read_by_of_convoy(others: usize) -> u64 {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let mut launches = Vec::new();
    for n in 0..=others {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        engine.vessels.request(&launch, "work", "host");
        launches.push(launch);
    }

    let vessels = &engine.vessels.vessels;
    let before = vessels.launches_read();
    let read = vessels.of_convoy(&launches[0]).expect("of convoy");
    assert_eq!(read.iter().map(|launch| launch.convoy.as_str()).collect::<Vec<_>>(), [launches[0].as_str()]);
    vessels.launches_read() - before
}

// Behaviour: a convoy's vessel launches are read by its works' launch index, not by reading every
// launch at every home and keeping its own.
#[tokio::test]
async fn a_convoys_vessels_are_read_without_the_others() {
    let small = launches_read_by_of_convoy(5).await;
    let large = launches_read_by_of_convoy(60).await;
    assert_eq!(small, 1, "only the convoy's one launch is read");
    assert_eq!(large, small, "the cost does not grow with the other convoys' launches");
}
