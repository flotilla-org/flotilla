//! Behaviour through the public daemon harness (#4321): each scenario starts
//! a daemon as `flotillad` composes one and touches only what an operator, a
//! crew or the environment vendor touches.

use flotilla_daemon_testkit::daemon::TestDaemon;
use flotilla_dummy_fleet::{
    catalog::{CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND},
    DummyVendor,
};
use flotilla_protocol::table::Row;
use flotilla_resources::{Convoy, ConvoyPhase, ResourceObject};

fn text<'a>(row: &'a Row, column: &str) -> &'a str {
    row.get(column).and_then(serde_json::Value::as_str).unwrap_or_default()
}

/// An environment held for one of `convoy`'s vessels, in `state`.
fn of_convoy<'a>(convoy: &'a str, state: &'a str) -> impl Fn(&Row) -> bool + 'a {
    move |row| {
        let holder = text(row, "holder");
        let vessel = holder.rsplit([':', '/']).next().unwrap_or(holder);
        vessel.starts_with(&format!("{convoy}-")) && text(row, "state") == state
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
