//! The terminal port (#3108) over a decided store: launches are requested and renewed through it,
//! stepped as the TerminalSession controller steps them, and read back through it with the facts
//! each was realised with.

use flotilla_protocol::LifecycleReading;
use flotilla_provisioning_testkit::TerminalsFixture;
use flotilla_resources::terminal_records::{
    launched, newest_launches, AgentLaunch, TerminalLaunchFacts, TerminalRecord, TerminalRecords, TerminalRenewal,
};

fn facts() -> TerminalLaunchFacts {
    TerminalLaunchFacts {
        endpoint: Some(("/run/cleat".into(), "daemon-1".into())),
        agent: Some(AgentLaunch { adapter: "claude".into(), model: Some("opus".into()), stance: "bypass".into() }),
        build_jobs: Some(4),
    }
}

fn names(records: Vec<TerminalRecord>) -> Vec<String> {
    let mut names: Vec<_> = records.into_iter().map(|record| record.name).collect();
    names.sort();
    names
}

#[test]
fn a_request_is_served_by_its_live_launch_and_an_ended_one_starts_the_next() {
    let fixture = TerminalsFixture::default();
    let request = TerminalsFixture::agent("v1", "lead", "env-a");
    let first = fixture.request(&request);
    assert_eq!(launched(&first), "terminal-v1-lead");
    assert_eq!(fixture.record(&first).reading, LifecycleReading::InProgress);
    assert_eq!(fixture.request(&request), first, "a live launch serves the request");

    // One session is one environment's and one holder's.
    let elsewhere = TerminalsFixture::agent("v1", "lead", "env-b");
    assert!(fixture.terminals.request(elsewhere).is_err());

    fixture.ready(&first, "terminal-v1-lead", &facts());
    fixture.exited(&first, "exit 1");
    let second = fixture.request(&request);
    assert_ne!(second, first);
    assert_eq!(launched(&second), "terminal-v1-lead");
    assert!(fixture.record(&second).launch_number() > fixture.record(&first).launch_number());
    // The session's name reads its newest launch; a launch's name reads that launch.
    assert_eq!(fixture.terminals.get("terminal-v1-lead", None).expect("read").expect("held").name, second);
    assert!(fixture.record(&first).ended());
}

#[test]
fn a_realised_launch_reads_back_with_its_facts_through_each_index() {
    let fixture = TerminalsFixture::default();
    let lead = fixture.request(&TerminalsFixture::agent("v1", "lead", "env-a"));
    let review = fixture.request(&TerminalsFixture::agent("v1", "review", "env-a"));
    let other = fixture.request(&TerminalsFixture::agent("v2", "lead", "env-b"));
    fixture.ready(&lead, "terminal-v1-lead", &facts());

    let record = fixture.record(&lead);
    assert!(record.is_running());
    assert!(record.is_agent());
    assert_eq!(record.session_id(), Some("terminal-v1-lead"));
    assert_eq!(record.facts, Some(facts()));
    assert_eq!(record.vessel.as_deref(), Some("v1"));
    assert_eq!(record.crew_id(), Some(lead.as_str()), "a crew id is its launch name");
    assert!(record.started_at().is_some());
    // Not realised yet: no facts.
    assert_eq!(fixture.record(&review).facts, None);

    assert_eq!(names(fixture.terminals.of_vessel("v1").expect("of vessel")), names(vec![fixture.record(&lead), fixture.record(&review)]));
    assert_eq!(names(fixture.terminals.of_environment("env-b", "home").expect("of environment")), vec![other.clone()]);
    assert_eq!(names(fixture.terminals.at("home").expect("at")).len(), 3);
    assert_eq!(names(fixture.terminals.every().expect("every")).len(), 3);
    // Served through the engine's readings.
    assert!(fixture.reads.terminals().expect("served").get(&other, None).expect("read").is_some());
}

