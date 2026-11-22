//! What the reconciles' reads cost (#3108 read-cost audit): a read of one convoy's records reads
//! a number that does not grow with the unrelated records held.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use flotilla_capability_contract::{CredentialCapability, SessionCapabilitySource};
use flotilla_crew::{
    convoy::ConvoyReconciler,
    crew_capabilities::{refresh_cards, CapabilityCardTracker, CardScope},
    reconcilers::vessel::ConvoyVesselWake,
};
use flotilla_engine_convoys_testkit::{convoy, single_agent, EngineConvoysHome, KeyedGets};
use flotilla_provisioning::engine::checkouts::CheckoutConvoyWake;
use flotilla_provisioning_testkit::{EngineFixture, OtherHome, TerminalsFixture};
use flotilla_resources::{
    account_access::AccountRecords,
    address::{artifact_address, record_name},
    checkout_records::CheckoutHolder,
    convoy_records::OpenLaunches,
    terminal_records::TerminalLaunchFacts,
    vessel_records::{environment_vessels, EnvironmentVessel},
    Artifact, ArtifactSpec, CheckoutSpec, Convoy, ConvoySpec, EnvironmentSpec, HostDirectEnvironmentSpec, ObservedCheckoutSpec,
    RepositoryKey, ResourceError, ResourceObject, SystemClock,
};
use flotilla_store::{
    controller::{ControllerLoop, ReconcileOutcome, Reconciler, SecondaryWatch, WorkQueueSender},
    load_metrics::{attributed, measure_now, CallerReads, ControllerLoadMetrics, ControllerLoadSnapshot},
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
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
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

/// The launches read, and the keyed gets made, by one convoy's pass finding its placed vessel,
/// with `others` other convoys each holding a launch of its work.
async fn launches_read_by_a_pass(others: usize) -> (u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let mut launches = Vec::new();
    for n in 0..=others {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        engine.vessels.request(&launch, "work", "host");
        launches.push(launch);
    }
    let passed = backend.clone().using::<Convoy>(NAMESPACE).get(&launches[0]).await.expect("the convoy's view");
    let reconciler = ConvoyReconciler::new(
        Arc::clone(convoys.reads()),
        Arc::new(flotilla_provisioning_testkit::hosts::HeldHosts::default()),
        convoys.path(),
        crate::common::detached_messages(),
    );

    let vessels = &engine.vessels.vessels;
    let before = (vessels.launches_read(), keyed_gets(&convoys, &engine));
    reconciler.prepare(&passed).await.expect("prepare");
    (vessels.launches_read() - before.0, keyed_gets(&convoys, &engine) - before.1)
}

// Behaviour: a pass finds whether its convoy has a vessel by its works' launch index, not by
// listing every vessel at every home and keeping its own.
#[tokio::test]
async fn a_pass_reads_its_convoys_vessels_without_the_others() {
    let (small, small_gets) = launches_read_by_a_pass(5).await;
    let (large, large_gets) = launches_read_by_a_pass(60).await;
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

/// The vessel launches rechecked, and the launches read and keyed gets made, when one convoy's brief
/// artifact changes with `others` other convoys each holding a launch.
async fn queued_by_one_brief(others: usize) -> (Vec<String>, u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let mut launches = Vec::new();
    for n in 0..=others + 1 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        let vessel = engine.vessels.request(&launch, "work", "host");
        launches.push((launch, vessel));
    }

    let accounts: Arc<dyn AccountRecords> = Arc::new(OldStoreAccounts::new(backend.clone()));
    let wake = ConvoyVesselWake::new(&backend, NAMESPACE, Arc::clone(convoys.reads()), convoys.path(), accounts);
    let (sender, mut queued) = tokio::sync::mpsc::channel(256);
    let waking = tokio::spawn(wake.recheck(NAMESPACE.to_owned(), sender));

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
    // The settle lets any earlier publish's wake finish its read before the count starts.
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
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
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
    // The wake's reads count for it, as a controller's watch's do.
    let reads = Arc::new(CallerReads::default());
    let wake = Box::new(CheckoutConvoyWake::new(Arc::clone(store.service())));
    let waking = tokio::spawn(attributed(Arc::clone(&reads), wake.spawn(backend.clone(), NAMESPACE.to_owned(), sender)));
    let converted = || reads.snapshot().get("Convoy").map_or(0, |counts| counts.converted);

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

    let before = (converted(), keyed_gets(&convoys, &engine));
    let changed = &launches[1];
    engine.checkouts.request("changed", &checkout("changed"), Some(&CheckoutHolder::Convoy(changed.clone())));
    let mut names = vec![tokio::time::timeout(Duration::from_secs(5), queued.recv()).await.expect("queued").expect("open queue")];
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(name) = queued.try_recv() {
        names.push(name);
    }
    waking.abort();
    (names, converted() - before.0, keyed_gets(&convoys, &engine) - before.1)
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
    assert_eq!(
        small_gets,
        KeyedGets { vessels: 1, convoys: 1, ..KeyedGets::default() },
        "the holder's view is composed from its record and its work's newest launch, each read by key"
    );
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
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
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
    let gets = keyed_gets(&convoys, &engine);
    let (found, reads) = measure_now(|| environment_vessels(convoys.reads().as_ref(), Some(&BTreeSet::from(["shared".to_owned()]))));
    (
        found.expect("vessels").vessels,
        engine.vessels.vessels.launches_read() - launches,
        reads.get("Convoy").map_or(0, |counts| counts.converted),
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

/// Credentials and endpoints as delivered: none.
struct NothingDelivered;

#[async_trait::async_trait]
impl SessionCapabilitySource for NothingDelivered {
    async fn credentials(&self, _: &str, _: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
        Ok(Vec::new())
    }
}

/// The terminal and vessel launches read, and the keyed gets made, refreshing the capability
/// cards `scope` names of the convoys launched, with `others` other convoys each holding a running
/// crew in an environment of its own. The first two convoys' crews run in the shared environment.
async fn read_by_cards(others: usize, scope: impl FnOnce(&[String]) -> CardScope) -> (u64, u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let host_direct = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
        ..Default::default()
    };
    engine.environments.request("shared", &host_direct, None);
    let mut launches = Vec::new();
    for n in 0..others + 2 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        let vessel = engine.vessels.request(&launch, "work", "host");
        launches.push(launch);
        let environment = if n < 2 { "shared".to_owned() } else { format!("env-{n}") };
        if n >= 2 {
            engine.environments.request(&environment, &host_direct, None);
        }
        let session = engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", &environment));
        engine.terminals.ready(&session, &format!("{session}-runtime"), &TerminalLaunchFacts::default());
    }
    // The convoy views are filled once, at their first read, as a running daemon's already are.
    backend.clone().using::<Convoy>(NAMESPACE).list().await.expect("the convoy views");
    let tracker = CapabilityCardTracker::with_message_path(crate::common::detached_messages());
    let accounts = OldStoreAccounts::new(backend.clone());
    let scope = scope(&launches);

    let before = (engine.terminals.terminals.launches_read(), engine.vessels.vessels.launches_read(), keyed_gets(&convoys, &engine));
    refresh_cards(&backend, NAMESPACE, &NothingDelivered, &tracker, Arc::clone(convoys.reads()), &accounts, &scope)
        .await
        .expect("the cards are refreshed");
    (
        engine.terminals.terminals.launches_read() - before.0,
        engine.vessels.vessels.launches_read() - before.1,
        keyed_gets(&convoys, &engine) - before.2,
    )
}

// Behaviour: refreshing one environment's capability cards reads that environment's sessions
// through its index, and each one's newest launch, vessel, convoy and environment by key, not
// every session at this home.
#[tokio::test]
async fn an_environments_cards_are_refreshed_without_the_others() {
    let shared = |_: &[String]| CardScope { environments: BTreeSet::from(["shared".to_owned()]), ..CardScope::default() };
    let (small_terminals, small_vessels, small_gets) = read_by_cards(5, shared).await;
    let (large_terminals, large_vessels, large_gets) = read_by_cards(60, shared).await;
    assert_eq!(
        small_terminals, 4,
        "only the shared environment's two launches are read through its index, and again as their convoys are composed"
    );
    assert_eq!(large_terminals, small_terminals, "the cost does not grow with the other environments' sessions");
    assert_eq!(small_vessels, 0, "each card's vessel, and each composed convoy's work's newest launch, is read by key");
    assert_eq!(large_vessels, small_vessels, "the vessel launches read do not grow with the other convoys");
    assert_eq!(
        small_gets,
        KeyedGets { vessels: 4, convoys: 2, environments: 2, terminals: 2 },
        "each card gets its session's newest launch, its vessel and its environment once; its convoy's view is composed from its \
         record and its work's newest launch, each read by key"
    );
    assert_eq!(large_gets, small_gets, "the keyed gets do not grow with the other convoys");
}

// Behaviour: refreshing one convoy's capability cards, as its account binding wakes it, reads its vessels through its works' launch index and its sessions
// through each vessel's, not every session at this home.
#[tokio::test]
async fn a_convoys_cards_are_refreshed_without_the_others() {
    let first = |launches: &[String]| CardScope { convoys: BTreeSet::from([launches[0].clone()]), ..CardScope::default() };
    let (small_terminals, small_vessels, small_gets) = read_by_cards(5, first).await;
    let (large_terminals, large_vessels, large_gets) = read_by_cards(60, first).await;
    assert_eq!(
        small_terminals, 2,
        "only the convoy's one session launch is read through its vessel's index, by the card and by its convoy's composition"
    );
    assert_eq!(large_terminals, small_terminals, "the cost does not grow with the other convoys' sessions");
    assert_eq!(small_vessels, 1, "only the convoy's one vessel launch is read through its work's launch index, by the card");
    assert_eq!(large_vessels, small_vessels, "the vessel launches read do not grow with the other convoys");
    assert_eq!(
        small_gets,
        KeyedGets { vessels: 1, convoys: 2, environments: 1, terminals: 1 },
        "the convoy is got once for its works and once as its view is composed, with its work's newest launch by key; the card \
         gets its session's newest launch and its environment once, and reuses the vessel its index read"
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

/// The open launches read, and the convoy listings and keyed gets the reads made, when one of
/// `others + 1` open launches has ended and another was admitted since the first read.
async fn read_by_open_launches(others: usize) -> (usize, u64, KeyedGets) {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let reads = convoys.reads().convoys().expect("convoys");
    // The log's changes reach followers from a task: each read below waits until they are quiet,
    // so a change still on its way is not counted in the wrong read.
    let mut sent = reads.changes();
    let home = &convoys;
    let admit = |n: usize| async move {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
        let mut admitted = convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"));
        admitted.metadata.name = home.launch(admitted.clone()).await;
        admitted
    };
    let mut launches = Vec::new();
    for n in 0..=others {
        launches.push(admit(n).await);
    }
    quiet(&mut sent).await;
    let mut open = OpenLaunches::default();
    assert_eq!(open.read(reads).expect("first read").len(), others + 1, "the first read lists every open launch");

    let before = (convoys.convoy_listings(), convoys.keyed_gets());
    convoys.path().abandon(&launches[0], "operator", "ended").await.expect("end the first launch");
    let ended = &launches[0].metadata.name;
    let admitted = admit(others + 1).await.metadata.name;
    quiet(&mut sent).await;
    let read = open.read(reads).expect("read");
    assert!(read.contains(&admitted) && !read.contains(ended), "the end and the admission are read: {read:?}");
    (read.len(), convoys.convoy_listings() - before.0, convoys.keyed_gets() - before.1)
}

/// Waits until no change has been sent for 300 ms.
async fn quiet(sent: &mut tokio::sync::broadcast::Receiver<flotilla_resources::convoy_records::ConvoyChange>) {
    use tokio::sync::broadcast::error::RecvError;
    while let Ok(received) = tokio::time::timeout(Duration::from_millis(300), sent.recv()).await {
        assert!(!matches!(received, Err(RecvError::Closed)), "the convoys' changes are followed");
    }
}

// Behaviour: the open launches a grant event reaches are read again by the launches changed since
// the last read, not by listing every launch at this home.
#[tokio::test]
async fn open_launches_are_read_by_those_changed_since() {
    let (small, small_listings, small_gets) = read_by_open_launches(5).await;
    let (large, large_listings, large_gets) = read_by_open_launches(60).await;
    assert_eq!((small, large), (6, 61), "the ended launch is gone and the admitted one is there");
    assert_eq!(small_listings, 0, "no launch is listed again");
    assert_eq!(large_listings, small_listings, "the cost does not grow with the other convoys");
    assert_eq!(small_gets, KeyedGets { convoys: 2, ..KeyedGets::default() }, "only the ended and the admitted launches are got");
    assert_eq!(large_gets, small_gets, "the keyed gets do not grow with the other convoys");
}

/// Changes nothing: a pass reads only what the controller reads to wake it.
struct Unchanged;

impl Reconciler for Unchanged {
    type Resource = Convoy;
    type Prepared = ();

    async fn prepare(&self, _: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn reconcile(&self, _: &ResourceObject<Convoy>, _: &(), _: chrono::DateTime<Utc>) -> ReconcileOutcome<Convoy> {
        ReconcileOutcome::new(None)
    }

    async fn run_finalizer(&self, _: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

/// A Convoy controller's load metrics once it has reconciled each of `count` convoys held by
/// this home's engine.
async fn first_pass(count: usize) -> ControllerLoadSnapshot {
    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    for n in 0..count {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(NAMESPACE.into()).build();
        convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
    }
    let metrics = Arc::new(ControllerLoadMetrics::default());
    let controller = ControllerLoop {
        primary: backend.using::<Convoy>(NAMESPACE),
        secondaries: Vec::new(),
        reconciler: Unchanged,
        backend: backend.clone(),
    };
    let running = tokio::spawn(controller.run_observed(Arc::clone(&metrics)));
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.snapshot().reconcile.count() < count as u64 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("each convoy is reconciled");
    running.abort();
    metrics.snapshot()
}

// Behaviour: a controller's load metrics count its wakes against what queued them, and its
// reads by kind and shape, apart from its watches' (#3108 load measurement). Its first pass gets
// each convoy by key through the engine port, composed from its records by key and index, each
// converted to the old shape; its watch lists the kind as often whatever the convoys held.
#[tokio::test]
async fn a_controller_pass_counts_its_reads_by_kind_and_shape() {
    let mut listings = Vec::new();
    for count in [2, 8] {
        let pass = first_pass(count).await;
        let n = count as u64;
        assert_eq!((pass.enqueued, pass.enqueued_coalesced, pass.wakes), (n, 0, n), "each listed convoy is queued and woken once");
        let port = pass.reads.get("Convoy port").copied().unwrap_or_default();
        assert_eq!((port.keyed, port.index, port.whole_kind, port.rows), (n, 0, 0, n), "each wake gets its convoy by key: {pass:#?}");
        let store = pass.reads.get("Convoy").copied().unwrap_or_default();
        assert_eq!(
            (store.keyed, store.index, store.whole_kind),
            (n, 0, 0),
            "the port composes each convoy got from its record, read once: {pass:#?}"
        );
        assert_eq!(store.converted, n, "each convoy got is converted once: {pass:#?}");
        let vessels = pass.reads.get("Vessel").copied().unwrap_or_default();
        assert_eq!((vessels.keyed, vessels.index, vessels.whole_kind), (0, n, 0), "each work's newest launch through its index: {pass:#?}");
        let watch = pass.watch_reads.get("Convoy port").copied().unwrap_or_default();
        assert!(watch.whole_kind > 0, "the watch lists the kind: {pass:#?}");
        assert_eq!((watch.keyed, watch.rows), (0, watch.whole_kind * n), "each listing returns every convoy: {pass:#?}");
        listings.push(watch.whole_kind);
    }
    assert_eq!(listings[0], listings[1], "the watch lists the kind as often whatever the convoys held");
}

/// One home's convoys, each running a coder in a ready vessel, with the Convoy and TerminalSession
/// views installed on `backend`.
struct CrewedConvoys {
    store: OtherHome,
    engine: EngineFixture,
    convoys: EngineConvoysHome,
    /// The TerminalSession views `backend` serves.
    sessions: Arc<flotilla_store::TerminalViews>,
    backend: ResourceBackend,
    /// The first convoy's launch and its coder's session.
    target: (String, String),
    /// The launch of the first convoy's coder's session.
    target_session: String,
    /// Each convoy's launch, its Vessel launch and its coder's session launch, the target first.
    launches: Vec<(String, String, String)>,
}

/// A target convoy of project "app" and `others` of project "other", each with a coder: all in
/// one environment, or each in an environment of its own when `own_environments`.
async fn crewed_convoys(others: usize, own_environments: bool) -> CrewedConvoys {
    use flotilla_resources::terminal_records::AgentLaunch;
    use flotilla_store::TerminalViews;

    let store = OtherHome::open("home");
    let engine = EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = EngineConvoysHome::over(store.service(), NAMESPACE, &backend, Arc::new(SystemClock), engine.reads());
    let sessions = Arc::new(TerminalViews::new(NAMESPACE, Arc::clone(convoys.reads())));
    backend.install_engine_records(Arc::clone(&sessions) as _);
    let host_direct = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
        ..Default::default()
    };
    let mut target = None;
    let mut target_session = None;
    let mut launches = Vec::new();
    for n in 0..=others {
        let project = if n == 0 { "app" } else { "other" };
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(project.into()).build();
        let launch = convoys.launch(convoy(NAMESPACE, &format!("convoy-{n}"), spec, single_agent("coder"))).await;
        let vessel = engine.vessels.request(&launch, "work", store.home());
        let environment = if own_environments { format!("env-{n}") } else { "env".to_owned() };
        if own_environments || n == 0 {
            engine.environments.request(&environment, &host_direct, Some(&vessel));
        }
        engine.vessels.ready(&vessel, &environment);
        let session = engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", &environment));
        let facts = TerminalLaunchFacts {
            agent: Some(AgentLaunch { adapter: "codex".into(), model: None, stance: "test".into() }),
            ..TerminalLaunchFacts::default()
        };
        engine.terminals.ready(&session, &format!("pool-{n}"), &facts);
        if n == 0 {
            target = Some((launch.clone(), engine.terminals.record(&session).session_name().to_owned()));
            target_session = Some(session.clone());
        }
        launches.push((launch, vessel, session));
    }
    let (target, target_session) = (target.expect("the target convoy"), target_session.expect("the target's session"));
    CrewedConvoys { store, engine, convoys, sessions, backend, target, target_session, launches }
}

/// What each of a scoped fleet listing's selecting queries read, by kind, with `others` convoys
/// of another project each running a coder.
async fn scoped_query_reads(others: usize) -> Vec<flotilla_store::load_metrics::ReadCounts> {
    use flotilla_resources::{TerminalSession, CONVOY_LABEL};
    use flotilla_store::{load_metrics::measure, StoreQuery};

    let CrewedConvoys { store: _store, engine: _engine, convoys: _convoys, backend, target: (launch, session), .. } =
        crewed_convoys(others, false).await;
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    sessions.list().await.expect("the session views");
    let crew = sessions.get(&session).await.expect("the target's session").status.and_then(|status| status.crew).expect("its crew").id;

    let mut costs = Vec::new();
    let by_name = StoreQuery::names([launch.clone()], true);
    let convoy_queries = [
        // The project scope.
        StoreQuery { fields: [("spec.project_ref".to_owned(), "app".to_owned())].into(), include_replicas: true, ..StoreQuery::default() },
        // The convoy scope: the convoy and its project's convoys.
        StoreQuery {
            any_of: Some(vec![
                by_name.clone(),
                StoreQuery { include_replicas: true, ..StoreQuery::related("spec.project_ref", "spec.project_ref", by_name) },
            ]),
            include_replicas: true,
            ..StoreQuery::default()
        },
    ];
    for query in convoy_queries {
        let (read, reads) = measure(backend.query_declared::<Convoy>(NAMESPACE, &query)).await;
        assert_eq!(
            read.expect("convoys").items.iter().map(|read| read.object.metadata.name.as_str()).collect::<Vec<_>>(),
            [launch.as_str()]
        );
        costs.push(reads);
    }
    let session_queries = [
        // A crew's session, by its crew identity.
        StoreQuery { fields: [("status.crew.id".to_owned(), crew)].into(), include_replicas: true, ..StoreQuery::default() },
        // Its convoy's sessions.
        StoreQuery {
            field_values: [(format!("metadata.labels.\"{CONVOY_LABEL}\""), vec![launch.clone()])].into(),
            include_replicas: true,
            ..StoreQuery::default()
        },
    ];
    for query in session_queries {
        let (read, reads) = measure(backend.query_declared::<TerminalSession>(NAMESPACE, &query)).await;
        assert_eq!(
            read.expect("sessions").items.iter().map(|read| read.object.metadata.name.as_str()).collect::<Vec<_>>(),
            [session.as_str()]
        );
        costs.push(reads);
    }
    costs
}

// Behaviour: a scoped fleet listing's queries of sessions by crew and by convoy read through the
// session views' indexes: the rows read are the scope's, whatever other convoys and sessions are
// held. Its queries of convoys by project, and by a convoy and its project's convoys, give the
// scope's convoys; a generic read of convoys composes the launches it reads (#4412 C), and may
// scan them.
#[tokio::test]
async fn scoped_queries_read_the_scopes_rows() {
    let (small, large) = (scoped_query_reads(0).await, scoped_query_reads(200).await);
    for reads in small.iter().chain(&large).skip(2) {
        let port = reads.get("TerminalSession port").copied().unwrap_or_default();
        assert_eq!(port.whole_kind, 0, "no query reads every session row: {reads:#?}");
    }
    assert_eq!(large[2..], small[2..], "the session queries' reads do not grow with other convoys and sessions");
}

/// What a daemon's first fill of the TerminalSession views reads with `convoys` crewed convoys
/// held: a read port opened afresh over the same store, as at a start, the views installed over
/// it and read once.
async fn first_fill_reads(convoys: usize, own_environments: bool) -> flotilla_store::load_metrics::ReadCounts {
    use flotilla_resources::TerminalSession;
    use flotilla_store::{load_metrics::measure, TerminalViews};

    let crewed = crewed_convoys(convoys - 1, own_environments).await;
    let restarted = EngineFixture::over_service(crewed.store.service());
    let started = ResourceBackend::new(InMemoryBackend::default());
    let ((), counts) = measure(async {
        let home = EngineConvoysHome::over(crewed.store.service(), NAMESPACE, &started, Arc::new(SystemClock), restarted.reads());
        started.install_engine_records(Arc::new(TerminalViews::new(NAMESPACE, Arc::clone(home.reads()))));
        let listed = started.clone().using::<TerminalSession>(NAMESPACE).list().await.expect("the session views");
        assert_eq!(listed.items.len(), convoys);
    })
    .await;
    counts
}

/// The read calls in `reads`, by kind: keyed, indexed and whole-kind reads, not the rows they read.
fn read_calls(reads: &flotilla_store::load_metrics::ReadCounts) -> BTreeMap<String, u64> {
    reads.iter().map(|(kind, counts)| (kind.clone(), counts.keyed + counts.index + counts.whole_kind)).collect()
}

// Behaviour: a start's first fill of the TerminalSession views reads each Vessel and session
// launch, and each launch's facts, once: one read of each kind, joined in memory, so the read calls do not grow with the convoys held while the rows read do, one each.
// The environments the sessions run in are read once too, shared or each its own.
#[tokio::test]
async fn a_first_fill_reads_each_joined_row_once() {
    for own_environments in [false, true] {
        let (small, large) = (first_fill_reads(5, own_environments).await, first_fill_reads(40, own_environments).await);
        for (n, reads) in [(5, &small), (40, &large)] {
            for kind in ["Vessel", "TerminalSession", "TerminalLaunchFacts"] {
                let counts = reads.get(kind).copied().unwrap_or_default();
                assert_eq!((counts.keyed, counts.index, counts.whole_kind), (0, 0, 1), "{kind} is read in one call: {reads:#?}");
                assert_eq!((counts.rows, counts.decoded), (n, n), "each {kind} row is read and decoded once: {reads:#?}");
            }
            let environments = if own_environments { n } else { 1 };
            let counts = reads.get("Environment").copied().unwrap_or_default();
            assert_eq!((counts.keyed, counts.index, counts.whole_kind), (0, 0, 1), "Environment is read in one call: {reads:#?}");
            assert_eq!((counts.rows, counts.decoded), (environments, environments), "each Environment once: {reads:#?}");
            assert_eq!(
                reads.get("EnvironmentLaunchFacts").map_or(0, |counts| counts.keyed),
                0,
                "no environment's facts by key: {reads:#?}"
            );
        }
        assert_eq!(read_calls(&large), read_calls(&small), "the fill's read calls do not grow with the convoys held");
    }
}

// Behaviour: once the views are filled, a session launch's change is read once for both the
// Convoy and the TerminalSession views, whichever passes first: one keyed get of the launch, and
// none of the Vessel launches it is joined to.
#[tokio::test]
async fn a_change_is_read_once_for_both_views() {
    use flotilla_resources::TerminalSession;

    let crewed = crewed_convoys(4, false).await;
    let convoys = crewed.backend.clone().using::<Convoy>(NAMESPACE);
    let sessions = crewed.backend.clone().using::<TerminalSession>(NAMESPACE);
    convoys.list().await.expect("fill the convoy views");
    sessions.list().await.expect("fill the session views");
    let before = crewed.convoys.keyed_gets();

    crewed.engine.terminals.exited(&crewed.target_session, "done");
    let listed = sessions.list().await.expect("the session views");
    convoys.list().await.expect("the convoy views");

    let exited = listed.items.iter().find(|session| session.metadata.name == crewed.target.1).expect("the target's session");
    assert!(exited.status.as_ref().is_some_and(|status| status.stopped_at.is_some()), "the views show the change: {exited:?}");
    let gets = crewed.convoys.keyed_gets() - before;
    // The convoy listing composes each of the five convoys afresh, each with its work's newest
    // launch read by key; the session view reads the changed launch once.
    assert_eq!((gets.terminals, gets.vessels), (1, 5), "the change is read once, each convoy's vessel by key: {gets:?}");
}

// Behaviour: the held joins hold live work, not history. Once half the convoys end and their
// Vessel and session launches are released, and both views have composed that, those launches are
// no longer held; until the session views have read a release, its session stays listed by them
// correctly as released. An ended convoy composed again later keeps what its works showed.
#[tokio::test]
async fn ended_launches_leave_the_held_joins() {
    use flotilla_resources::{convoy::engine::set_notes, vessel_records::VesselStep, TerminalSession};

    let crewed = crewed_convoys(9, false).await;
    let convoys = crewed.backend.clone().using::<Convoy>(NAMESPACE);
    let sessions = crewed.backend.clone().using::<TerminalSession>(NAMESPACE);
    convoys.list().await.expect("fill the convoy views");
    sessions.list().await.expect("fill the session views");
    let joins = crewed.convoys.reads().joins().expect("the held joins");
    let held = joins.held();
    assert_eq!((held.vessels, held.terminals), (10, 10), "every launch is held: {held:?}");

    let (ended, live) = crewed.launches.split_at(5);
    let engine = crewed.convoys.path();
    for (launch, vessel, session) in ended {
        let view = convoys.get(launch).await.expect("the convoy");
        engine.abandon(&view, "operator", "ended").await.expect("end the convoy");
        crewed.engine.terminals.release(session);
        crewed.engine.vessels.step(vessel, VesselStep::Released);
    }
    // The convoy views compose the ends; the session views have not read the releases yet.
    convoys.list().await.expect("the convoy views");
    convoys.list().await.expect("the convoy views");
    let composed = convoys.get(&ended[0].0).await.expect("an ended convoy").status.expect("its status").work;
    assert!(composed.get("work").is_some_and(|work| work.started_at.is_some()), "its work shows its Vessel launch: {composed:?}");
    let listed = sessions.list().await.expect("the session views");
    let names: BTreeSet<String> = listed.items.iter().map(|session| session.metadata.name.clone()).collect();
    for (_, _, session) in live {
        assert!(names.contains(crewed.engine.terminals.record(session).session_name()), "a live session is listed: {names:?}");
    }
    assert_eq!(names.len(), live.len(), "the released sessions are listed no more: {names:?}");

    convoys.list().await.expect("the convoy views");
    let held = joins.held();
    assert_eq!((held.vessels, held.terminals), (5, 5), "only the live launches are held: {held:?}");

    // Composed again as its notes change, an ended convoy's works show what they did.
    let mut status = convoys.get(&ended[0].0).await.expect("an ended convoy").status.expect("its status");
    status.branch_subject_scan_error = Some("noted after the end".into());
    set_notes(&crewed.convoys.path(), &ended[0].0, &status);
    let again = convoys.get(&ended[0].0).await.expect("an ended convoy").status.expect("its status");
    assert_eq!(again.branch_subject_scan_error.as_deref(), Some("noted after the end"), "the view is composed again");
    assert_eq!(again.work, composed, "its works show what they did");
}

/// What the held joins, the session views and the Vessel views hold.
#[derive(Debug, PartialEq, Eq)]
struct HeldWork {
    joins: (usize, usize),
    sessions: usize,
    vessels: usize,
}

/// What is held with `convoys` convoys running, and again once each has ended with its Vessel and
/// session launches: every other one released, the rest ended Broke and Lost and never released.
/// With `watched`, the Vessel views are watched, as a daemon's are, and keep their listing from
/// pass to pass; unwatched, each read lists them afresh.
async fn held_before_and_after_ending(convoys: usize, watched: bool) -> (HeldWork, HeldWork) {
    use flotilla_resources::{vessel_records::VesselStep, TerminalSession, Vessel};
    use flotilla_store::VesselViews;

    let crewed = crewed_convoys(convoys - 1, false).await;
    let vessel_views = Arc::new(VesselViews::new(crewed.backend.clone(), NAMESPACE, Arc::clone(crewed.convoys.reads())));
    crewed.backend.install_engine_records(Arc::clone(&vessel_views) as _);
    let _watch = match watched {
        true => Some(crewed.backend.clone().including_replicas::<Vessel>(NAMESPACE).watch().await.expect("watch the vessels")),
        false => None,
    };
    let views = (
        crewed.backend.clone().using::<Convoy>(NAMESPACE),
        crewed.backend.clone().using::<TerminalSession>(NAMESPACE),
        crewed.backend.clone().using::<Vessel>(NAMESPACE),
    );
    let joins = crewed.convoys.reads().joins().expect("the held joins");
    let held = || async {
        // Each view reads what changed: the ends, the joins' evictions after them, and those.
        for _ in 0..3 {
            views.0.list().await.expect("the convoy views");
            views.1.list().await.expect("the session views");
            views.2.list().await.expect("the vessel views");
        }
        let joins = joins.held();
        HeldWork {
            joins: (joins.vessels, joins.terminals),
            sessions: crewed.sessions.held().await.expect("the session views"),
            vessels: vessel_views.held().await.expect("the vessel views"),
        }
    };
    let running = held().await;

    let engine = crewed.convoys.path();
    for (n, (launch, vessel, session)) in crewed.launches.iter().enumerate() {
        let view = views.0.get(launch).await.expect("the convoy");
        engine.abandon(&view, "operator", "ended").await.expect("end the convoy");
        if n % 2 == 0 {
            crewed.engine.terminals.release(session);
            crewed.engine.vessels.step(vessel, VesselStep::Released);
        } else {
            crewed.engine.terminals.lost(session, "gone");
            crewed.engine.vessels.step(vessel, VesselStep::Broke { reason: "gone".into() });
        }
    }
    let ended = held().await;
    // The second convoy's launches ended and were never released.
    let (_, vessel, session) = &crewed.launches[1];
    let lost = crewed.engine.terminals.record(session);
    // Nothing holds it once its vessel and convoy ended: it retired, and reads as absent by name
    // as from the listing (#4421).
    assert!(views.1.get(lost.session_name()).await.is_err(), "a retired session reads as absent by name");
    assert!(views.2.get(vessel).await.is_ok(), "a Vessel launch that ended is read by name");
    (running, ended)
}

// Behaviour: the joins and the session and Vessel views hold live work, not history. Once every
// convoy has ended, and its launches have ended released or ended and were never released, they
// hold nothing, whether 3 convoys ran or 12. A read by name still finds an ended launch.
#[tokio::test]
async fn ended_work_leaves_the_views_at_any_size() {
    for watched in [true, false] {
        let (small_running, small) = held_before_and_after_ending(3, watched).await;
        let (large_running, large) = held_before_and_after_ending(12, watched).await;
        assert_eq!(small_running, HeldWork { joins: (3, 3), sessions: 3, vessels: 6 }, "each live launch is held");
        assert_eq!(large_running, HeldWork { joins: (12, 12), sessions: 12, vessels: 24 }, "each live launch is held");
        let none = || HeldWork { joins: (0, 0), sessions: 0, vessels: 0 };
        assert_eq!((small, large), (none(), none()), "nothing ended is held, watched: {watched}");
    }
}
