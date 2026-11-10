use flotilla_store_memory::InMemoryBackend;
use std::{collections::BTreeMap, sync::Arc};

use chrono::Utc;
use flotilla_admission::reconcilers::vessel_requests::{ConvoyStanding, ConvoyWorks, VesselReleaseGate, VesselRequests, WorkNeedingVessel};
use flotilla_protocol::{CanonicalHostId, LifecycleReading, NodeId, PlacementDecision, PlacementTargetHost};
use flotilla_provisioning_testkit::VesselsFixture;
use flotilla_resources::{
    vessel_records::VesselRecords, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta, WorkPhase, WorkState, WorkflowSnapshot,
};
use flotilla_store::ResourceBackend;
use futures::StreamExt;

const NAMESPACE: &str = "flotilla";

/// A gate that sanctions each ended convoy's reclaim, or refuses every one.
struct Gate(bool);

#[async_trait::async_trait]
impl VesselReleaseGate for Gate {
    async fn may_release(&self, convoy: &str, _convoy_home: &str) -> Result<(), String> {
        if self.0 {
            Ok(())
        } else {
            Err(format!("convoy {} has unlanded work", convoy))
        }
    }
}

fn sanctioned() -> Arc<dyn VesselReleaseGate> {
    Arc::new(Gate(true))
}

fn decision(host: &str) -> PlacementDecision {
    PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        kind: format!("host-direct-{host}"),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved(host), display_name: host.to_owned() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    }
}

/// Convoy `name` admitted on `backend`, its one work in `work`, placed on `host` when given.
async fn admit(backend: &ResourceBackend, name: &str, host: Option<&str>, work: WorkPhase) {
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name(name.to_owned()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_owned()).placement_policy("host-direct".to_owned()).build(),
        )
        .await
        .expect("admit convoy");
    let workflow = flotilla_resources::single_agent_workflow_spec();
    convoys
        .update_status(
            name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                placement_decision: host.map(decision),
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: workflow.exit,
                    turn_delivery: workflow.turn_delivery,
                    vessels: workflow.vessels,
                }),
                work: BTreeMap::from([("work".to_owned(), WorkState::builder().phase(work).build())]),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("convoy status");
}

async fn replicate(from: &ResourceBackend, root: &NodeId, to: &ResourceBackend) {
    to.replica_writer::<Convoy>(root.clone(), NAMESPACE)
        .replace(&from.using::<Convoy>(NAMESPACE).list().await.expect("convoys"), Utc::now())
        .await
        .expect("replicate convoys");
}

// A convoy admitted at kiwi and placed on feta's host is launched at feta, as a launch of its work
// in the convoy homed at kiwi; kiwi requests nothing for it.
#[tokio::test]
async fn a_placed_work_is_launched_at_the_home_that_serves_its_host() {
    let kiwi_root = NodeId::new("kiwi");
    let kiwi = ResourceBackend::new(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("feta"));
    admit(&kiwi, "remote", Some("feta-host"), WorkPhase::Ready).await;
    replicate(&kiwi, &kiwi_root, &feta).await;

    let kiwi_vessels = VesselsFixture::at("kiwi");
    let kiwi_requests = VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(kiwi.clone(), NAMESPACE)),
        kiwi_vessels.reads.clone(),
        sanctioned(),
        CanonicalHostId::resolved("kiwi-host"),
        [],
    );
    assert!(kiwi_requests.sync_once(Utc::now()).await.expect("kiwi pass").requested.is_empty());

    let feta_vessels = VesselsFixture::at("feta");
    let feta_requests = VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(feta.clone(), NAMESPACE)),
        feta_vessels.reads.clone(),
        sanctioned(),
        CanonicalHostId::resolved("feta-host"),
        [],
    );
    let pass = feta_requests.sync_once(Utc::now()).await.expect("feta pass");
    assert_eq!(pass.requested.len(), 1);
    let launch = feta_vessels.vessels.get(&pass.requested[0], None).expect("read").expect("launch");
    assert_eq!((launch.convoy.as_str(), launch.convoy_home.as_str(), launch.launch_of.as_str()), ("remote", "kiwi", "remote-work"));
    assert_eq!(launch.host, "feta-host");
    assert_eq!(launch.reading, LifecycleReading::InProgress);

    // A second pass keeps the live launch.
    assert!(feta_requests.sync_once(Utc::now()).await.expect("again").requested.is_empty());
}

// A daemon requests the launches placed on the SSH hosts it actuates for, on those hosts.
#[tokio::test]
async fn a_daemon_launches_the_works_placed_on_its_ssh_hosts() {
    let root = NodeId::new("admitting");
    let admitting = ResourceBackend::new(InMemoryBackend::default()).with_local_root(root.clone());
    let owner = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("owner"));
    admit(&admitting, "ssh", Some("ssh-host"), WorkPhase::Ready).await;
    replicate(&admitting, &root, &owner).await;

    let vessels = VesselsFixture::at("owner");
    let requests = VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(owner.clone(), NAMESPACE)),
        vessels.reads.clone(),
        sanctioned(),
        CanonicalHostId::resolved("owner-host"),
        [CanonicalHostId::resolved("ssh-host")],
    );
    let pass = requests.sync_once(Utc::now()).await.expect("pass");
    let launch = vessels.vessels.get(&pass.requested[0], None).expect("read").expect("launch");
    assert_eq!(launch.host, "ssh-host");
}