#[test]
fn a_renewal_starts_the_next_launch_only_once_the_newest_ended() {
    let fixture = TerminalsFixture::default();
    let first = fixture.request(&TerminalsFixture::agent("v1", "lead", "env-a"));
    fixture.ready(&first, "terminal-v1-lead", &facts());
    match fixture.terminals.renew("terminal-v1-lead").expect("renew") {
        TerminalRenewal::Live(record) => assert_eq!(record.name, first),
        TerminalRenewal::Started(name) => panic!("a live launch is not renewed: {name}"),
    }

    fixture.release(&first);
    let TerminalRenewal::Started(second) = fixture.terminals.renew("terminal-v1-lead").expect("renew") else {
        panic!("an ended launch is renewed")
    };
    assert_ne!(second, first);
    let renewed = fixture.record(&second);
    assert_eq!(renewed.environment, "env-a");
    assert_eq!(renewed.vessel.as_deref(), Some("v1"));
    assert_eq!(renewed.facts, None, "facts are each launch's own");

    let newest = newest_launches(fixture.terminals.every().expect("every"));
    assert_eq!(newest.len(), 1);
    assert_eq!(newest.values().next().expect("one").name, second);
}

#[test]
fn a_session_never_requested_here_cannot_be_renewed() {
    let fixture = TerminalsFixture::default();
    assert!(fixture.terminals.renew("terminal-v9-lead").is_err());
    assert!(fixture.terminals.get("terminal-v9-lead", None).expect("read").is_none());
}

// Behaviour (#3108): the old store's TerminalSession reads are served from the launches. Each
// session reads as its newest launch, with the engine's identity, phase and ending reason, and
// its writes are refused: sessions are requested and renewed through the terminal port. Its
// live Vessel launch holds an ended session for its resume; once the vessel ends too, nothing
// holds it and it retires: absent from the listing and from a read by name (#4421).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_store_reads_see_each_sessions_newest_launch() {
    use std::sync::Arc;

    use flotilla_provisioning::engine::reads::{LifecycleReadPort, StoreReads};
    use flotilla_provisioning_testkit::VesselsFixture;
    use flotilla_resources::{terminal_records::TerminalRecords, vessel_records::VesselRecords, TerminalSession, TerminalSessionPhase};
    use flotilla_store::{ResourceBackend, TerminalViews};

    let fixture = TerminalsFixture::default();
    let vessels = VesselsFixture::over(&fixture.home, Arc::clone(&fixture.store));
    let vessel = vessels.request("c1", "v1", "host-a");
    let reads = Arc::new(
        LifecycleReadPort::new(StoreReads::new(Arc::clone(&fixture.store)), fixture.home.clone())
            .with_terminals(Arc::clone(&fixture.terminals) as Arc<dyn TerminalRecords>)
            .with_vessels(Arc::clone(&vessels.vessels) as Arc<dyn VesselRecords>),
    );
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new("flotilla", reads)));
    backend.serve_engine_in("flotilla");
    let sessions = backend.using::<TerminalSession>("flotilla");
    let request = flotilla_resources::terminal_records::TerminalLaunchRequest {
        vessel: Some(vessel.clone()),
        ..TerminalsFixture::agent("v1", "lead", "env-a")
    };
    let read = || async { sessions.get(&request.session).await.expect("session").status.expect("status") };

    let first = fixture.request(&request);
    assert_eq!(read().await.phase, TerminalSessionPhase::Starting);
    fixture.ready(&first, "engine identity", &facts());
    let running = read().await;
    assert_eq!(running.phase, TerminalSessionPhase::Running);
    assert_eq!(running.session_id.as_deref(), Some("engine identity"));
    fixture.exited(&first, "agent exited with code 0");
    assert_eq!(read().await.phase, TerminalSessionPhase::Stopped, "an exited agent waits for its resume");

    let TerminalRenewal::Started(second) = fixture.terminals.renew(&request.session).expect("renew") else { panic!("next launch") };
    assert_eq!(read().await.phase, TerminalSessionPhase::Starting, "the newest launch is read");
    fixture.ready(&second, "s2", &facts());
    fixture.failed(&second, "engine ending reason");
    let failed = read().await;
    assert_eq!(failed.phase, TerminalSessionPhase::Failed);
    assert!(failed.message.expect("reason").contains("engine ending reason"));

    assert_eq!(sessions.list().await.expect("list").items.len(), 1, "its live vessel holds the ended session");

    // The vessel ends: nothing holds the failed launch, so it retires.
    vessels.step(&vessel, flotilla_resources::vessel_records::VesselStep::Failed { cause: "no capacity".into(), retryable: false });
    assert!(sessions.list().await.expect("list").items.is_empty(), "an unheld ended session leaves the listing");
    assert!(sessions.get(&request.session).await.is_err(), "and reads by name as absent");
    assert!(sessions.delete(&request.session).await.is_err(), "a session ends with its holder; it is not deleted");
}

