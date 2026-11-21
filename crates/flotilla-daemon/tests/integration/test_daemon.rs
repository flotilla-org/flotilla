//! Behaviour through the public daemon harness (#4321): each scenario starts
//! a daemon as `flotillad` composes one and touches only what an operator, a
//! crew or the environment vendor touches.

use flotilla_daemon_testkit::daemon::{TestDaemon, NAMESPACE};
use flotilla_dummy_fleet::{
    catalog::{CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND},
    DummyVendor,
};
use flotilla_protocol::{
    commands::{AttachMode, CommandValue, CrewSupervisionAction},
    table::Row,
    AgentActivity, AgentSessionReading, CommandAction, QueryId, ResultSet,
};
use flotilla_resources::{
    ChangeRequest, Convoy, ConvoyPhase, ObservedChangeRequestState, ResourceObject, TerminalSession, TerminalSessionPhase,
};

fn text<'a>(row: &'a Row, column: &str) -> &'a str {
    row.get(column).and_then(serde_json::Value::as_str).unwrap_or_default()
}

/// An environment held for one of `convoy`'s vessels, in `state`.
fn of_convoy<'a>(convoy: &'a str, state: &'a str) -> impl Fn(&Row) -> bool + 'a {
    move |row| {
        let holder = text(row, "holder");
        let vessel = holder.rsplit([':', '/']).next().unwrap_or(holder);
        // A vessel is a launch of `<convoy launch_of>-<work>`.
        vessel.starts_with(&format!("{}-", flotilla_resources::convoy::launch_of(convoy))) && text(row, "state") == state
    }
}

fn in_phase(convoy: &str, phase: ConvoyPhase) -> impl Fn(&ResourceObject<Convoy>) -> bool + '_ {
    move |found| found.metadata.name == convoy && found.status.as_ref().is_some_and(|status| status.phase == phase)
}

// A daemon's time is virtual: it moves only while nothing runs (#4385). A blocking-pool job,
// such as the engine store's, is work the runtime cannot see run, and it counts as running: a
// timer due while it works fires only once it has ended, at the time it was due.
#[tokio::test(start_paused = true)]
async fn a_timer_due_during_blocking_work_fires_only_once_the_work_ends() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let _daemon = TestDaemon::builder().start().await;
    let ended = Arc::new(AtomicBool::new(false));
    let started = tokio::time::Instant::now();
    let timer = tokio::spawn({
        let ended = Arc::clone(&ended);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            (ended.load(Ordering::SeqCst), started.elapsed())
        }
    });
    let work = tokio::task::spawn_blocking({
        let ended = Arc::clone(&ended);
        move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            ended.store(true, Ordering::SeqCst);
        }
    });
    work.await.expect("the blocking work");
    let (after_the_work, at) = timer.await.expect("the timer");
    assert!(after_the_work, "the timer fired while the blocking work ran");
    assert!(at <= std::time::Duration::from_millis(2), "the timer fired {at:?} after it was set, not when it was due");
}

// A cloud sandbox the vendor deletes, or stops with no resume available, is
// Lost: the daemon learns it only from what the vendor reports, and no stored
// record is touched to make it so; the Lost status is written only as the
// engine's answer.
#[tokio::test(start_paused = true)]
async fn a_cloud_sandbox_the_vendor_deletes_becomes_lost() {
    cloud_sandbox_becomes_lost(DummyVendor::vanish).await;
}

#[tokio::test(start_paused = true)]
async fn a_cloud_sandbox_the_vendor_stops_becomes_lost() {
    cloud_sandbox_becomes_lost(DummyVendor::stop).await;
}

async fn cloud_sandbox_becomes_lost(vendor: fn(&DummyVendor, &str) -> bool) {
    let daemon = TestDaemon::builder().start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_CLOUD_FULFILMENT_KIND).await;
    let ready = daemon.wait_row("a Realised cloud sandbox", "environment", "", of_convoy(&convoy, "Realised")).await;
    // A cloud sandbox's recipe sizes the sandbox.
    assert!(ready.get("recipe").and_then(|recipe| recipe.get("size")).is_some(), "{ready:?}");
    let name = text(&ready, "resource.name").to_owned();
    assert!(vendor(daemon.vendor(), &name), "{ready:?}");
    daemon
        .wait_row("Lost", "environment", "", |row| {
            text(row, "resource.name") == name && text(row, "state") == "Gone" && text(row, "ending") == "Lost"
        })
        .await;
}

