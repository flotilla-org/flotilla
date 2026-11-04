//! Behaviour through the public daemon harness (#4321): each scenario starts
//! a daemon as `flotillad` composes one and touches only what an operator, a
//! crew or the environment vendor touches.

use std::time::Duration;

use flotilla_daemon_testkit::daemon::TestDaemon;
use flotilla_dummy_fleet::catalog::{CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND};
use flotilla_protocol::{
    commands::{CheckoutArchiveStatus, CommandValue},
    CommandAction,
};
use flotilla_resources::{
    Checkout, Convoy, ConvoyPhase, Environment, EnvironmentPhase, EnvironmentStatus, Event, ResourceObject, VESSEL_REF_LABEL,
};

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

/// The environment named `name`, once `done` holds of its status.
fn environment_named<'a>(
    name: &'a str,
    done: impl Fn(&EnvironmentStatus) -> bool + 'a,
) -> impl Fn(&ResourceObject<Environment>) -> bool + 'a {
    move |found| found.metadata.name == name && found.status.as_ref().is_some_and(&done)
}

/// A dispatched cloud convoy whose crew is still at work, its Ready sandbox,
/// and the checkout in it.
async fn working_cloud_convoy(daemon: &TestDaemon) -> (String, String, String) {
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_CLOUD_FULFILMENT_KIND).await;
    let ready = daemon.wait("a Ready cloud sandbox", of_convoy(&convoy, EnvironmentPhase::Ready)).await;
    assert!(ready.spec.cloud_sandbox.is_some(), "{ready:?}");
    let environment = ready.metadata.name;
    let in_sandbox = |checkout: &ResourceObject<Checkout>| checkout.spec.env_ref() == Some(environment.as_str());
    let checkout = daemon.wait("a checkout in the sandbox", in_sandbox).await.metadata.name;
    (convoy, environment, checkout)
}

/// Liveness passes come every resync, so a loss is confirmed promptly.
async fn cloud_daemon() -> TestDaemon {
    TestDaemon::builder().agent("turn_delay_ms = 60000").resync(Duration::from_millis(100)).start().await
}

// A cloud sandbox the vendor deletes is Lost: the daemon learns it only from
// what the vendor reports, and no stored record is touched to make it so.
// #4304: a later liveness pass that still finds it gone records the backing
// gone, and abandoning the convoy then releases its checkout and environment:
// the work went with the backing, so abandon names the checkout unrecoverable,
// nothing is asked of the vendor for a backing it no longer holds, and a
// forced delete then completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoning_a_convoy_whose_sandbox_the_vendor_deleted_releases_its_teardown() {
    let daemon = cloud_daemon().await;
    let (convoy, environment, checkout) = working_cloud_convoy(&daemon).await;
    assert!(daemon.vendor().vanish(&environment));
    daemon.wait("Lost", environment_named(&environment, |status| status.phase == EnvironmentPhase::Lost)).await;
    daemon.wait("backing gone", environment_named(&environment, |status| status.backing_gone_reason().is_some())).await;

    let abandoned = daemon.execute(CommandAction::ConvoyAbandon { namespace: None, name: convoy.clone(), reason: "gone".into() }).await;
    let CommandValue::ConvoyAbandoned { archives, .. } = abandoned else { panic!("abandon: {abandoned:?}") };
    assert!(
        archives.iter().any(|archive| archive.checkout == checkout && archive.status == CheckoutArchiveStatus::Lost),
        "abandon names the checkout unrecoverable: {archives:?}"
    );
    daemon.wait_gone::<Checkout>(&checkout).await;
    // The checkout's record leaves an event saying what went with the backing.
    daemon
        .wait("the lost work recorded", |event: &ResourceObject<Event>| {
            event.spec.reason == "CheckoutWorkLost" && event.spec.regarding.name == checkout
        })
        .await;
    daemon.wait_gone::<Environment>(&environment).await;
    let deleted = daemon.execute(CommandAction::ConvoyDelete { namespace: None, name: convoy.clone(), force: true }).await;
    assert!(!matches!(deleted, CommandValue::Error { .. }), "nothing holds the forced delete: {deleted:?}");
    daemon.wait_gone::<Convoy>(&convoy).await;
    let counts = daemon.counts();
    assert_eq!((counts.environments_vanished, counts.environment_destroys_not_found, counts.live_environments), (1, 0, 0), "{counts:?}");
}

// A cloud sandbox the vendor stops, with no resume available, is Lost too.
// #4304: but its work still exists at the vendor, so no liveness pass records
// the backing gone. Abandon leaves the work in place, and a forced delete
// holds teardown and says what holds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_convoy_whose_sandbox_the_vendor_stopped_holds_its_teardown() {
    let daemon = cloud_daemon().await;
    let (convoy, environment, checkout) = working_cloud_convoy(&daemon).await;
    assert!(daemon.vendor().stop(&environment));
    daemon.wait("Lost", environment_named(&environment, |status| status.phase == EnvironmentPhase::Lost)).await;

    let abandoned = daemon.execute(CommandAction::ConvoyAbandon { namespace: None, name: convoy.clone(), reason: "stopped".into() }).await;
    let CommandValue::ConvoyAbandoned { archives, .. } = abandoned else { panic!("abandon: {abandoned:?}") };
    assert!(archives.iter().all(|archive| archive.status != CheckoutArchiveStatus::Lost), "the work still exists: {archives:?}");
    let deleted = daemon.execute(CommandAction::ConvoyDelete { namespace: None, name: convoy.clone(), force: true }).await;
    let CommandValue::Error { message } = deleted else { panic!("a held teardown is not a deletion: {deleted:?}") };
    // Each held resource is named on a line of its own.
    let names = |held: String| message.lines().any(|line| line.starts_with(&held));
    assert!(names(format!("checkout {checkout}:")) && names(format!("environment {environment}:")), "{message}");

    // Many liveness passes later, the loss is still unconfirmed and teardown held.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let held = daemon.get::<Environment>(&environment).await.expect("environment held");
    assert!(held.status.as_ref().is_some_and(|status| status.phase == EnvironmentPhase::Lost && !status.backing_gone), "{held:?}");
    assert!(daemon.get::<Checkout>(&checkout).await.is_some(), "checkout held");
    assert_eq!((daemon.counts().environments_destroyed, daemon.counts().live_environments), (0, 1));
}