// Behaviour (#3108): a session's view says when its environment cannot serve it, from the
// Environment's own record: not ready while the launch waits on it, or ended under a running
// launch (explained as the provider being unavailable; terminal_health's tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sessions_view_explains_an_environment_that_cannot_serve_it() {
    use std::sync::Arc;

    use flotilla_provisioning_testkit::EnvironmentFixture;
    use flotilla_resources::{environment_records::LaunchFacts, EnvironmentSpec, HostDirectEnvironmentSpec, TerminalSession};
    use flotilla_store::{ResourceBackend, TerminalViews};

    let environments = EnvironmentFixture::at("home");
    let terminals = TerminalsFixture::over("home", Arc::clone(&environments.store));
    let environments = environments.serving_terminals(terminals.terminals.clone() as Arc<dyn TerminalRecords>);
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new("flotilla", environments.reads.clone())));
    let status = || async {
        let session = backend.using::<TerminalSession>("flotilla").get("terminal-work-coder").await.expect("session view");
        session.status.expect("status")
    };

    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "home".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    environments.request("env", &spec, None);
    let launch = terminals.request(&TerminalsFixture::agent("work", "coder", "env"));
    // Waiting on an environment still being realised.
    assert_eq!(status().await.environment_unavailable.as_deref(), Some("environment env: being realised"));

    environments.ready("env", "backing", LaunchFacts::default());
    assert_eq!(status().await.environment_unavailable, None, "a ready environment serves a waiting launch");
    terminals.ready(&launch, "crew-session", &facts());
    assert_eq!(status().await.environment_unavailable, None, "a ready environment serves a running launch");

    environments.lose("env", "cgroup_oom");
    assert_eq!(status().await.environment_unavailable.as_deref(), Some("environment env: cgroup_oom"));
}

/// The next change a watcher of the sessions hears: what happened, to which session, and its phase.
async fn next_session(
    watch: &mut futures::stream::BoxStream<
        'static,
        Result<flotilla_resources::ReadWatchEvent<flotilla_resources::TerminalSession>, flotilla_resources::ResourceError>,
    >,
) -> (&'static str, String, Option<flotilla_resources::TerminalSessionPhase>) {
    use flotilla_resources::ReadWatchEvent;
    use futures::StreamExt;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next()).await.expect("a change").expect("watching");
    match event.expect("read") {
        ReadWatchEvent::Added(read) => ("added", read.object.metadata.name, read.object.status.map(|status| status.phase)),
        ReadWatchEvent::Modified(read) => ("modified", read.object.metadata.name, read.object.status.map(|status| status.phase)),
        ReadWatchEvent::Deleted(read) => ("deleted", read.object.metadata.name, None),
        ReadWatchEvent::DeletedByName { tombstone, .. } => ("deleted", tombstone.name, None),
    }
}

