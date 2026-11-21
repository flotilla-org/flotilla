//! What the convoy planner reads of a Host (#4413), through the public daemon harness: the scenario
//! touches only what an operator touches, and holds before and after the planners moved onto their
//! Host port.
//!
//! Admission names each placement's host by its canonical Host name and freezes its provider, so
//! the planners read a host only by that name.

use std::time::Duration;

use flotilla_daemon_testkit::daemon::{TestDaemon, NAMESPACE};
use flotilla_dummy_fleet::{
    catalog::{fulfilment_kind, CrewMix, DUMMY_FULFILMENT_KIND},
    DUMMY_AGENT_ADAPTER,
};
use flotilla_protocol::{commands::CommandValue, CommandAction};
use flotilla_resources::{
    Convoy, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, Host, HostConnection, HostSpec, ResourceObject, StallCause,
};

/// Whether `convoy` is noted as waiting for capacity.
fn waits_for_capacity(convoy: &str) -> impl Fn(&ResourceObject<Convoy>) -> bool + '_ {
    move |found| {
        found.metadata.name == convoy
            && found
                .status
                .as_ref()
                .and_then(|status| status.stalls.about(None))
                .is_some_and(|stall| stall.cause == Some(StallCause::Capacity))
    }
}

/// An SSH host this daemon owns, declared by the operator rather than in `hosts.toml`: no home
/// serves its vessels, so a convoy placed on it waits on its capacity until the host changes.
const BOX: &str = "box";
/// Host-direct crews on the box.
const BOX_KIND: &str = "box-crews";

/// The box, capped at `max_vessels`, as the operator declares it.
fn box_spec(owner: &str, max_vessels: u32) -> HostSpec {
    HostSpec {
        display_name: BOX.into(),
        connection: HostConnection::SshHost { owning_daemon: owner.into(), destination: "crew@box".into() },
        max_vessels: Some(max_vessels),
        single_tenant: true,
        ..HostSpec::default()
    }
}

// Behaviour (#4413): a convoy waiting for room on a capped host stops waiting once the operator raises the host's cap. The Host change alone wakes it: no
// controller resync falls within the wait.
#[tokio::test(start_paused = true)]
async fn a_convoy_waiting_for_room_is_woken_when_its_host_raises_its_cap() {
    let resync = Duration::from_secs(24 * 60 * 60);
    let daemon = TestDaemon::builder().agent("turn_delay_ms = 600000").resync(resync).start().await;
    let local = daemon.list::<Host>().await.into_iter().next().expect("the local host");
    daemon.apply::<Host>(BOX, box_spec(&local.metadata.name, 1)).await;
    // What the box's owner would observe there: the local host's facts, and the dummy adapter.
    let mut observed = local.status.clone().expect("the local host's status");
    let facts = observed.fulfilment_facts.get(DUMMY_FULFILMENT_KIND).cloned().expect("the dummy kind's facts");
    observed.fulfilment_facts = [(BOX_KIND.to_owned(), facts)].into();
    observed.capabilities.insert(flotilla_resources::AGENT_ADAPTERS_CAPABILITY.into(), serde_json::json!([DUMMY_AGENT_ADAPTER]));
    patch_host_status(&daemon, BOX, observed).await;
    let kind = FulfilmentKindSpec { realisation: FulfilmentRealisation::HostDirect, ..fulfilment_kind(BOX) };
    daemon.apply::<FulfilmentKind>(BOX_KIND, kind).await;

    let first = daemon.dispatch(CrewMix::Coder, BOX_KIND).await;
    // Admission order is creation time: the second is created later.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second = daemon.dispatch(CrewMix::Coder, BOX_KIND).await;
    daemon.wait("the second convoy waits for room", waits_for_capacity(&second)).await;
    assert!(!waits_for_capacity(&first)(&daemon.get::<Convoy>(&first).await.expect("first")), "the first holds the room");

    let raised = tokio::time::Instant::now();
    daemon.apply::<Host>(BOX, box_spec(&local.metadata.name, 2)).await;
    daemon
        .wait("the second convoy has room", |found: &ResourceObject<Convoy>| {
            found.metadata.name == second && !waits_for_capacity(&second)(found)
        })
        .await;
    assert!(raised.elapsed() < resync / 2, "woken by the Host change, not a resync: {:?}", raised.elapsed());
}

/// Replace Host `name`'s status with `status`, as an operator's `flotilla` status patch does.
async fn patch_host_status(daemon: &TestDaemon, name: &str, status: flotilla_resources::HostStatus) {
    let patched = daemon
        .execute(CommandAction::ResourceStatusPatch {
            namespace: NAMESPACE.into(),
            kind: "Host".into(),
            name: name.into(),
            status: serde_json::to_value(status).expect("status"),
        })
        .await;
    assert!(matches!(patched, CommandValue::ResourceObject(_)), "patch Host {name}: {patched:?}");
}
