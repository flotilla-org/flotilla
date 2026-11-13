//! What the reconciles' reads cost (#3108 read-cost audit): a read of one convoy's records reads
//! a number that does not grow with the unrelated records held.

use std::{
    collections::BTreeSet,
    sync::{Arc, OnceLock},
    time::Duration,
};

use chrono::Utc;
use flotilla_crew::reconcilers::vessel::ConvoyVesselWake;
use flotilla_engine_convoys_testkit::{convoy, single_agent, EngineConvoysHome, KeyedGets};
use flotilla_provisioning::engine::checkouts::CheckoutConvoyWake;
use flotilla_provisioning_testkit::{EngineFixture, OtherHome, TerminalsFixture};
use flotilla_resources::{
    account_access::AccountRecords,
    address::{artifact_address, record_name},
    checkout_records::CheckoutHolder,
    vessel_records::{environment_vessels, EnvironmentVessel},
    Artifact, ArtifactSpec, CheckoutSpec, ConvoySpec, ObservedCheckoutSpec, RepositoryKey, SystemClock,
};
use flotilla_store::{
    controller::{SecondaryWatch, WorkQueueSender},
    ResourceBackend,
};
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::OldStoreAccounts;

use crate::common::meta;

const NAMESPACE: &str = "flotilla";

/// The keyed gets made so far through `convoys`' reads, with the convoy records `engine`'s
/// vessels read by name for `of_convoy` counted as convoy gets.
fn keyed_gets(convoys: &EngineConvoysHome, engine: &EngineFixture) -> KeyedGets {
    let mut gets = convoys.keyed_gets();
    gets.convoys += engine.vessels.vessels.convoys_read();
    gets
}

/// The launches read, and the keyed gets made, by one convoy's `of_convoy`, with `others` other
/// convoys each holding a launch of its work.
async fn launches_read_by_of_convoy(others: usize) -> (u64, KeyedGets) {
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
    let before = (vessels.launches_read(), keyed_gets(&convoys, &engine));
    let read = convoys.reads().vessels().expect("vessels").of_convoy(&launches[0]).expect("of convoy");
    assert_eq!(read.iter().map(|launch| launch.convoy.as_str()).collect::<Vec<_>>(), [launches[0].as_str()]);
    (vessels.launches_read() - before.0, keyed_gets(&convoys, &engine) - before.1)
}

// Behaviour: a convoy's vessel launches are read by its works' launch index, not by reading every
// launch at every home and keeping its own.
#[tokio::test]
async fn a_convoys_vessels_are_read_without_the_others() {
    let (small, small_gets) = launches_read_by_of_convoy(5).await;
    let (large, large_gets) = launches_read_by_of_convoy(60).await;
    assert_eq!(small, 1, "only the convoy's one launch is read");
    assert_eq!(large, small, "the cost does not grow with the other convoys' launches");
    assert_eq!(small_gets, KeyedGets { convoys: 1, ..KeyedGets::default() }, "only the convoy's own record is got, for its works");
    assert_eq!(large_gets, small_gets, "the keyed gets do not grow with the other convoys");
}

/// Publishes convoy `convoy`'s brief artifact for `role`, replacing any earlier one.
async fn publish_brief(backend: &ResourceBackend, convoy: &str, role: &str) {
    let name = record_name(&artifact_address(convoy, role, "brief", convoy).expect("artifact address")).expect("record name");
    let artifacts = backend.clone().using::<Artifact>(NAMESPACE);
    let spec = ArtifactSpec::builder()
        .convoy(convoy.to_owned())
        .producer(role.to_owned())
        .kind("brief".to_owned())
        .subject(convoy.to_owned())
        .digest(format!("{:064}", Utc::now().timestamp_nanos_opt().unwrap_or_default()))
        .size(12)
        .media_type("text/markdown".to_owned())
        .expires_at(Utc::now() + chrono::Duration::days(3650))
        .build();
    match artifacts.get(&name).await {
        Ok(prior) => {
            artifacts.update(&meta(&name), &prior.metadata.resource_version, &spec).await.expect("replace brief");
        }
        Err(_) => {
            artifacts.create(&meta(&name), &spec).await.expect("publish brief");
        }
    }
}