/// The next change a watcher of the sessions hears, added or modified, and its version.
async fn next_version(
    watch: &mut futures::stream::BoxStream<
        'static,
        Result<flotilla_resources::ReadWatchEvent<flotilla_resources::TerminalSession>, flotilla_resources::ResourceError>,
    >,
) -> (&'static str, u64) {
    use flotilla_resources::ReadWatchEvent;
    use futures::StreamExt;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next()).await.expect("a change").expect("watching");
    let (told, read) = match event.expect("read") {
        ReadWatchEvent::Added(read) => ("added", read),
        ReadWatchEvent::Modified(read) => ("modified", read),
        other => panic!("not an added or modified session: {other:?}"),
    };
    (told, read.object.metadata.resource_version.parse().expect("a version"))
}

// Behaviour (#3108): a session's next launch in place of one that ended is a Modified change at a
// version past every version the ended launch was told at, though the new launch's own record has
// been written once and the ended one three times: a view's version follows how far the log was
// read, never the record it shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sessions_next_launch_moves_its_version_past_the_last() {
    use std::sync::Arc;

    use flotilla_resources::{environment_records::LaunchFacts, EnvironmentSpec, HostDirectEnvironmentSpec, TerminalSession};
    use flotilla_store::{ResourceBackend, TerminalViews};

    let store = flotilla_provisioning_testkit::OtherHome::open("home");
    let engine = flotilla_provisioning_testkit::EngineFixture::over_service(store.service());
    let _followers = [
        engine.terminals.terminals.follow(Arc::clone(store.service())).expect("follow sessions"),
        engine.environments.environments.follow(Arc::clone(store.service())).expect("follow environments"),
        engine.vessels.vessels.follow(Arc::clone(store.service())).expect("follow vessels"),
    ];
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new("flotilla", engine.reads())));
    backend.serve_engine_in("flotilla");
    let mut watch = backend.including_replicas::<TerminalSession>("flotilla").watch().await.expect("watch sessions");

    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "home".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    engine.environments.request("env", &spec, None);
    engine.environments.ready("env", "backing", LaunchFacts::default());
    let vessel = engine.vessels.request("c", "work", "host");
    let request = TerminalsFixture::agent(&vessel, "coder", "env");
    let first = engine.terminals.request(&request);
    let (told, requested) = next_version(&mut watch).await;
    assert_eq!(told, "added");
    engine.terminals.ready(&first, "sid", &facts());
    let (_, running) = next_version(&mut watch).await;
    engine.terminals.exited(&first, "agent exited");
    let (_, stopped) = next_version(&mut watch).await;
    assert!(requested < running && running < stopped, "each change of the launch moves the version on");

    let TerminalRenewal::Started(_) = engine.terminals.terminals.renew(&request.session).expect("renew") else { panic!("next launch") };
    let (told, starting) = next_version(&mut watch).await;
    assert_eq!(told, "modified", "the next launch is a change of the session");
    assert!(starting > stopped, "the next launch's version {starting} is past the ended one's {stopped}");
}

