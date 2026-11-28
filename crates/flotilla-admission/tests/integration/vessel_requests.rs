use std::{collections::BTreeMap, sync::Arc};

use chrono::Utc;
use flotilla_admission::reconcilers::vessel_requests::{
    ConvoyKey, ConvoyStanding, ConvoyWorks, ConvoyWorksRead, VesselReleaseGate, VesselRequests, WorkNeedingVessel,
};
use flotilla_engine_convoys_testkit::EngineHome;
use flotilla_protocol::{CanonicalHostId, LifecycleReading, PlacementDecision, PlacementTargetHost};
use flotilla_provisioning_testkit::VesselsFixture;
use flotilla_resources::{vessel_records::VesselRecords, ConvoyPhase, ConvoySpec, ConvoyStatus, WorkPhase, WorkState, WorkflowSnapshot};
use flotilla_store::load_metrics::{measure, KindReadCounts};
use std::collections::BTreeSet;

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

/// Convoy `name` admitted at `home`, its one work in `work`, placed on `host` when given; returns
/// the launch.
async fn admit(home: &EngineHome, name: &str, host: Option<&str>, work: WorkPhase) -> String {
    let workflow = flotilla_resources::single_agent_workflow_spec();
    let status = ConvoyStatus {
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
    };
    let spec = ConvoySpec::builder()
        .project_ref(NAMESPACE.into())
        .workflow_ref("workflow".to_owned())
        .placement_policy("host-direct".to_owned())
        .build();
    home.admit(name, spec, status).await
}

// A convoy admitted at kiwi and placed on feta's host is launched at feta, as a launch of its work
// in the convoy homed at kiwi; kiwi requests nothing for it.
#[tokio::test]
async fn a_placed_work_is_launched_at_the_home_that_serves_its_host() {
    let kiwi = EngineHome::open("kiwi");
    let feta = EngineHome::open("feta");
    let convoy = admit(&kiwi, "remote", Some("feta-host"), WorkPhase::Ready).await;
    kiwi.replicate_to(&feta);

    let kiwi_vessels = VesselsFixture::at("kiwi");
    let kiwi_requests = VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::EngineConvoyWorks::new(
            Arc::clone(kiwi.convoys.reads()),
            NAMESPACE,
        )),
        kiwi_vessels.reads.clone(),
        sanctioned(),
        CanonicalHostId::resolved("kiwi-host"),
        [],
    );
    assert!(kiwi_requests.sync_once(Utc::now()).await.expect("kiwi pass").requested.is_empty());

    let feta_vessels = VesselsFixture::at("feta");
    let feta_requests = VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::EngineConvoyWorks::new(
            Arc::clone(feta.convoys.reads()),
            NAMESPACE,
        )),
        feta_vessels.reads.clone(),
        sanctioned(),
        CanonicalHostId::resolved("feta-host"),
        [],
    );
    let pass = feta_requests.sync_once(Utc::now()).await.expect("feta pass");
    assert_eq!(pass.requested.len(), 1);
    let launch = feta_vessels.vessels.get(&pass.requested[0], None).expect("read").expect("launch");
    assert_eq!((launch.convoy.as_str(), launch.convoy_home.as_str(), launch.launch_of.as_str()), (convoy.as_str(), "kiwi", "remote-work"));
    assert_eq!(launch.host, "feta-host");
    assert_eq!(launch.reading, LifecycleReading::InProgress);

    // A second pass keeps the live launch.
    assert!(feta_requests.sync_once(Utc::now()).await.expect("again").requested.is_empty());
}

