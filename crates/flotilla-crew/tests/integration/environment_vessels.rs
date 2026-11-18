//! The vessels credential delivery reads in an environment, from the engine's records.

use std::{collections::BTreeSet, sync::Arc};

use flotilla_engine_convoys_testkit::{convoy, single_agent, EngineConvoysHome};
use flotilla_provisioning_testkit::{EngineFixture, OtherHome, TerminalsFixture};
use flotilla_resources::{
    host_direct_environment_name,
    vessel_records::{environment_vessels, VesselWithoutConvoy},
    ConvoySpec, EnvironmentSpec, HostDirectEnvironmentSpec, SystemClock,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

/// A home's engine, with its convoys served on a backend.
fn home() -> (OtherHome, EngineFixture, EngineConvoysHome) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    (store, engine, convoys)
}

/// Launches convoy `name` with one work, and that work's vessel realised in `environment` with
/// its agent crew requested there; returns the vessel's launch.
async fn vessel_in(engine: &EngineFixture, convoys: &EngineConvoysHome, name: &str, environment: &str) -> String {
    let spec = ConvoySpec::builder().project_ref(NAMESPACE.into()).workflow_ref("workflow".into()).build();
    let launch = convoys.launch(convoy(NAMESPACE, name, spec, single_agent("coder"))).await;
    let vessel = engine.vessels.request(&launch, "work", "host");
    engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", environment));
    engine.vessels.ready(&vessel, environment);
    vessel
}

// Behaviour: a host's own environment is held by no vessel; each vessel realised in it is found
// through the crew launches that name it.
#[tokio::test]
async fn each_vessel_in_a_host_direct_environment_is_read() {
    let (_store, engine, convoys) = home();
    let shared = host_direct_environment_name("host");
    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    engine.environments.request(&shared, &spec, None);
    let first = vessel_in(&engine, &convoys, "convoy-a", &shared).await;
    let second = vessel_in(&engine, &convoys, "convoy-b", &shared).await;
    vessel_in(&engine, &convoys, "convoy-c", "env-c").await;

    let read = environment_vessels(convoys.reads().as_ref(), Some(&BTreeSet::from([shared.clone()]))).expect("vessels");
    let names: BTreeSet<_> = read.vessels.iter().map(|vessel| vessel.name.clone()).collect();
    assert_eq!(names, BTreeSet::from([first, second]), "both vessels in the host's environment: {read:?}");
    assert!(read.vessels.iter().all(|vessel| vessel.environment == shared && vessel.live), "{read:?}");
    assert!(read.without_convoy.is_empty());
}

// Behaviour: a vessel whose convoy's record is missing is left out and named, and the other
// vessels in its environment are still read, in a named read and a full one.
#[tokio::test]
async fn a_vessel_without_its_convoy_is_named_and_the_others_read() {
    let (_store, engine, convoys) = home();
    let kept = vessel_in(&engine, &convoys, "convoy-a", "shared").await;
    let orphan = engine.vessels.request("gone", "work", "host");
    engine.terminals.request(&TerminalsFixture::agent(&orphan, "coder", "shared"));
    engine.vessels.ready(&orphan, "shared");

    let missing = vec![VesselWithoutConvoy { vessel: orphan, convoy: "gone".into(), convoy_home: "home".into() }];
    for environments in [Some(BTreeSet::from(["shared".to_owned()])), None] {
        let read = environment_vessels(convoys.reads().as_ref(), environments.as_ref()).expect("vessels");
        assert_eq!(read.vessels.iter().map(|vessel| vessel.name.as_str()).collect::<Vec<_>>(), [kept.as_str()], "{read:?}");
        assert_eq!(read.without_convoy, missing, "the vessel and its missing convoy are named");
    }
}