// A convoy admitted here with no placement runs its work on this home's own host. Once the convoy
// has ended, the live launch is kept while the gate refuses its reclaim, and released once it
// sanctions it.
#[tokio::test]
async fn an_unplaced_local_work_runs_here_until_its_convoy_is_reclaimed() {
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("home"));
    admit(&backend, "local", None, WorkPhase::Ready).await;
    let vessels = VesselsFixture::at("home");
    let requests = |gate: bool| {
        VesselRequests::new(
            std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(backend.clone(), NAMESPACE)),
            Arc::clone(&vessels.reads),
            Arc::new(Gate(gate)),
            CanonicalHostId::resolved("home-host"),
            [],
        )
    };
    let launch = requests(true).sync_once(Utc::now()).await.expect("pass").requested.pop().expect("requested");
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").host, "home-host");

    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys.get("local").await.expect("convoy");
    let mut status = convoy.status.clone().expect("status");
    status.phase = ConvoyPhase::Landed;
    convoys.update_status("local", &convoy.metadata.resource_version, &status).await.expect("land");

    let refused = requests(false).sync_once(Utc::now()).await.expect("refused pass");
    assert!(refused.released.is_empty());
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::InProgress);

    let pass = requests(true).sync_once(Utc::now()).await.expect("pass");
    assert_eq!(pass.released, vec![launch.clone()]);
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::Released);
}

/// Convoys as the port reports them, held in memory: the works needing a vessel, and where each
/// convoy stands.
#[derive(Default)]
struct HeldWorks {
    works: std::sync::Mutex<Vec<WorkNeedingVessel>>,
    standing: std::sync::Mutex<BTreeMap<(String, String), ConvoyStanding>>,
}

#[async_trait::async_trait]
impl ConvoyWorks for HeldWorks {
    async fn works_needing_vessel(&self) -> Result<Vec<WorkNeedingVessel>, String> {
        Ok(self.works.lock().expect("works").clone())
    }

    async fn standing(&self, convoy: &str, convoy_home: &str) -> Result<ConvoyStanding, String> {
        Ok(self
            .standing
            .lock()
            .expect("standing")
            .get(&(convoy.to_owned(), convoy_home.to_owned()))
            .copied()
            .unwrap_or(ConvoyStanding::Gone))
    }

    async fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, String> {
        Ok(futures::stream::pending().boxed())
    }
}

/// A gate that must not be asked.
struct Unasked;

#[async_trait::async_trait]
impl VesselReleaseGate for Unasked {
    async fn may_release(&self, convoy: &str, _convoy_home: &str) -> Result<(), String> {
        panic!("convoy {convoy} is gone; its reclaim needs no gate")
    }
}

// Through the port alone: a work placed here is launched with the convoy's record name, kept while
// its convoy is live, released without the gate once the convoy is gone, and an ended convoy's
// launch is released, never failed, once the gate sanctions it.
#[tokio::test]
async fn the_requester_follows_its_convoys_through_the_port() {
    let held = Arc::new(HeldWorks::default());
    let work = |convoy: &str| WorkNeedingVessel {
        convoy: convoy.into(),
        convoy_home: "kiwi".into(),
        work: "work".into(),
        host: Some(CanonicalHostId::resolved("home-host")),
    };
    *held.works.lock().expect("works") = vec![work("c#3"), work("gone#1")];
    held.standing.lock().expect("standing").insert(("c#3".into(), "kiwi".into()), ConvoyStanding::Live);
    let vessels = VesselsFixture::at("home");
    let requests = |gate: Arc<dyn VesselReleaseGate>| {
        VesselRequests::new(held.clone(), Arc::clone(&vessels.reads), gate, CanonicalHostId::resolved("home-host"), [])
    };
    let mut requested = requests(Arc::new(Unasked)).sync_once(Utc::now()).await.expect("pass").requested;
    requested.sort();
    assert_eq!(requested.len(), 2);
    let launch = vessels.vessels.get(&requested[0], None).expect("read").expect("launch");
    assert_eq!((launch.convoy.as_str(), launch.launch_of.as_str()), ("c#3", "c-work"), "a work's launches are of the convoy's launch_of");

    // Neither work needs a vessel now: the live convoy's launch is kept, the gone one's released.
    held.works.lock().expect("works").clear();
    let pass = requests(Arc::new(Unasked)).sync_once(Utc::now()).await.expect("pass");
    assert_eq!(pass.released, vec![requested[1].clone()]);
    assert_eq!(vessels.vessels.get(&requested[0], None).expect("read").expect("launch").reading, LifecycleReading::InProgress);

    held.standing.lock().expect("standing").insert(("c#3".into(), "kiwi".into()), ConvoyStanding::Ended);
    assert!(requests(Arc::new(Gate(false))).sync_once(Utc::now()).await.expect("refused").released.is_empty());
    let pass = requests(sanctioned()).sync_once(Utc::now()).await.expect("sanctioned");
    assert_eq!(pass.released, vec![requested[0].clone()]);
    assert_eq!(vessels.vessels.get(&requested[0], None).expect("read").expect("launch").reading, LifecycleReading::Released);
}