// Attach is offered only while a session's environment is live on the engine (owner ruling): once
// the vendor deletes the crew's backing and the probe ends it Lost, attaching to the crew's
// session is refused for that reason and its vessel row offers no attach. The dummy fleet's
// containers offer no terminal route, so before the loss attach is refused for that instead.
#[tokio::test(start_paused = true)]
async fn a_lost_environment_withdraws_its_sessions_attach() {
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 60000").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let ready = daemon.wait_row("a Realised environment", "environment", "", of_convoy(&convoy, "Realised")).await;
    let name = text(&ready, "resource.name").to_owned();
    let vessel_row = |set: &ResultSet| {
        set.rows
            .as_convoys()
            .and_then(|rows| rows.iter().find(|row| row.resource.name == convoy))
            .and_then(|row| row.vessels.first().cloned())
    };
    let launched = daemon
        .wait_query(QueryId::Convoys { scope: None }, |set| {
            vessel_row(set).is_some_and(|vessel| vessel.crew.iter().any(|crew| crew.session.is_some()))
        })
        .await;
    // The row names the session by address; attach takes its name.
    let address = vessel_row(&launched).and_then(|vessel| vessel.crew.into_iter().find_map(|crew| crew.session)).expect("session");
    let session = address.rsplit('/').next().unwrap_or(&address).to_owned();
    let attach = || CommandAction::Attach { reference: session.clone(), host: None, mode: AttachMode::default() };
    let lost = format!("environment {name} is Lost");
    let before = daemon.execute(attach()).await;
    assert!(!matches!(&before, CommandValue::Error { message } if message.contains(&lost)), "{before:?}");
    assert!(DummyVendor::vanish(daemon.vendor(), &name), "{ready:?}");
    daemon
        .wait_row("Lost", "environment", "", |row| {
            text(row, "resource.name") == name && text(row, "state") == "Gone" && text(row, "ending") == "Lost"
        })
        .await;
    let refused = daemon.execute(attach()).await;
    let CommandValue::Error { message } = &refused else { panic!("attach to {session} on a lost environment: {refused:?}") };
    assert!(message.contains(&lost), "{message}");
    daemon.wait_query(QueryId::Convoys { scope: None }, |set| vessel_row(set).is_some_and(|vessel| vessel.materialize.is_none())).await;
}

// A vessel whose backing the vendor deletes mid-exchange is relaunched, and its role passes to
// the new launch's session, so the governor's turns reach the new aide and the convoy lands. The
// governor's handoffs in the gap are refused until the next launch replaces the lost one, so it
// stalls, and the operator resumes it once the new aide runs.
#[tokio::test(start_paused = true)]
async fn a_relaunched_vessels_role_passes_to_its_new_session() {
    the_aides_role_passes_to_its_relaunch(false).await;
}

// As above, with the daemon restarting just after the loss, as b0's did: the restarted daemon
// finds the backing gone at adoption and relaunches the vessel.
#[tokio::test(start_paused = true)]
async fn a_vessel_lost_across_a_restart_passes_its_role_to_its_relaunch() {
    the_aides_role_passes_to_its_relaunch(true).await;
}