// A daemon requests the launches placed on the SSH hosts it actuates for, on those hosts.
#[tokio::test]
async fn a_daemon_launches_the_works_placed_on_its_ssh_hosts() {
    let admitting = EngineHome::open("admitting");
    let owner = EngineHome::open("owner");
    admit(&admitting, "ssh", Some("ssh-host"), WorkPhase::Ready).await;
    admitting.replicate_to(&owner);

    let vessels = VesselsFixture::at("owner");
    let requests = VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::EngineConvoyWorks::new(
            Arc::clone(owner.convoys.reads()),
            NAMESPACE,
        )),
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
    let home = EngineHome::open("home");
    let convoy = admit(&home, "local", None, WorkPhase::Ready).await;
    let vessels = VesselsFixture::at("home");
    let requests = |gate: bool| {
        VesselRequests::new(
            std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::EngineConvoyWorks::new(
                Arc::clone(home.convoys.reads()),
                NAMESPACE,
            )),
            Arc::clone(&vessels.reads),
            Arc::new(Gate(gate)),
            CanonicalHostId::resolved("home-host"),
            [],
        )
    };
    let launch = requests(true).sync_once(Utc::now()).await.expect("pass").requested.pop().expect("requested");
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").host, "home-host");

    flotilla_engine_convoys_testkit::land(&home.convoys.path(), &home.convoy(&convoy).await).await;

    let refused = requests(false).sync_once(Utc::now()).await.expect("refused pass");
    assert!(refused.released.is_empty());
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::InProgress);

    let pass = requests(true).sync_once(Utc::now()).await.expect("pass");
    assert_eq!(pass.released, vec![launch.clone()]);
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::Released);
}

/// Convoys as the port reports them, held in memory: the works needing a vessel, and where each
/// convoy stands (one not standing is not read). Each convoy has the one
/// work `work`; the live ones are those with works and those standing live.
#[derive(Default)]
struct HeldWorks {
    works: std::sync::Mutex<Vec<WorkNeedingVessel>>,
    standing: std::sync::Mutex<BTreeMap<(String, String), ConvoyStanding>>,
}

#[async_trait::async_trait]
impl ConvoyWorks for HeldWorks {
    fn live(&self) -> Result<Vec<ConvoyKey>, String> {
        let mut live: std::collections::BTreeSet<ConvoyKey> =
            self.works.lock().expect("works").iter().map(|work| (work.convoy.clone(), work.convoy_home.clone())).collect();
        let standing = self.standing.lock().expect("standing");
        live.extend(standing.iter().filter(|(_, standing)| **standing == ConvoyStanding::Live).map(|(key, _)| key.clone()));
        Ok(live.into_iter().collect())
    }

    async fn convoy(&self, convoy: &str, convoy_home: &str) -> Result<Option<ConvoyWorksRead>, String> {
        let standing = self.standing(convoy, convoy_home).await?;
        let works = self.works.lock().expect("works");
        let needing = works.iter().filter(|work| work.convoy == convoy && work.convoy_home == convoy_home).cloned().collect();
        Ok((standing != ConvoyStanding::Unread).then(|| ConvoyWorksRead { standing, works: vec!["work".into()], needing }))
    }

    async fn standing(&self, convoy: &str, convoy_home: &str) -> Result<ConvoyStanding, String> {
        Ok(self
            .standing
            .lock()
            .expect("standing")
            .get(&(convoy.to_owned(), convoy_home.to_owned()))
            .copied()
            .unwrap_or(ConvoyStanding::Unread))
    }
}

/// Convoy `convoy`'s one work, homed at kiwi and placed on this home's host.
fn placed(convoy: &str) -> WorkNeedingVessel {
    WorkNeedingVessel {
        convoy: convoy.into(),
        convoy_home: "kiwi".into(),
        work: "work".into(),
        host: Some(CanonicalHostId::resolved("home-host")),
    }
}

/// A gate that must not be asked.
struct Unasked;

#[async_trait::async_trait]
impl VesselReleaseGate for Unasked {
    async fn may_release(&self, convoy: &str, _convoy_home: &str) -> Result<(), String> {
        panic!("convoy {convoy} is not ended; its reclaim is not judged")
    }
}