/// The vessel launches queued, and the launches read and keyed gets made, when one convoy's brief
/// artifact changes with `others` other convoys each holding a launch.
async fn queued_by_one_brief(others: usize) -> (Vec<String>, u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let mut launches = Vec::new();
    for n in 0..=others + 1 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        let vessel = engine.vessels.request(&launch, "work", "host");
        launches.push((launch, vessel));
    }

    let accounts: Arc<dyn AccountRecords> = Arc::new(OldStoreAccounts::new(backend.clone()));
    let wake = ConvoyVesselWake::new(&backend, NAMESPACE, Arc::clone(convoys.reads()), Arc::new(OnceLock::new()), accounts);
    let (sender, mut queued) = WorkQueueSender::channel();
    let waking = tokio::spawn(Box::new(wake).spawn(backend.clone(), NAMESPACE.to_owned(), sender));

    // The wake's watches open after it is spawned: the first convoy's brief is published until it
    // is heard.
    let (warm, warm_vessel) = &launches[0];
    let heard = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            publish_brief(&backend, warm, "coder").await;
            if let Ok(Some(name)) = tokio::time::timeout(Duration::from_millis(50), queued.recv()).await {
                return name;
            }
        }
    })
    .await
    .expect("the wake hears a brief");
    assert_eq!(&heard, warm_vessel);
    // A queued name is not sent again until the controller takes it, which this test never does;
    // the settle lets any earlier publish's wake finish its read before the count starts.
    tokio::time::sleep(Duration::from_millis(100)).await;
    while queued.try_recv().is_ok() {}

    let vessels = &engine.vessels.vessels;
    let before = (vessels.launches_read(), keyed_gets(&convoys, &engine));
    let (changed, _) = &launches[1];
    publish_brief(&backend, changed, "coder").await;
    let mut names = vec![tokio::time::timeout(Duration::from_secs(5), queued.recv()).await.expect("queued").expect("open queue")];
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(name) = queued.try_recv() {
        names.push(name);
    }
    waking.abort();
    (names, vessels.launches_read() - before.0, keyed_gets(&convoys, &engine) - before.1)
}

/// The convoys queued, and the convoy views converted and keyed gets made, when a checkout held
/// for one convoy is requested, with `others` other convoys launched.
async fn queued_by_one_checkout(others: usize) -> (Vec<String>, u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let mut launches = Vec::new();
    for n in 0..=others + 1 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
        launches.push(convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await);
    }
    let checkout = |name: &str| {
        CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: name.into(),
            path: format!("/work/{name}"),
            repo_ref: RepositoryKey("github.com/o/r".into()),
            host_ref: "home".into(),
            is_main: false,
        })
    };

    let (sender, mut queued) = WorkQueueSender::channel();
    let waking =
        tokio::spawn(Box::new(CheckoutConvoyWake::new(Arc::clone(store.service()))).spawn(backend.clone(), NAMESPACE.to_owned(), sender));

    // The wake follows the log from when it starts: the first convoy's checkouts are requested
    // until one is heard.
    let warm = &launches[0];
    let heard = tokio::time::timeout(Duration::from_secs(5), async {
        for n in 0.. {
            let name = format!("warm-{n}");
            engine.checkouts.request(&name, &checkout(&name), Some(&CheckoutHolder::Convoy(warm.clone())));
            if let Ok(Some(name)) = tokio::time::timeout(Duration::from_millis(50), queued.recv()).await {
                return name;
            }
        }
        unreachable!()
    })
    .await
    .expect("the wake hears a checkout");
    assert_eq!(&heard, warm);
    tokio::time::sleep(Duration::from_millis(100)).await;
    while queued.try_recv().is_ok() {}

    let before = (convoys.views().views_converted(), keyed_gets(&convoys, &engine));
    let changed = &launches[1];
    engine.checkouts.request("changed", &checkout("changed"), Some(&CheckoutHolder::Convoy(changed.clone())));
    let mut names = vec![tokio::time::timeout(Duration::from_secs(5), queued.recv()).await.expect("queued").expect("open queue")];
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(name) = queued.try_recv() {
        names.push(name);
    }
    waking.abort();
    (names, convoys.views().views_converted() - before.0, keyed_gets(&convoys, &engine) - before.1)
}

