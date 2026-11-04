//! Behaviour through the public daemon harness (#4321): each scenario starts
//! a daemon as `flotillad` composes one and touches only what an operator, a
//! crew or the environment vendor touches.

use flotilla_daemon_testkit::daemon::TestDaemon;
use flotilla_dummy_fleet::{
    catalog::{CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND},
    DummyVendor,
};
use flotilla_resources::{Convoy, ConvoyPhase, Environment, EnvironmentPhase, ResourceObject, VESSEL_REF_LABEL};

/// An environment of one of `convoy`'s vessels, in `phase`.
fn of_convoy(convoy: &str, phase: EnvironmentPhase) -> impl Fn(&ResourceObject<Environment>) -> bool + '_ {
    move |environment| {
        environment.metadata.labels.get(VESSEL_REF_LABEL).is_some_and(|vessel| vessel.starts_with(&format!("{convoy}-")))
            && environment.status.as_ref().is_some_and(|status| status.phase == phase)
    }
}

fn in_phase(convoy: &str, phase: ConvoyPhase) -> impl Fn(&ResourceObject<Convoy>) -> bool + '_ {
    move |found| found.metadata.name == convoy && found.status.as_ref().is_some_and(|status| status.phase == phase)
}

// A cloud sandbox the vendor deletes, or stops with no resume available, is
// Lost: the daemon learns it only from what the vendor reports, and no stored
// record is touched to make it so.
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
    let ready = daemon.wait("a Ready cloud sandbox", of_convoy(&convoy, EnvironmentPhase::Ready)).await;
    assert!(ready.spec.cloud_sandbox.is_some(), "{ready:?}");
    assert!(vendor(daemon.vendor(), &ready.metadata.name));
    let name = ready.metadata.name;
    let lost = |environment: &ResourceObject<Environment>| {
        environment.metadata.name == name && environment.status.as_ref().is_some_and(|s| s.phase == EnvironmentPhase::Lost)
    };
    daemon.wait("Lost", lost).await;
}

// A restarted daemon finds its running vessels again: the vendor's backings and
// the crew's process survive, and the convoy lands with nothing provisioned or
// launched twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_daemon_adopts_its_running_vessels() {
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 5000").start().await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let environment = daemon.wait("a Ready environment", of_convoy(&convoy, EnvironmentPhase::Ready)).await;
    daemon.wait("a launched coder", |_: &ResourceObject<Convoy>| daemon.counts().sessions_launched == 1).await;
    let daemon = daemon.restart().await;
    daemon.wait("Landed", in_phase(&convoy, ConvoyPhase::Landed)).await;
    let counts = daemon.counts();
    assert_eq!((counts.sessions_launched, counts.environments_provisioned, counts.command_errors), (1, 1, 0), "{counts:?}");
    // The adopted backing is the one torn down once the convoy lands.
    daemon.wait_gone::<Environment>(&environment.metadata.name).await;
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