// Behaviour (#3108): the session listing a watcher reads keeps up with each change its passes
// compose: a launch's edges, its environment's end, its next launch in its place, and its release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_listing_follows_each_launch_and_its_environment() {
    use std::sync::Arc;

    use flotilla_resources::{
        environment_records::LaunchFacts, EnvironmentSpec, HostDirectEnvironmentSpec, TerminalSession, TerminalSessionPhase,
    };
    use flotilla_store::{ResourceBackend, TerminalViews};

    let store = flotilla_provisioning_testkit::OtherHome::open("home");
    let engine = flotilla_provisioning_testkit::EngineFixture::over_service(store.service());
    let _followers = [
        engine.terminals.terminals.follow(Arc::clone(store.service())).expect("follow sessions"),
        engine.environments.environments.follow(Arc::clone(store.service())).expect("follow environments"),
        engine.vessels.vessels.follow(Arc::clone(store.service())).expect("follow vessels"),
    ];
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new("flotilla", engine.reads())));
    backend.serve_engine_in("flotilla");
    let sessions = backend.including_replicas::<TerminalSession>("flotilla");
    let mut watch = sessions.watch().await.expect("watch sessions");
    let listed = || async { sessions.list().await.expect("list").items.into_iter().map(|read| read.object).collect::<Vec<_>>() };

    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "home".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    engine.environments.request("env", &spec, None);
    engine.environments.ready("env", "backing", LaunchFacts::default());
    let vessel = engine.vessels.request("c", "work", "host");
    let request = TerminalsFixture::agent(&vessel, "coder", "env");
    let first = engine.terminals.request(&request);
    assert_eq!(next_session(&mut watch).await, ("added", request.session.clone(), Some(TerminalSessionPhase::Starting)));
    engine.terminals.ready(&first, "sid", &facts());
    assert_eq!(next_session(&mut watch).await, ("modified", request.session.clone(), Some(TerminalSessionPhase::Running)));
    let shown = listed().await;
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].metadata.labels.get(flotilla_resources::VESSEL_LABEL).map(String::as_str), Some("work"), "joined with its holder");

    // Its environment's end is composed into its view, though the launch did not change.
    engine.environments.lose("env", "cgroup_oom");
    assert_eq!(next_session(&mut watch).await.0, "modified");
    let shown = listed().await;
    assert_eq!(shown[0].status.as_ref().and_then(|status| status.environment_unavailable.as_deref()), Some("environment env: cgroup_oom"));

    // The session's next launch is read in place of the one that exited.
    engine.terminals.exited(&first, "agent exited");
    assert_eq!(next_session(&mut watch).await, ("modified", request.session.clone(), Some(TerminalSessionPhase::Stopped)));
    let TerminalRenewal::Started(second) = engine.terminals.terminals.renew(&request.session).expect("renew") else {
        panic!("next launch")
    };
    assert_eq!(next_session(&mut watch).await, ("modified", request.session.clone(), Some(TerminalSessionPhase::Starting)));

    // A released launch leaves the listing.
    engine.terminals.release(&second);
    assert_eq!(next_session(&mut watch).await, ("deleted", request.session.clone(), None));
    assert!(listed().await.is_empty());
}

// Behaviour (d15): a listing read made while no commit has landed since the session listing last
// passed reads it as published: it reads no store, so it waits on no pass. A writer still reads its
// commit back at its next read, without waiting for any follower, and an observation at the next
// read after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_read_with_no_commit_since_the_last_pass_reads_no_store() {
    use std::sync::Arc;

    use flotilla_resources::{
        environment_records::LaunchFacts, EnvironmentSpec, HostDirectEnvironmentSpec, TerminalOccupancy, TerminalSession,
        TerminalSessionPhase,
    };
    use flotilla_store::{load_metrics::measure, ResourceBackend, TerminalViews};

    let store = flotilla_provisioning_testkit::OtherHome::open("home");
    let engine = flotilla_provisioning_testkit::EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new("flotilla", engine.reads())));
    backend.serve_engine_in("flotilla");
    let sessions = backend.including_replicas::<TerminalSession>("flotilla");
    let listed = || async { sessions.list().await.expect("list").items.into_iter().map(|read| read.object).collect::<Vec<_>>() };

    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "home".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    engine.environments.request("env", &spec, None);
    engine.environments.ready("env", "backing", LaunchFacts::default());
    let vessel = engine.vessels.request("c", "work", "host");
    let request = TerminalsFixture::agent(&vessel, "coder", "env");
    let first = engine.terminals.request(&request);
    assert_eq!(listed().await.len(), 1, "the writer reads its request back");

    let (shown, reads) = measure(listed()).await;
    assert_eq!(shown.len(), 1);
    let stored: Vec<_> = reads
        .iter()
        .filter(|(kind, counts)| !kind.ends_with(" port") && counts.keyed + counts.index + counts.whole_kind > 0)
        .map(|(kind, _)| kind.clone())
        .collect();
    assert!(stored.is_empty(), "a read with nothing committed since the last pass reads no store, but read {stored:?}");

    engine.terminals.ready(&first, "sid", &facts());
    let shown = listed().await;
    assert_eq!(
        shown[0].status.as_ref().map(|status| status.phase),
        Some(TerminalSessionPhase::Running),
        "the writer reads its commit back"
    );

    let observed = engine.terminals.terminals.observations().update(&first, |observation| {
        observation.occupancy = TerminalOccupancy::Occupied;
    });
    assert!(observed);
    let shown = listed().await;
    let occupancy = shown[0].status.as_ref().map(|status| status.occupancy);
    assert_eq!(occupancy, Some(TerminalOccupancy::Occupied), "an observation is read at the next read");
}

