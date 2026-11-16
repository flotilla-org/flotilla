//! The vessel listing served from the engine's records.

use std::sync::Arc;

use flotilla_engine_convoys_testkit::{convoy, single_agent, EngineConvoysHome};
use flotilla_provisioning_testkit::{EngineFixture, OtherHome};
use flotilla_resources::{engine_records::KindRecords, vessel_records::VesselStep, Convoy, ConvoySpec, SystemClock, Vessel};
use flotilla_store::{ResourceBackend, VesselViews};
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

// Behaviour (#3108): a watched vessel listing reads a convoy's end already in the log, though the
// convoys' watch has not told it yet. A table's position, read before its rows, counts the convoy
// entries a vessel's view joins, so the rows must have applied every entry it counts.
#[tokio::test]
async fn a_watched_listing_reads_a_convoy_end_its_watch_has_not_told() {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let views = Arc::new(VesselViews::new(backend.clone(), NAMESPACE, Arc::clone(convoys.reads())));
    backend.install_engine_records(Arc::clone(&views) as _);
    backend.serve_engine_in(NAMESPACE);
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
    let launch = convoys.launch(convoy(NAMESPACE, "convoy-a", spec, single_agent("coder"))).await;
    let vessel = engine.vessels.request(&launch, "work", "host");
    engine.vessels.ready(&vessel, "env");
    engine.vessels.step(&vessel, VesselStep::Lost { reason: "gone".into() });
    // Watched, as a table's feed watches it; the convoys' watch inside is never read.
    let _unread = views.changes().await.expect("watch the vessels");
    let vessels = backend.using::<Vessel>(NAMESPACE);
    let listed = || async { vessels.list().await.expect("list").items.into_iter().map(|vessel| vessel.metadata.name).collect::<Vec<_>>() };
    assert_eq!(listed().await, std::slice::from_ref(&vessel), "a lost launch is listed while its convoy is live");

    let ended = backend.using::<Convoy>(NAMESPACE).get(&launch).await.expect("convoy");
    convoys.path().get().expect("the Convoy path").abandon(&ended, "operator", "abandoned").await.expect("abandon");
    assert!(listed().await.is_empty(), "the convoy's end is read from the log at the listing");
}