async fn the_aides_role_passes_to_its_relaunch(restart: bool) {
    let daemon = TestDaemon::builder().start().await;
    let convoy = daemon.dispatch(CrewMix::LongRunner, DUMMY_FULFILMENT_KIND).await;
    daemon
        .wait("both crews exchanging turns", |_: &ResourceObject<TerminalSession>| {
            daemon.counts().sessions_launched == 2 && daemon.counts().turns_received >= 2
        })
        .await;
    let assist = |row: &Row| of_convoy(&convoy, "Realised")(row) && text(row, "holder").contains("-aide#");
    let ready = daemon.wait_row("the aide's Realised environment", "environment", "", assist).await;
    let name = text(&ready, "resource.name").to_owned();
    assert!(DummyVendor::vanish(daemon.vendor(), &name), "{ready:?}");
    let daemon = if restart { daemon.restart().await } else { daemon };
    // A lost launch is replaced only after the provisioning backoff, 30 s of wall time.
    let waiting = daemon
        .wait("the aide relaunched", |found: &ResourceObject<Convoy>| {
            found.metadata.name == convoy
                && found.status.as_ref().is_some_and(|status| status.stalled.is_some() || status.phase == ConvoyPhase::Landed)
                && daemon.counts().sessions_launched == 3
        })
        .await;
    if waiting.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed) {
        return;
    }
    let resume = CommandAction::CrewSupervise {
        namespace: Some(NAMESPACE.into()),
        convoy: convoy.clone(),
        vessel: "governor".into(),
        role: "governor".into(),
        operation: CrewSupervisionAction::Resume,
        message: "The aide is back.".into(),
        actor_crew_id: None,
    };
    let resumed = daemon.execute(resume).await;
    assert!(!matches!(resumed, CommandValue::Error { .. }), "{resumed:?}");
    daemon.wait("Landed", in_phase(&convoy, ConvoyPhase::Landed)).await;
}

// A crew member's row shows its AgentSession as the decided store holds it (#3108), joined by
// the TerminalSession's record name. The crew reports working, then idle once its work is
// done, as a harness's hooks do; each is written after the view began following the store, so
// the row reaching it shows the view woke on the store's commit. Its change request is not
// merged while the scenario runs, so the convoy waits on it, idle, rather than landing at once.
#[tokio::test(start_paused = true)]
async fn a_crew_members_row_follows_its_agent_session() {
    let daemon = TestDaemon::builder().change_request("merge_after_ms = 86400000").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let reading = |set: &ResultSet| -> Option<AgentSessionReading> {
        let row = set.rows.as_convoys()?.iter().find(|row| row.resource.name == convoy)?;
        row.vessels.iter().flat_map(|vessel| &vessel.crew).find_map(|crew| crew.agent_session.clone())
    };
    let activity = |set: &ResultSet, activity: AgentActivity| reading(set).is_some_and(|reading| reading.activity == Some(activity));
    let working = daemon.wait_query(QueryId::Convoys { scope: None }, |set| activity(set, AgentActivity::Working)).await;
    let working = reading(&working).expect("working reading");
    assert_eq!(working.ended, None, "{working:?}");
    assert_eq!(working.activity_cause.as_deref(), Some("UserPromptSubmit"), "{working:?}");
    let idle = daemon.wait_query(QueryId::Convoys { scope: None }, |set| activity(set, AgentActivity::Idle)).await;
    let idle = reading(&idle).expect("idle reading");
    assert_eq!(idle.activity_cause.as_deref(), Some("Stop"), "{idle:?}");
    assert!(idle.activity_since > working.activity_since, "{working:?} then {idle:?}");
}

// Another home's crew shows its AgentSession here too: the session is written at its home, and
// this daemon reads the replica its decided store follows from that home's log, joined to the
// replicated TerminalSession by home and record name. A stalled crew waits idle for guidance, so
// its reading holds still while the peers connect.
#[tokio::test(start_paused = true)]
async fn a_crew_members_row_shows_its_agent_session_at_another_home() {
    let (a, b) = tokio::join!(
        TestDaemon::builder().agent("stall_rate = 1.0").settings("host_name = \"agents-a\"\n").start(),
        TestDaemon::builder().agent("stall_rate = 1.0").settings("host_name = \"agents-b\"\n").start()
    );
    let convoy = a.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let reading = |set: &ResultSet| -> Option<AgentSessionReading> {
        let row = set.rows.as_convoys()?.iter().find(|row| row.resource.name == convoy)?;
        row.vessels.iter().flat_map(|vessel| &vessel.crew).find_map(|crew| crew.agent_session.clone())
    };
    let idle = |set: &ResultSet| reading(set).is_some_and(|reading| reading.activity == Some(AgentActivity::Idle));
    let at_home = reading(&a.wait_query(QueryId::Convoys { scope: None }, idle).await).expect("reading at its home");
    let _peering = TestDaemon::peer(&a, &b).await;
    let elsewhere = b.wait_query(QueryId::Convoys { scope: None }, |set| reading(set).is_some_and(|reading| reading == at_home)).await;
    assert_eq!(reading(&elsewhere), Some(at_home));
}