// Behaviour (d16 finding 1): a read made by anyone who has learned of a commit sees it. Here the
// read is made the moment the store tells its listeners of a session's realise, as a watcher woken
// by the commit reads, from a listener told before the views' own. It reads the session realised,
// never the listing published before the realise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_told_of_a_realise_commit_reads_the_session_realised() {
    use std::{
        sync::{mpsc, Arc, Mutex},
        time::Duration,
    };

    use flotilla_resources::{
        environment_records::LaunchFacts, EnvironmentSpec, HostDirectEnvironmentSpec, TerminalSession, TerminalSessionPhase,
    };
    use flotilla_schema_sqlite::stream::{CommitListener, Committed};
    use flotilla_store::{ResourceBackend, TerminalViews};

    type Read = Box<dyn FnOnce() + Send>;

    async fn phase_of(backend: &ResourceBackend, session: &str) -> Option<TerminalSessionPhase> {
        let listed = backend.including_replicas::<TerminalSession>("flotilla").list().await.expect("list");
        listed.items.into_iter().find(|read| read.object.metadata.name == session)?.object.status.map(|status| status.phase)
    }

    let store = flotilla_provisioning_testkit::OtherHome::open("home");
    // Listening before the views start: told of each commit before their cursors are.
    let armed: Arc<Mutex<Option<Read>>> = Arc::default();
    let listener: Arc<CommitListener> = {
        let armed = Arc::clone(&armed);
        Arc::new(move |_: &Committed<'_>| {
            let read = armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            if let Some(read) = read {
                read();
            }
        })
    };
    store.service().on_commit(Arc::downgrade(&listener));
    let engine = flotilla_provisioning_testkit::EngineFixture::over_service(store.service());
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new("flotilla", engine.reads())));
    backend.serve_engine_in("flotilla");

    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "home".into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    };
    engine.environments.request("env", &spec, None);
    engine.environments.ready("env", "backing", LaunchFacts::default());
    let runtime = tokio::runtime::Handle::current();
    for n in 0..8 {
        let vessel = engine.vessels.request(&format!("c{n}"), "work", "host");
        let launch = engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", "env"));
        let session = launched(&launch).to_owned();
        let requested = phase_of(&backend, &session).await;
        assert!(requested.is_some_and(|phase| phase != TerminalSessionPhase::Running), "requested, not yet realised: {requested:?}");

        let (sent, read) = mpsc::channel();
        let (backend_, runtime_, session_) = (backend.clone(), runtime.clone(), session.clone());
        let reader: Read = Box::new(move || {
            let (done, finished) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = sent.send(runtime_.block_on(phase_of(&backend_, &session_)));
                let _ = done.send(());
            });
            // A read served without the store's lock answers at once; one that reads the store
            // waits for this commit to return.
            let _ = finished.recv_timeout(Duration::from_millis(300));
        });
        *armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reader);
        engine.terminals.ready(&launch, "sid", &facts());

        let phase = tokio::task::spawn_blocking(move || read.recv_timeout(Duration::from_secs(30)).expect("the read answers"))
            .await
            .expect("the read");
        assert_eq!(phase, Some(TerminalSessionPhase::Running), "a read told of the realise reads the session realised");
    }
}