// Through the port alone: a work placed here is launched with the convoy's record name, kept while
// its convoy is live or not read here, and an ended or archived convoy's launch is released, never
// failed, once the gate sanctions it.
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
    held.standing.lock().expect("standing").insert(("gone#1".into(), "kiwi".into()), ConvoyStanding::Live);
    let vessels = VesselsFixture::at("home");
    let requests = |gate: Arc<dyn VesselReleaseGate>| {
        VesselRequests::new(held.clone(), Arc::clone(&vessels.reads), gate, CanonicalHostId::resolved("home-host"), [])
    };
    let mut requested = requests(Arc::new(Unasked)).sync_once(Utc::now()).await.expect("pass").requested;
    requested.sort();
    assert_eq!(requested.len(), 2);
    let launch = vessels.vessels.get(&requested[0], None).expect("read").expect("launch");
    assert_eq!((launch.convoy.as_str(), launch.launch_of.as_str()), ("c#3", "c-work"), "a work's launches are of the convoy's launch_of");

    // Neither work needs a vessel now: the live convoy's launch is kept, and so is the one whose
    // convoy is not read here, until its tombstone says it is gone and the gate sanctions it.
    held.works.lock().expect("works").clear();
    held.standing.lock().expect("standing").remove(&("gone#1".to_owned(), "kiwi".to_owned()));
    let pass = requests(Arc::new(Unasked)).sync_once(Utc::now()).await.expect("pass");
    assert!(pass.released.is_empty(), "{pass:?}");
    held.standing.lock().expect("standing").insert(("gone#1".into(), "kiwi".into()), ConvoyStanding::Ended);
    let gone = BTreeSet::from([("gone#1".to_owned(), "kiwi".to_owned())]);
    assert!(requests(Arc::new(Gate(false))).sync_convoys(&gone, Utc::now()).await.expect("refused").released.is_empty());
    let pass = requests(sanctioned()).sync_convoys(&gone, Utc::now()).await.expect("sanctioned");
    assert_eq!(pass.released, vec![requested[1].clone()]);
    assert_eq!(vessels.vessels.get(&requested[0], None).expect("read").expect("launch").reading, LifecycleReading::InProgress);

    held.standing.lock().expect("standing").insert(("c#3".into(), "kiwi".into()), ConvoyStanding::Ended);
    assert!(requests(Arc::new(Gate(false))).sync_once(Utc::now()).await.expect("refused").released.is_empty());
    let pass = requests(sanctioned()).sync_once(Utc::now()).await.expect("sanctioned");
    assert_eq!(pass.released, vec![requested[0].clone()]);
    assert_eq!(vessels.vessels.get(&requested[0], None).expect("read").expect("launch").reading, LifecycleReading::Released);
}

// A pass over the convoys a change names does what a pass over every convoy does for them: it
// launches a work placed here, keeps a live convoy's launch and one whose convoy is not read here,
// holds an ended convoy's while the gate refuses (naming it refused), and releases it once the
// gate sanctions it.
#[tokio::test]
async fn a_pass_over_named_convoys_follows_them_as_a_full_pass_does() {
    let held = Arc::new(HeldWorks::default());
    *held.works.lock().expect("works") = vec![placed("c#3"), placed("gone#1")];
    for convoy in ["c#3", "gone#1"] {
        held.standing.lock().expect("standing").insert((convoy.into(), "kiwi".into()), ConvoyStanding::Live);
    }
    let vessels = VesselsFixture::at("home");
    let requests = |gate: Arc<dyn VesselReleaseGate>| {
        VesselRequests::new(held.clone(), Arc::clone(&vessels.reads), gate, CanonicalHostId::resolved("home-host"), [])
    };
    let both = BTreeSet::from([("c#3".to_owned(), "kiwi".to_owned()), ("gone#1".to_owned(), "kiwi".to_owned())]);
    let mut requested = requests(Arc::new(Unasked)).sync_convoys(&both, Utc::now()).await.expect("pass").requested;
    requested.sort();
    assert_eq!(requested.len(), 2);
    assert!(requests(Arc::new(Unasked)).sync_convoys(&both, Utc::now()).await.expect("again").requested.is_empty());

    held.works.lock().expect("works").clear();
    held.standing.lock().expect("standing").remove(&("gone#1".to_owned(), "kiwi".to_owned()));
    let pass = requests(Arc::new(Unasked)).sync_convoys(&both, Utc::now()).await.expect("pass");
    assert!(pass.released.is_empty(), "{pass:?}");
    assert_eq!(vessels.vessels.get(&requested[1], None).expect("read").expect("launch").reading, LifecycleReading::InProgress);

    held.standing.lock().expect("standing").insert(("c#3".into(), "kiwi".into()), ConvoyStanding::Ended);
    let refused = requests(Arc::new(Gate(false))).sync_convoys(&both, Utc::now()).await.expect("refused");
    assert!(refused.released.is_empty());
    assert_eq!(refused.refused, BTreeSet::from([("c#3".to_owned(), "kiwi".to_owned())]));
    let pass = requests(sanctioned()).sync_convoys(&both, Utc::now()).await.expect("sanctioned");
    assert_eq!(pass.released, vec![requested[0].clone()]);
}