// Once the peering drops, another home's crew reading goes stale here at the moment it stops
// being current: HOME_HEARD_WITHIN after this daemon last heard from that home. Nothing else
// happens then, so the row changing shows the reader's one deadline wake. The daemons' clock
// is the runtime's virtual time, so the clock and the deadline sleep move together.
#[tokio::test(start_paused = true)]
async fn a_crew_members_row_goes_stale_at_its_deadline_once_its_home_is_unheard() {
    use flotilla_resources::{agent_activity::HOME_HEARD_WITHIN, clock::now};
    let (a, b) = tokio::join!(
        TestDaemon::builder().agent("stall_rate = 1.0").settings("host_name = \"agents-a\"\n").start(),
        TestDaemon::builder().agent("stall_rate = 1.0").settings("host_name = \"agents-b\"\n").start()
    );
    let convoy = a.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let reading = |set: &ResultSet| -> Option<AgentSessionReading> {
        let row = set.rows.as_convoys()?.iter().find(|row| row.resource.name == convoy)?;
        row.vessels.iter().flat_map(|vessel| &vessel.crew).find_map(|crew| crew.agent_session.clone())
    };
    let idle = |set: &ResultSet| reading(set).is_some_and(|reading| reading.activity == Some(AgentActivity::Idle));
    let at_home = reading(&a.wait_query(QueryId::Convoys { scope: None }, idle).await).expect("reading at its home");
    let peering = TestDaemon::peer(&a, &b).await;
    b.wait_query(QueryId::Convoys { scope: None }, |set| reading(set).is_some_and(|reading| reading == at_home)).await;

    drop(peering);
    // The last keepalive it heard came at most one held round before the drop.
    let dropped = now();
    let stale = b.wait_query(QueryId::Convoys { scope: None }, |set| reading(set).is_some_and(|reading| reading.stale)).await;
    let stale_after = now() - dropped;
    assert!(
        stale_after <= HOME_HEARD_WITHIN && stale_after > HOME_HEARD_WITHIN - chrono::Duration::seconds(15),
        "stale {stale_after} after the drop"
    );
    assert_eq!(reading(&stale).map(|reading| AgentSessionReading { stale: false, ..reading }), Some(at_home), "only its freshness changed");
}

// A restarted daemon finds its running vessels again: the vendor's backings and
// the crew's process survive, and the convoy lands with nothing provisioned or
// launched twice.
#[tokio::test(start_paused = true)]
async fn a_restarted_daemon_adopts_its_running_vessels() {
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 5000").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let environment = daemon.wait_row("a Realised environment", "environment", "", of_convoy(&convoy, "Realised")).await;
    let name = text(&environment, "resource.name").to_owned();
    daemon.wait("a launched coder", |_: &ResourceObject<Convoy>| daemon.counts().sessions_launched == 1).await;
    let daemon = daemon.restart().await;
    daemon.wait("Landed", in_phase(&convoy, ConvoyPhase::Landed)).await;
    let counts = daemon.counts();
    assert_eq!((counts.sessions_launched, counts.environments_provisioned, counts.command_errors), (1, 1, 0), "{counts:?}");
    // The adopted backing is the one torn down once the convoy lands.
    daemon
        .wait_row("its environment Gone", "environment", "", |row| text(row, "resource.name") == name && text(row, "state") == "Gone")
        .await;
    assert_eq!(daemon.counts().environments_destroyed, 1);
}

