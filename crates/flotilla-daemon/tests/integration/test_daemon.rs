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
use flotilla_resources::{Convoy, ConvoyPhase, ResourceObject, TerminalSession};

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

// A cloud sandbox the vendor deletes, or stops with no resume available, is
// Lost: the daemon learns it only from what the vendor reports, and no stored
// record is touched to make it so; the Lost status is written only as the
// engine's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloud_sandbox_the_vendor_deletes_becomes_lost() {
    cloud_sandbox_becomes_lost(DummyVendor::vanish).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relaunched_vessels_role_passes_to_its_new_session() {
    the_aides_role_passes_to_its_relaunch(false).await;
}

// As above, with the daemon restarting just after the loss, as b0's did: the restarted daemon
// finds the backing gone at adoption and relaunches the vessel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let assist = |row: &Row| of_convoy(&convoy, "Realised")(row) && text(row, "holder").contains("-assist#");
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
        .within(std::time::Duration::from_secs(120))
        .await;
    if waiting.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed) {
        return;
    }
    let resume = CommandAction::CrewSupervise {
        namespace: Some(NAMESPACE.into()),
        convoy: convoy.clone(),
        vessel: "govern".into(),
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
// the row reaching it shows the view woke on the store's commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crew_members_row_follows_its_agent_session() {
    let daemon = TestDaemon::builder().start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let reading = |set: &ResultSet| -> Option<AgentSessionReading> {
        let row = set.rows.as_convoys()?.iter().find(|row| row.resource.name == convoy)?;
        row.vessels.iter().flat_map(|vessel| &vessel.crew).find_map(|crew| crew.agent_session.clone())
    };
    let activity = |set: &ResultSet, activity: AgentActivity| reading(set).is_some_and(|reading| reading.activity == Some(activity));
    let working = daemon.wait_query(QueryId::Convoys { scope: None }, |set| activity(set, AgentActivity::Working)).await;
    let working = reading(&working).expect("working reading");
    assert_eq!(working.ended, None, "{working:?}");
    assert_eq!(working.activity_cause.as_deref(), Some("dummy-working"), "{working:?}");
    let idle = daemon.wait_query(QueryId::Convoys { scope: None }, |set| activity(set, AgentActivity::Idle)).await;
    let idle = reading(&idle).expect("idle reading");
    assert_eq!(idle.activity_cause.as_deref(), Some("dummy-idle"), "{idle:?}");
    assert!(idle.activity_since > working.activity_since, "{working:?} then {idle:?}");
}

// Another home's crew shows its AgentSession here too: the session is written at its home, and
// this daemon reads the replica its decided store follows from that home's log, joined to the
// replicated TerminalSession by home and record name. A stalled crew waits idle for guidance, so
// its reading holds still while the peers connect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
// follows tokio's paused time, so the clock and the deadline sleep move together.
#[tokio::test(start_paused = true)]
async fn a_crew_members_row_goes_stale_at_its_deadline_once_its_home_is_unheard() {
    use flotilla_resources::{agent_activity::HOME_HEARD_WITHIN, Clock as _};
    let clock = std::sync::Arc::new(flotilla_store_testkit::PausedClock::start());
    let (a, b) = tokio::join!(
        TestDaemon::builder().clock(clock.clone()).agent("stall_rate = 1.0").settings("host_name = \"agents-a\"\n").start(),
        TestDaemon::builder().clock(clock.clone()).agent("stall_rate = 1.0").settings("host_name = \"agents-b\"\n").start()
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
    let dropped = clock.now();
    let stale = b
        .wait_query(QueryId::Convoys { scope: None }, |set| reading(set).is_some_and(|reading| reading.stale))
        .within(std::time::Duration::from_secs(300))
        .await;
    let stale_after = clock.now() - dropped;
    assert!(
        stale_after <= HOME_HEARD_WITHIN && stale_after > HOME_HEARD_WITHIN - chrono::Duration::seconds(15),
        "stale {stale_after} after the drop"
    );
    assert_eq!(reading(&stale).map(|reading| AgentSessionReading { stale: false, ..reading }), Some(at_home), "only its freshness changed");
}

// A restarted daemon finds its running vessels again: the vendor's backings and
// the crew's process survive, and the convoy lands with nothing provisioned or
// launched twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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

// A vendor that refuses to provision fails the convoy before any crew launches;
// once it accepts again, the next convoy lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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

// A client whose watch of an engine-served kind drops reconnects with its cursor: the engine keeps
// no position to resume from, so the resume is refused as expired, and the client's fresh watch
// converges on the records as they are, with an update and a delete made in the gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let refused = resumed.next().await.expect_err("a resume from the engine's records is refused");
    assert!(refused.contains("expired"), "the refusal says the cursor expired: {refused}");

    let mut fresh = daemon.resources().watch(request().build()).await.expect("watch again");
    let (records, _) = watched_snapshot(&mut fresh).await;
    assert!(!records.contains_key("watched-gone"), "the record deleted in the gap is gone: {:?}", records.keys());
    let kept: FulfilmentKindSpec =
        serde_json::from_value(records.get("watched-kept").expect("the updated record is present")["spec"].clone()).expect("spec");
    assert_eq!(kept.grants, updated.grants, "the record holds the update made in the gap");
    fresh.cancel().await.expect("cancel");
}