// Behaviour (#3108 load run d3): the pass a convoy's change runs reads that convoy's launches by
// their index, never every launch here, so it reads as much with 20 convoys here as with 2.
#[tokio::test]
async fn a_convoy_pass_reads_as_much_with_twenty_convoys_as_with_two() {
    async fn cost(convoys: usize) -> KindReadCounts {
        let held = Arc::new(HeldWorks::default());
        let names: Vec<String> = (0..convoys).map(|n| format!("c{n}#1")).collect();
        *held.works.lock().expect("works") = names.iter().map(|name| placed(name)).collect();
        for name in &names {
            held.standing.lock().expect("standing").insert((name.clone(), "kiwi".into()), ConvoyStanding::Live);
        }
        let vessels = VesselsFixture::at("home");
        let requests = VesselRequests::new(held, Arc::clone(&vessels.reads), Arc::new(Unasked), CanonicalHostId::resolved("home-host"), []);
        assert_eq!(requests.sync_once(Utc::now()).await.expect("first pass").requested.len(), convoys);
        let one = BTreeSet::from([("c0#1".to_owned(), "kiwi".to_owned())]);
        let (pass, reads) = measure(requests.sync_convoys(&one, Utc::now())).await;
        assert!(pass.expect("pass").requested.is_empty(), "c0's launch is live");
        reads.get("Vessel").copied().unwrap_or_default()
    }
    let (two, twenty) = (cost(2).await, cost(20).await);
    assert_eq!(twenty.whole_kind, 0, "{twenty:?}");
    assert_eq!(two, twenty);
}

// A convoy homed at kiwi and placed on feta's host is launched at feta. A feta whose replica of
// the convoy is missing (its store restored, or read before replication caught up) keeps the live
// launch: a convoy it cannot read has not ended. Once the convoy's end replicates there, the launch
// is released through the gate: kept while it refuses, released once it sanctions the reclaim.
#[tokio::test]
async fn a_convoy_not_read_here_keeps_its_launches_until_its_end_is() {
    let kiwi = EngineHome::open("kiwi");
    let launched = EngineHome::open("feta");
    let convoy = admit(&kiwi, "remote", Some("feta-host"), WorkPhase::Ready).await;
    kiwi.replicate_to(&launched);
    let vessels = VesselsFixture::at("feta");
    let requests = |home: &EngineHome, gate: bool| {
        VesselRequests::new(
            std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::EngineConvoyWorks::new(
                Arc::clone(home.convoys.reads()),
                NAMESPACE,
            )),
            Arc::clone(&vessels.reads),
            Arc::new(Gate(gate)),
            CanonicalHostId::resolved("feta-host"),
            [],
        )
    };
    let launch = requests(&launched, true).sync_once(Utc::now()).await.expect("pass").requested.pop().expect("requested");

    let missing = EngineHome::open("feta");
    let key = BTreeSet::from([(convoy.clone(), "kiwi".to_owned())]);
    let pass = requests(&missing, true).sync_convoys(&key, Utc::now()).await.expect("pass");
    assert!(pass.released.is_empty(), "{pass:?}");
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::InProgress);

    flotilla_engine_convoys_testkit::land(&kiwi.convoys.path(), &kiwi.convoy(&convoy).await).await;
    kiwi.replicate_to(&missing);
    let refused = requests(&missing, false).sync_convoys(&key, Utc::now()).await.expect("refused pass");
    assert!(refused.released.is_empty());
    assert_eq!(refused.refused, key);
    let pass = requests(&missing, true).sync_convoys(&key, Utc::now()).await.expect("pass");
    assert_eq!(pass.released, vec![launch.clone()]);
    assert_eq!(vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::Released);
}