// A convoy idle on a change request that is not changing is not stalled on its observer (run 5
// R5-1): the observer's reads keep confirming the record though they write nothing, and the stall
// judge counts those confirmations, before a restart and after it, when the record's last write
// is older than the observer's staleness bound.
#[tokio::test(start_paused = true)]
async fn a_convoy_idle_on_an_unchanged_change_request_is_not_stalled_across_a_restart() {
    // The change request is never merged while the scenario runs.
    let daemon = TestDaemon::builder().change_request("merge_after_ms = 86400000").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    daemon
        .wait("its open change request", |found: &ResourceObject<ChangeRequest>| {
            found.status.as_ref().is_some_and(|status| status.state.value == Some(ObservedChangeRequestState::Open))
        })
        .await;
    let reading = |set: &ResultSet| -> Option<AgentSessionReading> {
        let row = set.rows.as_convoys()?.iter().find(|row| row.resource.name == convoy)?;
        row.vessels.iter().flat_map(|vessel| &vessel.crew).find_map(|crew| crew.agent_session.clone())
    };
    daemon
        .wait_query(QueryId::Convoys { scope: None }, |set| {
            reading(set).is_some_and(|reading| reading.activity == Some(AgentActivity::Idle))
        })
        .await;
    // Past the staleness bound and the grace of the observer's next read, with nothing changing.
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    assert_unstalled(&daemon, &convoy, "idle before the restart").await;
    let daemon = daemon.restart().await;
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    assert_unstalled(&daemon, &convoy, "after the restart").await;
    // Most of half an hour more: on virtual time a wait this long costs only the daemon's passes.
    tokio::time::sleep(std::time::Duration::from_secs(25 * 60)).await;
    assert_unstalled(&daemon, &convoy, "idle after the restart").await;
}

/// `convoy` is still Active and has no stall, `when`.
async fn assert_unstalled(daemon: &TestDaemon, convoy: &str, when: &str) {
    let found = daemon.get::<Convoy>(convoy).await.expect("the convoy");
    let status = found.status.expect("convoy status");
    assert_eq!(status.phase, ConvoyPhase::Active, "{when}: {status:?}");
    assert!(status.stalls.is_empty() && status.stalled.is_none(), "{when}: stalled: {:?}", status.stalled);
}

// A vendor that refuses to provision fails the convoy before any crew launches;
// once it accepts again, the next convoy lands.
#[tokio::test(start_paused = true)]
async fn a_vendor_refusing_to_provision_fails_its_convoy() {
    let daemon = TestDaemon::builder().start().await;
    daemon.vendor().refuse_provisioning(true);
    let refused = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    daemon.wait("Failed", in_phase(&refused, ConvoyPhase::Failed)).await;
    assert_eq!((daemon.counts().environments_provisioned, daemon.counts().sessions_launched), (0, 0));
    daemon.vendor().refuse_provisioning(false);
    let accepted = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    daemon.wait("Landed", in_phase(&accepted, ConvoyPhase::Landed)).await;
}

/// A resource watch's records up to its bookmark, by name, and the bookmark's cursor.
async fn watched_snapshot(
    watch: &mut flotilla_client::resource::ResourceWatch,
) -> (std::collections::BTreeMap<String, serde_json::Value>, flotilla_protocol::ResourceCursor) {
    let mut records = std::collections::BTreeMap::new();
    loop {
        let envelope = watch.next().await.expect("the watch reads").expect("the watch is open");
        for record in envelope.records {
            match (record.record_type, record.object) {
                (flotilla_protocol::ResourceRecordType::Bookmark, _) => return (records, envelope.cursor),
                (_, Some(object)) => {
                    let name = object["metadata"]["name"].as_str().expect("a named record").to_owned();
                    records.insert(name, object);
                }
                (_, None) => {}
            }
        }
    }
}

