//! Behaviour through the public daemon harness (#4321): each scenario starts
//! a daemon as `flotillad` composes one and touches only what an operator, a
//! crew or the environment vendor touches.

use flotilla_daemon_testkit::daemon::TestDaemon;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix},
    DummyFleet,
};
use flotilla_protocol::{commands::CommandValue, CommandAction};
use flotilla_resources::{ConvoyPhase, Environment, EnvironmentPhase, ResourceObject, VESSEL_REF_LABEL};

/// An environment of one of `convoy`'s vessels, in `phase`.
fn of_convoy(convoy: &str, phase: EnvironmentPhase) -> impl Fn(&ResourceObject<Environment>) -> bool + '_ {
    move |environment| {
        environment.metadata.labels.get(VESSEL_REF_LABEL).is_some_and(|vessel| vessel.starts_with(&format!("{convoy}-")))
            && environment.status.as_ref().is_some_and(|status| status.phase == phase)
    }
}

// A cloud sandbox the vendor deletes, or stops with no resume available, is
// Lost: the daemon learns it only from what the vendor reports, and no stored
// record is touched to make it so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloud_sandbox_the_vendor_deletes_becomes_lost() {
    cloud_sandbox_becomes_lost(DummyFleet::vanish_environment).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloud_sandbox_the_vendor_stops_becomes_lost() {
    cloud_sandbox_becomes_lost(DummyFleet::stop_environment).await;
}

async fn cloud_sandbox_becomes_lost(vendor: fn(&DummyFleet, &str) -> bool) {
    let daemon = TestDaemon::builder().start().await;
    daemon.dispatch_to_cloud("lost", CrewMix::Coder).await;
    let convoy = daemon.wait_for("lost", "admission", |convoy| convoy.status.is_some()).await.metadata.name;
    let ready = daemon.wait_for_any::<Environment>("a Ready cloud sandbox", of_convoy(&convoy, EnvironmentPhase::Ready)).await;
    assert!(ready.spec.cloud_sandbox.is_some(), "{ready:?}");
    assert!(vendor(daemon.fleet(), &ready.metadata.name));
    let lost = |environment: &ResourceObject<Environment>| environment.status.as_ref().is_some_and(|s| s.phase == EnvironmentPhase::Lost);
    daemon.wait_for_resource::<Environment>(&ready.metadata.name, "Lost", lost).await;
}

// A restarted daemon finds its running vessels again: the vendor's backings and
// the crew's process survive, and the convoy lands with nothing provisioned or
// launched twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_daemon_adopts_its_running_vessels() {
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 5000").start().await;
    daemon.dispatch("adopted", CrewMix::Coder).await;
    let convoy = daemon.wait_for("adopted", "a launched coder", |_| daemon.snapshot().sessions_launched == 1).await.metadata.name;
    let environment = daemon.wait_for_any::<Environment>("a Ready environment", of_convoy(&convoy, EnvironmentPhase::Ready)).await;
    let daemon = daemon.restart().await;
    daemon.wait_for_phase("adopted", ConvoyPhase::Landed).await;
    let snapshot = daemon.snapshot();
    assert_eq!((snapshot.sessions_launched, snapshot.environments_provisioned), (1, 1), "{snapshot:?}");
    assert_eq!(snapshot.command_errors, 0, "{:?}", daemon.fleet().command_error_samples());
    // The adopted backing is the one torn down once the convoy lands.
    daemon.wait_for_absence::<Environment>(&environment.metadata.name).await;
    assert_eq!(daemon.snapshot().environments_destroyed, 1);
}

// A vendor that refuses to provision fails the convoy before any crew launches;
// once it accepts again, the next convoy lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vendor_refusing_to_provision_fails_its_convoy() {
    let daemon = TestDaemon::builder().start().await;
    daemon.fleet().refuse_provisioning(true);
    daemon.dispatch("refused", CrewMix::Coder).await;
    daemon.wait_for_phase("refused", ConvoyPhase::Failed).await;
    assert_eq!((daemon.snapshot().environments_provisioned, daemon.snapshot().sessions_launched), (0, 0));
    daemon.fleet().refuse_provisioning(false);
    daemon.deliver("accepted", CrewMix::Coder).await;
}

// An operator's command travels the request dispatcher a socket client uses and
// returns its value; a query returns its listing the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_operator_drives_the_daemon_through_the_request_dispatcher() {
    let daemon = TestDaemon::builder().start().await;
    let client = daemon.client().await;
    let forge = catalog::forge(&daemon.fleet().config().forge_service_url);
    let created = client
        .execute(CommandAction::ConvoyCreate {
            name: "operated".into(),
            workflow_ref: CrewMix::Coder.workflow_name().into(),
            inputs: Vec::new(),
            repository_url: Some(catalog::repository_url(&forge)),
            r#ref: Some("main".into()),
            project_ref: None,
            placement_policy: Some(catalog::DUMMY_FULFILMENT_KIND.into()),
            adopted_checkout: None,
        })
        .await;
    assert_eq!(created, CommandValue::ConvoyCreated { name: "operated".into() });
    daemon.wait_for_phase("operated", ConvoyPhase::Landed).await;
    let listed = client.execute(CommandAction::QueryFleetList { project: None, crew_id: None, convoy: None }).await;
    assert!(matches!(listed, CommandValue::FleetList(_)), "{listed:?}");
}