// Behaviour: a checkout's change queues the convoy it is held for, read by that convoy's launch
// name, not by converting every convoy.
#[tokio::test]
async fn a_checkout_queues_only_its_holders_convoy() {
    let (small, small_read, small_gets) = queued_by_one_checkout(5).await;
    let (large, large_read, large_gets) = queued_by_one_checkout(60).await;
    assert_eq!(small.len(), 1, "only the holder's convoy is queued: {small:?}");
    assert_eq!(large.len(), 1, "only the holder's convoy is queued: {large:?}");
    assert_eq!(small_read, 1, "only that convoy's view is converted");
    assert_eq!(large_read, small_read, "the cost does not grow with the other convoys");
    assert_eq!(small_gets, KeyedGets::default(), "the holder's view is held composed: no record is got");
    assert_eq!(large_gets, small_gets, "the keyed gets do not grow with the other convoys");
}

/// The vessels credential delivery finds in the shared environment, and the launches read,
/// convoy views converted and keyed gets made finding them, with `others` other convoys each holding a ready vessel
/// and a starting crew in an environment of its own. Two convoys' vessels are realised in the
/// shared environment, which no vessel holds, as a host's own environment is held.
async fn vessels_in_one_environment(others: usize) -> (Vec<EnvironmentVessel>, u64, u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    for n in 0..others + 2 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        let vessel = engine.vessels.request(&launch, "work", "host");
        let environment = if n < 2 { "shared".to_owned() } else { format!("env-{n}") };
        engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", &environment));
        engine.vessels.ready(&vessel, &environment);
    }
    if others == 5 {
        let every = environment_vessels(convoys.reads().as_ref(), None).expect("every environment's vessels").vessels;
        assert_eq!(every.len(), others + 2, "a full pass finds every vessel: {every:?}");
    }

    let launches = engine.vessels.vessels.launches_read();
    let converted = convoys.views().views_converted();
    let gets = keyed_gets(&convoys, &engine);
    let found = environment_vessels(convoys.reads().as_ref(), Some(&BTreeSet::from(["shared".to_owned()]))).expect("vessels").vessels;
    (
        found,
        engine.vessels.vessels.launches_read() - launches,
        convoys.views().views_converted() - converted,
        keyed_gets(&convoys, &engine) - gets,
    )
}

// Behaviour: credential delivery to one environment reads that environment's vessels and their
// convoys' records by key, not every vessel view at this home and every convoy they serve.
#[tokio::test]
async fn an_environments_credential_vessels_are_read_without_the_others() {
    let (small, small_launches, small_converted, small_gets) = vessels_in_one_environment(5).await;
    let (large, large_launches, large_converted, large_gets) = vessels_in_one_environment(60).await;
    for found in [&small, &large] {
        let convoys: Vec<_> = found.iter().map(|vessel| vessel.convoy.as_str()).collect();
        assert_eq!(convoys.len(), 2, "the shared environment's two vessels: {found:?}");
        assert!(found.iter().all(|vessel| vessel.environment == "shared" && vessel.live && vessel.requirement.name == "work"));
    }
    assert_eq!(small_converted, 0, "no convoy view is converted");
    assert_eq!(large_converted, 0, "no convoy view is converted");
    assert_eq!(small_launches, 0, "each vessel is read by key: no launch is listed, scanned or read through an index");
    assert_eq!(large_launches, small_launches, "the cost does not grow with the other convoys");
    assert_eq!(
        small_gets,
        KeyedGets { environments: 1, vessels: 4, convoys: 2, terminals: 0 },
        "the environment, each of its two vessels and that vessel's newest launch, and their two convoys are got"
    );
    assert_eq!(large_gets, small_gets, "the keyed gets do not grow with the other convoys");
}

// Behaviour: a brief artifact queues only the vessels of the convoy it belongs to, read by that
// convoy's launch index, not every vessel at this home.
#[tokio::test]
async fn a_brief_queues_only_its_convoys_vessels() {
    let (small, small_read, small_gets) = queued_by_one_brief(5).await;
    let (large, large_read, large_gets) = queued_by_one_brief(60).await;
    assert_eq!(small.len(), 1, "only the brief's convoy's vessel is queued: {small:?}");
    assert_eq!(large.len(), 1, "only the brief's convoy's vessel is queued: {large:?}");
    assert_eq!(small_read, 1, "only that convoy's launch is read");
    assert_eq!(large_read, small_read, "the cost does not grow with the other convoys");
    assert_eq!(small_gets, KeyedGets { convoys: 1, ..KeyedGets::default() }, "only that convoy's record is got, for its works");
    assert_eq!(large_gets, small_gets, "the keyed gets do not grow with the other convoys");
}