// A client whose watch of an engine-served kind drops reconnects with its cursor: the resume names
// the update and the delete made in the gap, without a snapshot, and a fresh watch converges on
// the records as they are.
#[tokio::test(start_paused = true)]
async fn a_resource_watch_reconnecting_after_a_gap_converges_on_the_records() {
    use flotilla_client::resource::ResourceWatchRequest;
    use flotilla_resources::{FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec};

    let daemon = TestDaemon::builder().start().await;
    let base = daemon.get::<FulfilmentKind>(DUMMY_FULFILMENT_KIND).await.expect("the dummy fulfilment kind").spec;
    daemon.apply::<FulfilmentKind>("watched-kept", base.clone()).await;
    daemon.apply::<FulfilmentKind>("watched-gone", base.clone()).await;
    let request = || ResourceWatchRequest::builder().kind("FulfilmentKind".into()).namespace(NAMESPACE.into());

    let mut watch = daemon.resources().watch(request().build()).await.expect("watch");
    let (records, cursor) = watched_snapshot(&mut watch).await;
    assert!(records.contains_key("watched-kept") && records.contains_key("watched-gone"), "{:?}", records.keys());
    watch.cancel().await.expect("disconnect");

    let mut updated = base.clone();
    updated.grants.insert(FulfilmentGrant::network("watched".into()));
    daemon.apply::<FulfilmentKind>("watched-kept", updated.clone()).await;
    let deleted = daemon
        .execute(CommandAction::ResourceDelete {
            namespace: NAMESPACE.into(),
            kind: "FulfilmentKind".into(),
            name: "watched-gone".into(),
            replica_origin: None,
        })
        .await;
    assert!(matches!(deleted, CommandValue::ResourceDeleted(_)), "delete: {deleted:?}");

    let mut resumed = daemon.resources().watch(request().cursor(cursor).build()).await.expect("reconnect");
    let mut gap = std::collections::BTreeMap::new();
    while !(gap.contains_key("watched-kept") && gap.contains_key("watched-gone")) {
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(10), resumed.next())
            .await
            .expect("the gap's changes in time")
            .expect("the resume reads")
            .expect("the resume is open");
        for record in envelope.records {
            assert_ne!(record.record_type, flotilla_protocol::ResourceRecordType::Current, "a resume sends no snapshot");
            if let Some(object) = record.object {
                let name = object["metadata"]["name"].as_str().expect("a named record").to_owned();
                gap.insert(name, (record.record_type, object));
            }
        }
    }
    let (kept_type, kept) = gap.get("watched-kept").expect("the update made in the gap");
    assert_eq!(*kept_type, flotilla_protocol::ResourceRecordType::Modified);
    let kept: FulfilmentKindSpec = serde_json::from_value(kept["spec"].clone()).expect("spec");
    assert_eq!(kept.grants, updated.grants, "the resume carries the update");
    assert_eq!(gap.get("watched-gone").map(|(record_type, _)| *record_type), Some(flotilla_protocol::ResourceRecordType::Deleted));
    resumed.cancel().await.expect("cancel");

    let mut fresh = daemon.resources().watch(request().build()).await.expect("watch again");
    let (records, _) = watched_snapshot(&mut fresh).await;
    assert!(!records.contains_key("watched-gone"), "the record deleted in the gap is gone: {:?}", records.keys());
    let kept: FulfilmentKindSpec =
        serde_json::from_value(records.get("watched-kept").expect("the updated record is present")["spec"].clone()).expect("spec");
    assert_eq!(kept.grants, updated.grants, "the record holds the update made in the gap");
    fresh.cancel().await.expect("cancel");
}

// A table over an engine-served kind opens on its records, though the engine's listing carries no
// position, and streams a record created after it opened.
#[tokio::test(start_paused = true)]
async fn a_table_over_an_engine_kind_opens_and_streams_a_later_change() {
    use flotilla_protocol::table::Message;
    use flotilla_resources::{FulfilmentKind, Resource};

    let daemon = TestDaemon::builder().start().await;
    let base = daemon.get::<FulfilmentKind>(DUMMY_FULFILMENT_KIND).await.expect("the dummy fulfilment kind").spec;
    daemon.apply::<FulfilmentKind>("tabled-before", base.clone()).await;

    let mut watch = daemon.watch_table(FulfilmentKind::API_PATHS.plural).await;
    let mut names = Vec::new();
    loop {
        match watch.next().await {
            Message::Rows { rows, .. } => names.extend(rows.iter().map(|row| text(row, "metadata.name").to_owned())),
            Message::Synced { .. } => break,
            Message::Resync { reason } => panic!("the table opens without a resync: {reason}"),
            _ => {}
        }
    }
    assert!(names.iter().any(|name| name == "tabled-before"), "the table opens on the records: {names:?}");

    daemon.apply::<FulfilmentKind>("tabled-after", base).await;
    loop {
        match watch.next().await {
            Message::Upsert { row, .. } if text(&row, "metadata.name") == "tabled-after" => break,
            Message::Resync { reason } => panic!("the table streams the change without a resync: {reason}"),
            _ => {}
        }
    }
}

// d11 finding 1: a convoy with no change request lands by "no commits beyond base", and its
// checkout is dirty, so teardown holds its removal for a person. After a daemon restart the
// release inspects it again before the convoy's association is known again, and that unknown
// is not false: the checkout stays, held, until a person rules or it is clean. Finding 5: the
// convoy table shows the held convoy needs a person, before and after the restart.
#[tokio::test(start_paused = true)]
async fn a_held_dirty_landed_checkout_stays_held_across_a_restart() {
    let fleet = TestDaemon::builder().vcs("dirty_rate = 1.0").settings("host_name = \"restart-proof\"\n").start().await;
    let name = fleet.dispatch(CrewMix::LongRunner, DUMMY_FULFILMENT_KIND).await;
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    let held = "landed, but its removal is held";
    let needs_you = |set: &ResultSet| {
        set.rows.as_convoys().is_some_and(|rows| {
            rows.iter().any(|row| {
                row.resource.name == name
                    && row.surface_state == flotilla_protocol::result_set::SurfaceState::NeedsYou
                    && row.message.as_deref().is_some_and(|message| message.contains(held))
            })
        })
    };
    fleet.wait_query(QueryId::Convoys { scope: None }, needs_you).await;
    let checkout = fleet.wait_row("the convoy's checkout landed and dirty", "checkout_integration", "", |row| {
        row.get("landed") == Some(&serde_json::Value::Bool(true)) && row.get("clean") == Some(&serde_json::Value::Bool(false))
    });
    let launch = text(&checkout.await, "resource.name").to_owned();

    let fleet = fleet.restart().await;
    fleet.wait_query(QueryId::Convoys { scope: None }, needs_you).await;
    // The restarted daemon's release runs at its start; give it several controller passes.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let rows = fleet.rows("checkout", "").await;
        let row = rows.iter().find(|row| text(row, "resource.name") == launch).expect("the checkout's launch");
        assert_eq!(text(row, "state"), "Realised", "the held checkout is not released without a ruling: {row:?}");
        let integration = fleet.rows("checkout_integration", "").await;
        let row = integration.iter().find(|row| text(row, "resource.name") == launch).expect("the checkout's integration");
        assert_eq!(row.get("landed"), Some(&serde_json::Value::Bool(true)), "its landing stays judged: {row:?}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

// Fleet health counts a capped host's free vessel slots from the vessels the engine holds there:
// `flotilla fleet check`, `fleet spread` and the plain `flotilla fleet` dashboard all ask it.
#[tokio::test(start_paused = true)]
async fn fleet_health_counts_a_capped_hosts_free_slots_from_its_engine_vessels() {
    use flotilla_resources::{Host, HostSpec};
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 60000").start().await;
    let host = daemon.list::<Host>().await.into_iter().next().expect("the local host");
    daemon.apply::<Host>(&host.metadata.name, HostSpec { max_vessels: Some(4), ..host.spec.clone() }).await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    daemon.wait_row("a Realised environment", "environment", "", of_convoy(&convoy, "Realised")).await;

    let health = match daemon.execute(CommandAction::QueryFleetHealth {}).await {
        CommandValue::FleetHealth(health) => health,
        other => panic!("fleet health: {other:?}"),
    };
    let kind = health
        .hosts
        .iter()
        .flat_map(|row| row.fulfilments.iter())
        .find(|kind| kind.name == DUMMY_FULFILMENT_KIND)
        .unwrap_or_else(|| panic!("the dummy fulfilment kind: {health:?}"));
    assert_eq!((kind.max_vessels, kind.free_vessel_slots), (Some(4), Some(3)), "{kind:?}");
}

/// The convoy list's rows, once `daemon` lists none of convoy `convoy`'s: a minute of virtual time
/// at most, as its rows follow the aggregator's convoy rows and the session listing.
async fn wait_unlisted(daemon: &TestDaemon, convoy: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let rows = match daemon.execute(CommandAction::QueryFleetList { project: None, crew_id: None, convoy: None }).await {
            CommandValue::FleetList(list) => list.rows,
            other => panic!("the convoy list: {other:?}"),
        };
        let listed = daemon.list::<TerminalSession>().await;
        let sessions = listed
            .iter()
            .filter(|session| session.metadata.labels.get(flotilla_resources::CONVOY_LABEL).is_some_and(|of| of == convoy))
            .count();
        if rows.is_empty() && sessions == 0 {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "convoy {convoy} is still listed: {rows:?}, {sessions} sessions");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

// #4421: a crew whose terminal session vanishes from its pool mid-run is relaunched from its
// brief, and its convoy lands. The landed convoy then retires, and the lost launch leaves the
// convoy list with it.
#[tokio::test(start_paused = true)]
async fn a_landed_convoy_whose_crew_session_was_lost_retires_with_the_lost_launch() {
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 5000").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let launched = daemon
        .wait("the coder's session", |session: &ResourceObject<TerminalSession>| {
            session.metadata.labels.get(flotilla_resources::CONVOY_LABEL) == Some(&convoy) && daemon.counts().sessions_launched == 1
        })
        .await;
    let lost = launched.metadata.name;
    assert!(daemon.lose_crew(&convoy, "coder"), "the coder's session vanishes from its pool");
    daemon.wait("Landed", in_phase(&convoy, ConvoyPhase::Landed)).await;
    assert_eq!(daemon.counts().sessions_launched, 2, "the coder relaunched once: {:?}", daemon.counts());
    daemon.wait_reached("Retired", "convoy", "Retired", |row| text(row, "resource.name") == convoy).await;
    daemon.wait_gone::<TerminalSession>(&lost).await;
    wait_unlisted(&daemon, &convoy).await;
}

// #4421: a stalled crew whose session its pool loses is not relaunched, so its lost launch stays
// its session's newest. An operator completes the crew and the convoy lands. The convoy and the
// lost launch retire, and stay retired once the daemon restarts and reads every launch again.
#[tokio::test(start_paused = true)]
async fn a_landed_convoy_whose_stalled_crew_lost_its_session_retires_with_the_lost_launch() {
    let daemon = TestDaemon::builder().agent("stall_rate = 1.0").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    daemon
        .wait("a stall", |found: &ResourceObject<Convoy>| {
            found.metadata.name == convoy && found.status.as_ref().is_some_and(|status| status.stalled.is_some())
        })
        .await;
    let session = daemon.wait("the coder's session", |session: &ResourceObject<TerminalSession>| session.spec.role == "coder").await;
    let lost = session.metadata.name.clone();
    let flotilla_resources::TerminalSessionSource::Agent { context, .. } = session.spec.source else { panic!("an agent session") };
    assert!(daemon.lose_crew(&convoy, "coder"), "the coder's session vanishes from its pool");
    daemon
        .wait("the session lost", |session: &ResourceObject<TerminalSession>| {
            session.metadata.name == lost && session.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Lost)
        })
        .await;
    let completed = daemon
        .execute(CommandAction::CrewComplete {
            context: flotilla_protocol::query::CrewCommandContext {
                namespace: Some(NAMESPACE.into()),
                convoy: Some(convoy.clone()),
                vessel_ref: Some(context.vessel_ref),
                role: Some("coder".into()),
                ..Default::default()
            },
            message: Some("operator accepted the ledger-less claim".into()),
            disposition: None,
        })
        .await;
    assert_eq!(completed, CommandValue::Ok);
    daemon.wait("Landed", in_phase(&convoy, ConvoyPhase::Landed)).await;
    assert_eq!(daemon.counts().sessions_launched, 1, "the stalled coder was not relaunched: {:?}", daemon.counts());
    daemon.wait_reached("Retired", "convoy", "Retired", |row| text(row, "resource.name") == convoy).await;
    daemon.wait_gone::<TerminalSession>(&lost).await;
    wait_unlisted(&daemon, &convoy).await;
    let daemon = daemon.restart().await;
    daemon.wait_reached("Retired", "convoy", "Retired", |row| text(row, "resource.name") == convoy).await;
    daemon.wait_gone::<TerminalSession>(&lost).await;
    wait_unlisted(&daemon, &convoy).await;
}
