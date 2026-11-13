//! The engine convoy controller (#3108 C4'): what its home observes of a launch becomes the
//! launch's edges and the home's notes, and the controller writes no status. The convoy here is
//! a stored object standing in for the launch's view, read through an engine that records what
//! it is asked.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_resources::{
    address::{artifact_address, record_name, stored_change_request_address},
    artifact::blob_digest_of,
    convoy::engine::{Actor, EngineConvoyPath, EngineConvoys, OldPromises, VerdictScope},
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission},
    Artifact, ArtifactSpec, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, Convoy, ConvoyPhase,
    ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, InputMeta, Observation, ObservedChangeRequestState,
    ResourceObject, VesselRequirement, WorkflowSnapshot,
};
use flotilla_store::{controller::Reconciler, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::{assert_flat_reads, ReadCountsBackendExt, ReadOperation, ReadSnapshot, ReadTally};

const NAMESPACE: &str = "flotilla";

/// An engine that records what it is asked, holds what it is told to.
#[derive(Default)]
struct Recording {
    holds: bool,
    promises: Mutex<Vec<(String, String, PromiseOperation, Actor)>>,
    views: Mutex<usize>,
    /// The engine's change feed, when the test publishes on one.
    changes: Mutex<Option<tokio::sync::broadcast::Sender<String>>>,
    /// The promises the engine holds, once handed over from the stored status.
    held: Option<OldPromises>,
    /// The disposition the engine delivered the convoy with, once it has.
    delivered: Option<String>,
    /// How many times the convoy was delivered.
    deliveries: Mutex<usize>,
    /// The reasons the convoy was ended as failed with.
    failures: Mutex<Vec<String>>,
    memory: flotilla_resources::convoy_records::ConvoyMemory,
}

#[async_trait]
impl EngineConvoys for Recording {
    fn holds(&self, _: &ResourceObject<Convoy>) -> bool {
        self.holds
    }
    async fn reserve(&self, launch_of: &str) -> Result<String, String> {
        Ok(format!("{launch_of}#1"))
    }
    async fn admit_reserved(&self, _: &ResourceObject<Convoy>) -> Result<(), String> {
        Ok(())
    }
    async fn promise(
        &self,
        _: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
        operation: &PromiseOperation,
        actor: Actor,
    ) -> Result<(), String> {
        self.promises.lock().expect("promises").push((vessel.into(), role.into(), operation.clone(), actor));
        Ok(())
    }
    async fn reassign(&self, _: &ResourceObject<Convoy>, _: &str, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn keep_by_verdict(&self, _: &ResourceObject<Convoy>, _: &VerdictScope, _: &str, _: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    async fn verdict(&self, _: &ResourceObject<Convoy>, _: &str, _: bool, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn deliver(&self, _: &ResourceObject<Convoy>, _: &str) -> Result<(), String> {
        *self.deliveries.lock().expect("deliveries") += 1;
        Ok(())
    }
    fn delivered(&self, _: &ResourceObject<Convoy>) -> Option<String> {
        self.delivered.clone()
    }
    async fn abandon(&self, _: &ResourceObject<Convoy>, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn fail(&self, _: &ResourceObject<Convoy>, _: &str, reason: &str) -> Result<(), String> {
        self.failures.lock().expect("failures").push(reason.to_owned());
        Ok(())
    }
    fn promises(&self, convoy: &ResourceObject<Convoy>, _: Option<&str>) -> Result<Option<OldPromises>, String> {
        *self.views.lock().expect("views") += 1;
        // The engine's view: the promises handed to it, else the stored promises as they are.
        Ok(self
            .holds
            .then(|| self.held.clone().unwrap_or_else(|| convoy.status.as_ref().map(|status| status.promises.clone()).unwrap_or_default())))
    }
    fn changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        match self.changes.lock().expect("changes").as_ref() {
            Some(sender) => sender.subscribe(),
            None => tokio::sync::broadcast::channel(1).1,
        }
    }
    fn memory(&self) -> &flotilla_resources::convoy_records::ConvoyMemory {
        &self.memory
    }
    async fn fail_seat(&self, _: &ResourceObject<Convoy>, _: &str, _: &str, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn link(
        &self,
        _: &ResourceObject<Convoy>,
        _: &flotilla_protocol::Subject,
        _: flotilla_resources::SubjectDiscoverySource,
        _: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn unlink(&self, _: &ResourceObject<Convoy>, _: &flotilla_protocol::Subject, _: &str) -> Result<(), String> {
        Ok(())
    }
}

/// A Working coder holding an Open decision-ledger promise, and the ledger
/// artifact it names: the old observer submits it.
async fn ledger_convoy(backend: &ResourceBackend) -> ResourceObject<Convoy> {
    let crew = CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "test".into() }).build();
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![crew]).build()],
        }),
        crew_work: BTreeMap::from([(
            "work".to_string(),
            BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
        )]),
        ..Default::default()
    };
    let declare = PromiseOperation::Declare {
        id: "template/decision-ledger".into(),
        kind: PromiseKind::DecisionLedger,
        source: PromiseSource::Template,
    };
    promises::apply(&mut status, "work", "coder", &declare);
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let created = convoys
        .create(&InputMeta::builder().name("c".to_string()).build(), &ConvoySpec::builder().workflow_ref("w".to_string()).build())
        .await
        .expect("convoy");
    publish_ledger(backend, "c").await;
    convoys.update_status("c", &created.metadata.resource_version, &status).await.expect("status")
}

/// Publishes convoy `convoy`'s coder's decision ledger artifact.
async fn publish_ledger(backend: &ResourceBackend, convoy: &str) {
    let name = record_name(&artifact_address(convoy, "coder", "decision-ledger", convoy).expect("address")).expect("record name");
    backend
        .clone()
        .using::<Artifact>(NAMESPACE)
        .create(
            &InputMeta::builder().name(name).build(),
            &ArtifactSpec::builder()
                .convoy(convoy.to_string())
                .producer("coder".to_string())
                .kind("decision-ledger".to_string())
                .subject(convoy.to_string())
                .digest(blob_digest_of(b"ledger"))
                .size(1)
                .media_type("text/markdown".to_string())
                .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("artifact");
}

/// The reads one pass over the ledger convoy makes, with `others` other convoys' ledgers held.
async fn ledger_pass_reads(others: usize) -> ReadSnapshot {
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let convoy = ledger_convoy(&backend).await;
    for n in 0..others {
        publish_ledger(&backend, &format!("other-{n}")).await;
    }
    let reconciler = reconciler(&backend, Arc::new(Recording { holds: true, ..Default::default() }));
    let before = backend.read_snapshot();
    reconciler.prepare(&convoy).await.expect("prepare");
    backend.read_snapshot().since(&before)
}

// Behaviour: a pass reads the artifacts its ledger promises name, each by its name, not every
// convoy's artifacts.
#[tokio::test]
async fn a_pass_reads_only_its_ledgers_artifacts() {
    let reads = assert_flat_reads([0, 40], ledger_pass_reads).await;
    assert_eq!(reads.calls("Artifact", ReadOperation::List), 0, "{reads}");
    assert_eq!(reads.objects("Artifact", ReadOperation::Get), 1, "{reads}");
}

/// Stores change request `number` of `github.com/org/repo`, merged, under its record name.
async fn publish_merged_change_request(backend: &ResourceBackend, number: u64) {
    let name = record_name(&stored_change_request_address("github.com", "org/repo", number, &[]).expect("address")).expect("record name");
    let requests = backend.clone().using::<ChangeRequest>(NAMESPACE);
    let spec = ChangeRequestSpec::builder()
        .service("github.com".into())
        .scope("org/repo".into())
        .number(number)
        .observing_authority("test".into())
        .build();
    let created = requests.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.expect("change request");
    let status = ChangeRequestStatus {
        state: Observation::known(ObservedChangeRequestState::Merged, chrono::Utc::now()),
        head_sha: Default::default(),
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        checks: Default::default(),
        mergeable: Default::default(),
        review: ChangeRequestReviewObservation { actionable_at_head: Default::default() },
    };
    requests.update_status(&name, &created.metadata.resource_version, &status).await.expect("status");
}

/// The reads one pass over a convoy whose coder submitted PR 1 makes, and the edges it asked
/// for, with `others` other change requests held.
async fn pr_pass_reads(others: usize) -> (ReadSnapshot, Vec<(bool, String, String)>) {
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let mut convoy = ledger_convoy(&backend).await;
    let status = convoy.status.as_mut().expect("status");
    status.promises.clear();
    let submit = PromiseOperation::Submit {
        id: "pr".into(),
        kind: PromiseKind::Pr,
        source: PromiseSource::Crew,
        submission: Submission {
            reference: "cr/github.com/org/repo/1".into(),
            metadata: BTreeMap::new(),
            submitted_at: chrono::Utc::now(),
            verdict: None,
        },
    };
    promises::apply(status, "work", "coder", &submit);
    publish_merged_change_request(&backend, 1).await;
    for number in 0..others as u64 {
        publish_merged_change_request(&backend, 100 + number).await;
    }
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine))
        .with_change_requests(backend.including_replicas::<ChangeRequest>(NAMESPACE), std::time::Duration::from_secs(180));
    let before = backend.read_snapshot();
    reconciler.prepare(&convoy).await.expect("prepare");
    let reads = backend.read_snapshot().since(&before);
    // The verdicts asked for, without their times.
    let verdicts = engine
        .promises
        .lock()
        .expect("promises")
        .iter()
        .map(|(_, _, operation, _)| match operation {
            PromiseOperation::Verdict { verdict, .. } => (verdict.accepted, verdict.who.clone(), verdict.why.clone()),
            other => panic!("a verdict, not {other:?}"),
        })
        .collect();
    (reads, verdicts)
}

// Behaviour: a pass reads the change requests its promises name, each by its record name, not
// every change request held; the merged PR still becomes the observer's accepting verdict.
#[tokio::test]
async fn a_pass_reads_only_its_change_requests() {
    let (reads, verdicts) = assert_flat_reads([0, 40], pr_pass_reads).await;
    assert_eq!(reads.calls("ChangeRequest", ReadOperation::List), 0, "{reads}");
    // Its one name's copies: this home's and the replicas', one of them held.
    assert_eq!(reads.tally("ChangeRequest", ReadOperation::Get), ReadTally { calls: 2, objects: 1 }, "{reads}");
    assert_eq!(verdicts, [(true, "forge".to_string(), "PR merged".to_string())]);
}

fn reconciler(backend: &ResourceBackend, engine: Arc<Recording>) -> ConvoyReconciler {
    reconciler_reading(backend, engine, flotilla_provisioning_testkit::CheckoutFixture::default().reads)
}

/// [`reconciler`], reading the engine's lifecycle readings through `reads`.
fn reconciler_reading(
    backend: &ResourceBackend,
    engine: Arc<Recording>,
    reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
) -> ConvoyReconciler {
    let path: EngineConvoyPath = Arc::new(OnceLock::new());
    let engine: Arc<dyn EngineConvoys> = engine;
    assert!(path.set(engine).is_ok());
    ConvoyReconciler::new(reads).with_artifacts(backend.including_replicas::<Artifact>(NAMESPACE)).with_engine(path)
}

// Behaviour: the observed ledger is the engine's Submit by an observer, and the controller writes
// no status.
#[tokio::test]
async fn an_observation_is_the_engines_edge() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoy = ledger_convoy(&backend).await;
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine));
    let prepared = reconciler.prepare(&convoy).await.expect("prepare");
    let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
    let calls = engine.promises.lock().expect("promises").clone();
    assert_eq!(calls.len(), 1);
    let (vessel, role, operation, actor) = &calls[0];
    assert_eq!((vessel.as_str(), role.as_str(), actor), ("work", "coder", &Actor::Observer));
    assert!(matches!(operation, PromiseOperation::Submit { id, .. } if id == "template/decision-ledger"));
    assert!(outcome.patch.is_none() && outcome.actuations.is_empty(), "no status write: {:?}", outcome.patch);
}

// Behaviour: an observer edge is written only when the observation changes what the engine
// holds. Each recorded edge is applied as the engine applies it; once the launch's promise
// stands where the evidence puts it, further passes over the same evidence write nothing.
#[tokio::test]
async fn an_unchanged_observation_writes_no_edge() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut convoy = ledger_convoy(&backend).await;
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine));
    let mut written = Vec::new();
    for _ in 0..4 {
        reconciler.prepare(&convoy).await.expect("prepare");
        let calls = std::mem::take(&mut *engine.promises.lock().expect("promises"));
        for (vessel, role, operation, _) in calls {
            promises::apply(convoy.status.as_mut().expect("status"), &vessel, &role, &operation);
            written.push(operation);
        }
    }
    assert!(
        matches!(written.as_slice(), [PromiseOperation::Submit { .. }, PromiseOperation::Verdict { verdict, .. }] if verdict.accepted),
        "{written:?}"
    );
    let held = &convoy.status.as_ref().expect("status").promises["work"]["coder"][0];
    assert_eq!(held.state, PromiseState::Kept);
}

// Behaviour: a convoy held at another home is that home's to judge: this home asks its engine
// nothing and notes nothing.
#[tokio::test]
async fn a_convoy_held_elsewhere_is_left_to_its_home() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoy = ledger_convoy(&backend).await;
    let engine = Arc::new(Recording::default());
    let reconciler = reconciler(&backend, Arc::clone(&engine));
    let prepared = reconciler.prepare(&convoy).await.expect("prepare");
    let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
    assert!(engine.promises.lock().expect("promises").is_empty());
    assert!(outcome.patch.is_none() && outcome.requeue_after.is_none());
    assert_eq!(engine.memory.get("c"), Default::default());
}

// Behaviour: a convoy whose view reads Failed before its launch has ended (a work failed) ends
// Failed on the engine, with the view's reason; once it has ended, its home's notes on it are
// dropped, and it is not ended again.
#[tokio::test]
async fn a_failed_work_ends_the_launch_and_an_ended_launch_is_forgotten() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut convoy = ledger_convoy(&backend).await;
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine));
    let status = convoy.status.as_mut().expect("status");
    status.phase = ConvoyPhase::Failed;
    status.message = Some("coder failed".into());
    reconciler.prepare(&convoy).await.expect("prepare failed");
    assert_eq!(*engine.failures.lock().expect("failures"), ["coder failed"]);

    engine.memory.update("c", |notes| notes.branch_subject_scan_error = Some("held".into()));
    convoy.status.as_mut().expect("status").finished_at = Some(chrono::Utc::now());
    reconciler.prepare(&convoy).await.expect("prepare ended");
    assert_eq!(engine.failures.lock().expect("failures").len(), 1, "an ended launch is not ended again");
    assert_eq!(engine.memory.get("c"), Default::default(), "an ended launch's notes are dropped");
}

// Behaviour: a convoy whose placement chose a full host notes a capacity stall in its home's
// memory and sets no timer: a change wakes it (a_host_change_wakes_only_the_convoys_waiting_on_it).
// A sleeping host's wake is its one deadline. Once the host has room the stall is cleared.
// Nothing is written to the convoy.
#[tokio::test]
async fn a_capacity_wait_is_a_note() {
    use flotilla_protocol::{CanonicalHostId, FulfilmentAllocation, FulfilmentAllocationCandidate, PlacementDecision, PlacementTargetHost};
    use flotilla_resources::{FulfilmentFacts, Host, HostSpec, HostStatus, StallCause};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let host = hosts
        .create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec { display_name: "feta".into(), ..HostSpec::default() })
        .await
        .expect("host");
    let slots = |free| HostStatus {
        ready: true,
        heartbeat_at: Some(chrono::Utc::now()),
        fulfilment_facts: BTreeMap::from([(
            "docker".to_string(),
            FulfilmentFacts { free_vessel_slots: Some(free), observed_at: chrono::Utc::now(), ..Default::default() },
        )]),
        ..Default::default()
    };
    let host = hosts.update_status("feta", &host.metadata.resource_version, &slots(0)).await.expect("full");
    let mut convoy = ledger_convoy(&backend).await;
    convoy.status.as_mut().expect("status").placement_decision = Some(PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        kind: "docker".into(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("feta"), display_name: "feta".into() },
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: Some(FulfilmentAllocation {
            chosen_kind: "docker".into(),
            reservation_reason: None,
            candidates: vec![FulfilmentAllocationCandidate {
                kind: "docker".into(),
                host: "feta".into(),
                cost_class: "owned_idle".into(),
                host_ready: true,
                sleeping_until: None,
                free_vessel_slots: Some(0),
                max_vessels: None,
                single_tenant: false,
                reserved_for_platform: false,
                minimal: true,
                available: false,
            }],
        }),
    });
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine)).with_hosts(backend.including_replicas::<Host>(NAMESPACE));
    let prepared = reconciler.prepare(&convoy).await.expect("prepare full");
    let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
    assert!(outcome.patch.is_none());
    assert_eq!(outcome.requeue_after, None, "no timer: a change wakes it");
    let stall = engine.memory.get("c").stall.expect("a capacity stall is noted");
    assert_eq!(stall.cause, Some(StallCause::Capacity));
    assert!(stall.evidence.contains("no free vessel slots"), "{}", stall.evidence);
    assert!(engine.promises.lock().expect("promises").is_empty(), "a waiting convoy observes nothing yet");

    // The host sleeps for an hour: its wake is the deadline.
    let asleep = HostStatus { sleeping_until: Some(chrono::Utc::now() + chrono::Duration::hours(1)), ..slots(1) };
    let host = hosts.update_status("feta", &host.metadata.resource_version, &asleep).await.expect("asleep");
    let prepared = reconciler.prepare(&convoy).await.expect("prepare asleep");
    let requeue = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now()).requeue_after.expect("the host's wake");
    assert!(requeue > std::time::Duration::from_secs(59 * 60) && requeue <= std::time::Duration::from_secs(3600), "{requeue:?}");

    // The view shows the note; the host wakes with a free slot.
    engine.memory.get("c").write_into(convoy.status.as_mut().expect("status"));
    hosts.update_status("feta", &host.metadata.resource_version, &slots(1)).await.expect("free");
    let prepared = reconciler.prepare(&convoy).await.expect("prepare free");
    assert_eq!(reconciler.reconcile(&convoy, &prepared, chrono::Utc::now()).requeue_after, None);
    assert_eq!(engine.memory.get("c").stall, None, "the capacity stall is cleared");
}

// Behaviour: a change of what convoys wait on wakes the convoys their home notes as waiting on
// it, and no other: a held delivery on a Message change (here any watched kind stands in;
// secondary_watches names which kinds wake which wait). A capacity wait is woken by the host it
// waits on (a_host_change_wakes_only_the_convoys_waiting_on_it). No timer is involved.
#[tokio::test]
async fn a_change_wakes_only_the_convoys_noted_as_waiting() {
    use flotilla_crew::convoy::{NoteWake, WakeOn};
    use flotilla_resources::{
        convoy_records::ConvoyNotes, Host, HostSpec, HostStatus, StallCause, StallEvidenceSource, StallRung, StalledCondition,
    };
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let stall = StalledCondition {
        leaves: Vec::new(),
        maker: None,
        evidence: "no free vessel slots".into(),
        source: StallEvidenceSource::LeafEngine,
        cause: Some(StallCause::Capacity),
        began_at: chrono::Utc::now(),
        rung: StallRung::Operator,
        supervisor: None,
        supervision_message: None,
        supervision_index: None,
        supervision_exhausted: false,
        reason: None,
        proposed_disposition: None,
        nudge_history: Vec::new(),
    };
    engine.memory.update("waits-for-capacity", |notes| notes.stall = Some(stall));
    engine.memory.update("holds-delivery", |notes| notes.delivery_held_until = Some(chrono::Utc::now()));
    engine.memory.update("neither", |notes| notes.branch_subject_scan_error = Some("noted".into()));
    let path: EngineConvoyPath = Arc::new(OnceLock::new());
    let shared: Arc<dyn EngineConvoys> = engine;
    assert!(path.set(shared).is_ok());

    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let mut host = hosts
        .create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec { display_name: "feta".into(), ..HostSpec::default() })
        .await
        .expect("host");
    let woken = "holds-delivery";
    let (sender, mut queue) = WorkQueueSender::channel();
    let wake: Box<dyn SecondaryWatch<Primary = Convoy>> = Box::new(NoteWake {
        changes: WakeOn::Replicas(backend.including_replicas::<Host>(NAMESPACE)),
        engine: Arc::clone(&path),
        waiting: ConvoyNotes::holds_delivery,
    });
    let watch = tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender));
    // The wake opens its watch when it starts: change the host until it hears.
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = HostStatus { heartbeat_at: Some(chrono::Utc::now()), ..Default::default() };
            host = hosts.update_status("feta", &host.metadata.resource_version, &status).await.expect("host change");
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
                return name;
            }
        }
    })
    .await
    .expect("woken");
    assert_eq!(first, woken);
    while let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
        assert_eq!(name, woken, "only the convoys noted as waiting on it are woken");
    }
    watch.abort();
}

/// A docker placement on `host`, as admission records it.
fn placement_on(host: &str) -> flotilla_protocol::PlacementDecision {
    use flotilla_protocol::{CanonicalHostId, FulfilmentAllocation, FulfilmentAllocationCandidate, PlacementDecision, PlacementTargetHost};
    PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        kind: "docker".into(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved(host), display_name: host.into() },
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: Some(FulfilmentAllocation {
            chosen_kind: "docker".into(),
            reservation_reason: None,
            candidates: vec![FulfilmentAllocationCandidate {
                kind: "docker".into(),
                host: host.into(),
                cost_class: "owned_idle".into(),
                host_ready: true,
                sleeping_until: None,
                free_vessel_slots: Some(0),
                max_vessels: None,
                single_tenant: false,
                reserved_for_platform: false,
                minimal: true,
                available: false,
            }],
        }),
    }
}

/// Stores host `name`, ready, with `free` docker vessel slots.
async fn host_with_slots(backend: &ResourceBackend, name: &str, free: u32) -> ResourceObject<flotilla_resources::Host> {
    use flotilla_resources::{FulfilmentFacts, Host, HostSpec, HostStatus};
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let status = HostStatus {
        ready: true,
        heartbeat_at: Some(chrono::Utc::now()),
        fulfilment_facts: BTreeMap::from([(
            "docker".to_string(),
            FulfilmentFacts { free_vessel_slots: Some(free), observed_at: chrono::Utc::now(), ..Default::default() },
        )]),
        ..Default::default()
    };
    match hosts.get(name).await {
        Ok(host) => hosts.update_status(name, &host.metadata.resource_version, &status).await.expect("host status"),
        Err(_) => {
            let host = hosts
                .create(
                    &InputMeta::builder().name(name.to_string()).build(),
                    &HostSpec { display_name: name.into(), ..HostSpec::default() },
                )
                .await
                .expect("host");
            hosts.update_status(name, &host.metadata.resource_version, &status).await.expect("host status")
        }
    }
}

// Behaviour: a capacity wait notes in its home's memory the host it waits on, so that host's
// changes wake it, and a pass that no longer waits notes none.
#[tokio::test]
async fn a_capacity_wait_notes_the_host_it_waits_on() {
    use flotilla_resources::Host;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    host_with_slots(&backend, "feta", 0).await;
    let mut convoy = ledger_convoy(&backend).await;
    convoy.status.as_mut().expect("status").placement_decision = Some(placement_on("feta"));
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine)).with_hosts(backend.including_replicas::<Host>(NAMESPACE));

    reconciler.prepare(&convoy).await.expect("prepare full");
    assert!(engine.memory.get("c").waits_for_capacity());
    assert_eq!(engine.memory.reading("Host", "feta"), ["c"]);

    // The view shows the note; the host frees a slot.
    engine.memory.get("c").write_into(convoy.status.as_mut().expect("status"));
    host_with_slots(&backend, "feta", 1).await;
    reconciler.prepare(&convoy).await.expect("prepare free");
    assert!(!engine.memory.get("c").waits_for_capacity());
    assert!(engine.memory.readers("Host").is_empty(), "a convoy with room waits on no host");
}

/// What hearing one change of host `feta` reads, from when the wake has started, and which
/// convoys it wakes, with `others` convoys each waiting on a host of its own.
async fn capacity_wake_reads(others: usize) -> (ReadSnapshot, BTreeSet<String>) {
    use flotilla_crew::convoy::{host_references, CapacityWake, WakeOn};
    use flotilla_resources::Host;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    host_with_slots(&backend, "feta", 0).await;
    for n in 0..others {
        host_with_slots(&backend, &format!("host-{n}"), 0).await;
    }
    // A convoy waiting on the host the test changes until the wake hears.
    engine.memory.note_evidence("probe", "Host", ["probe-host".to_string()].into());

    let (sender, mut queue) = WorkQueueSender::channel();
    let wake: Box<dyn SecondaryWatch<Primary = Convoy>> = Box::new(CapacityWake {
        changes: WakeOn::Replicas(backend.including_replicas::<Host>(NAMESPACE)),
        engine: engine_path(&engine),
        hosts: host_references,
    });
    let watch = tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender));
    // The wake queues "probe" once its watch is open.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            host_with_slots(&backend, "probe-host", 0).await;
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
                assert_eq!(name, "probe");
                return;
            }
        }
    })
    .await
    .expect("woken");
    while let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
        assert_eq!(name, "probe");
    }
    // Noted once the wake is open: a queue holds a name until a controller takes it.
    engine.memory.note_evidence("c", "Host", ["feta".to_string()].into());
    for n in 0..others {
        engine.memory.note_evidence(&format!("other-{n}"), "Host", [format!("host-{n}")].into());
    }

    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let feta = hosts.get("feta").await.expect("feta");
    // Only what the wake reads: the test's own get of the host it changes comes before.
    let before = backend.read_snapshot();
    let status = flotilla_resources::HostStatus { heartbeat_at: Some(chrono::Utc::now()), ..feta.status.clone().unwrap_or_default() };
    hosts.update_status("feta", &feta.metadata.resource_version, &status).await.expect("feta change");
    let woken = woken(&mut queue).await;
    let reads = backend.read_snapshot().since(&before);
    watch.abort();
    (reads, woken)
}

// Behaviour: a change of a host wakes only the convoys waiting for capacity on it, reading
// nothing to find them, at any number of other waiting convoys and hosts.
#[tokio::test]
async fn a_host_change_wakes_only_the_convoys_waiting_on_it() {
    let (reads, woken) = assert_flat_reads([0, 40], capacity_wake_reads).await;
    reads.assert_zero("hearing a host change");
    assert_eq!(woken, BTreeSet::from(["c".to_string()]));
}

// Behaviour: what each capacity wake reads a changed record as standing on: a host by its name
// and display name, a vessel or convoy by the host its placement targets, an environment by its
// host; a convoy with no placement stands on none.
#[tokio::test]
async fn a_capacity_change_names_the_hosts_it_is_on() {
    use flotilla_crew::convoy::{convoy_hosts, environment_hosts, host_references, vessel_hosts};
    use flotilla_resources::{Environment, EnvironmentSpec, HostDirectEnvironmentSpec, HostSpec, Vessel, VesselSpec, VesselStatus};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut host = host_with_slots(&backend, "feta", 0).await;
    host.spec = HostSpec { display_name: "Feta".into(), ..HostSpec::default() };
    assert_eq!(host_references(&host), BTreeSet::from(["Feta".to_string(), "feta".to_string()]));

    let mut convoy = ledger_convoy(&backend).await;
    assert!(convoy_hosts(&convoy).is_empty(), "no placement, no host");
    convoy.status.as_mut().expect("status").placement_decision = Some(placement_on("feta"));
    let pin = flotilla_resources::convoy::VesselPlacementPin { kind_ref: "docker".into(), decision: placement_on("brie") };
    convoy.metadata.annotations.insert(
        flotilla_resources::convoy::VESSEL_PLACEMENTS_ANNOTATION.into(),
        serde_json::to_string(&BTreeMap::from([("work".to_string(), pin)])).expect("pins"),
    );
    assert_eq!(convoy_hosts(&convoy), BTreeSet::from(["brie".to_string(), "feta".to_string()]));

    // Any object's metadata will do: the hosts are read from spec and status.
    let vessel = ResourceObject::<Vessel> {
        metadata: convoy.metadata.clone(),
        spec: VesselSpec {
            convoy_ref: "c".into(),
            vessel_name: "work".into(),
            fulfilment_kind_ref: "docker".into(),
            adopted_checkout_refs: BTreeMap::new(),
        },
        status: Some(VesselStatus { placement_decision: Some(placement_on("feta")), ..Default::default() }),
    };
    assert_eq!(vessel_hosts(&vessel), BTreeSet::from(["feta".to_string()]));

    let environment = ResourceObject::<Environment> {
        metadata: convoy.metadata.clone(),
        spec: EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "feta".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        },
        status: None,
    };
    assert_eq!(environment_hosts(&environment), BTreeSet::from(["feta".to_string()]));
}

// Behaviour (#3108): a Message committed on the engine's Message path wakes the convoys noted as
// holding delivery, and no other, through the wake secondary_watches builds; hearing it loads
// and converts no message, where a listing of the kind would.
#[tokio::test]
async fn a_message_commit_wakes_the_convoys_holding_delivery_without_listing_messages() {
    use flotilla_crew::ports::MessagePath;
    use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
    use flotilla_messaging::EngineMessages;
    use flotilla_resources::{MessageRelation, MessageSpec};
    use flotilla_store::controller::WorkQueueSender;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let primitives = Arc::clone(home.primitives());
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    engine.memory.update("holds-delivery", |notes| notes.delivery_held_until = Some(chrono::Utc::now()));
    engine.memory.update("neither", |notes| notes.branch_subject_scan_error = Some("noted".into()));
    let path: EngineConvoyPath = Arc::new(OnceLock::new());
    let shared: Arc<dyn EngineConvoys> = engine;
    assert!(path.set(shared).is_ok());
    let messages: MessagePath = Arc::new(OnceLock::new());
    let installed: Arc<dyn EngineMessages> = Arc::clone(&primitives) as Arc<dyn EngineMessages>;
    assert!(messages.set(installed).is_ok());
    let admit = |n: usize| {
        let primitives = Arc::clone(&primitives);
        async move {
            let intent = MessageSpec::builder()
                .sender("system:turn-rules".into())
                .receiver("flotilla/c/work/coder".into())
                .relation(MessageRelation::System)
                .body(format!("turn {n}"))
                .build();
            primitives.admit(NAMESPACE, &format!("turn-{n}"), &intent).await.expect("admit");
        }
    };
    // Admission reads back the message it admits: what an admission costs, with nothing listening.
    let unheard = primitives.rows_read();
    for n in 0..4 {
        admit(n).await;
    }
    let admitted = primitives.rows_read();
    let admission = ((admitted.loaded - unheard.loaded) / 4, (admitted.converted - unheard.converted) / 4);

    // Every wake the convoy controller runs: only the delivery wake has a convoy to wake.
    let (sender, mut queue) = WorkQueueSender::channel();
    let before = primitives.rows_read();
    let watches = ConvoyReconciler::secondary_watches(&backend, NAMESPACE, Arc::clone(&path), messages)
        .into_iter()
        .map(|wake| tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender.clone())))
        .collect::<Vec<_>>();
    // The wake subscribes when it starts: admit until it hears.
    let mut n = 4;
    let woken = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            admit(n).await;
            n += 1;
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
                return name;
            }
        }
    })
    .await
    .expect("woken");
    assert_eq!(woken, "holds-delivery");
    while let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
        assert_eq!(name, "holds-delivery", "only the convoys noted as holding delivery are woken");
    }
    watches.iter().for_each(tokio::task::JoinHandle::abort);
    let heard = primitives.rows_read();
    let admissions = (n - 4) as u64;
    assert_eq!(
        (heard.loaded - before.loaded, heard.converted - before.converted),
        (admission.0 * admissions, admission.1 * admissions),
        "beyond its admissions, hearing a Message commit loads and converts no message"
    );

    // The meter sees a listing: opening the kind's generic watch lists every message.
    let before = heard;
    let _listed = backend.including_replicas::<flotilla_resources::Message>(NAMESPACE).watch().await.expect("listing watch");
    assert!(primitives.rows_read().loaded > before.loaded, "a listing of the kind loads the messages");
}

// Behaviour (#3732 review): each engine edit's convoy name wakes the convoy reconciler through
// EngineConvoyWake.
#[tokio::test]
async fn engine_changes_wake_the_convoy_reconciler() {
    use flotilla_crew::convoy::EngineConvoyWake;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default());

    let (feed, _) = tokio::sync::broadcast::channel(16);
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    *engine.changes.lock().expect("changes") = Some(feed.clone());
    let path: EngineConvoyPath = Arc::new(OnceLock::new());
    let shared: Arc<dyn EngineConvoys> = engine;
    assert!(path.set(shared).is_ok());
    let (sender, mut queue) = WorkQueueSender::channel();
    let on: Box<dyn SecondaryWatch<Primary = Convoy>> = Box::new(EngineConvoyWake { engine: path });
    let watch = tokio::spawn(on.spawn(backend, "flotilla".into(), sender));
    // The watch subscribes when it starts: publish until it hears.
    let woken = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let _ = feed.send("c".into());
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(20), queue.recv()).await {
                return name;
            }
        }
    })
    .await
    .expect("woken");
    assert_eq!(woken, "c");
    watch.abort();
}

fn change_request_name(number: u64) -> String {
    record_name(&stored_change_request_address("github.com", "org/repo", number, &[]).expect("address")).expect("record name")
}

/// The record name of convoy `convoy`'s coder's decision ledger slot.
fn ledger_name(convoy: &str) -> String {
    record_name(&artifact_address(convoy, "coder", "decision-ledger", convoy).expect("address")).expect("record name")
}

/// [`ledger_convoy`], whose coder has also submitted PR `number`.
async fn pr_ledger_convoy(backend: &ResourceBackend, number: u64) -> ResourceObject<Convoy> {
    let mut convoy = ledger_convoy(backend).await;
    submit_pr(&mut convoy, number);
    convoy
}

/// The coder of `convoy` submits PR `number`.
fn submit_pr(convoy: &mut ResourceObject<Convoy>, number: u64) {
    let submit = PromiseOperation::Submit {
        id: "pr".into(),
        kind: PromiseKind::Pr,
        source: PromiseSource::Crew,
        submission: Submission {
            reference: format!("cr/github.com/org/repo/{number}"),
            metadata: BTreeMap::new(),
            submitted_at: chrono::Utc::now(),
            verdict: None,
        },
    };
    promises::apply(convoy.status.as_mut().expect("status"), "work", "coder", &submit);
}

/// An engine path holding `engine`, for a wake beside the reconciler that reads through it.
fn engine_path(engine: &Arc<Recording>) -> EngineConvoyPath {
    let path: EngineConvoyPath = Arc::new(OnceLock::new());
    let shared: Arc<dyn EngineConvoys> = Arc::clone(engine) as Arc<dyn EngineConvoys>;
    assert!(path.set(shared).is_ok());
    path
}

/// The names queued within 100 ms of the first, which comes within 5 s.
async fn woken(queue: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> BTreeSet<String> {
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), queue.recv()).await.expect("woken").expect("queue");
    let mut woken = BTreeSet::from([first]);
    while let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(100), queue.recv()).await {
        woken.insert(name);
    }
    woken
}

// Behaviour: a pass notes in its home's memory the evidence records it reads, the change request
// its PR promise names and the artifact its ledger promise names, and an ended convoy reads none.
#[tokio::test]
async fn a_pass_notes_the_evidence_it_reads() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut convoy = pr_ledger_convoy(&backend, 1).await;
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine))
        .with_change_requests(backend.including_replicas::<ChangeRequest>(NAMESPACE), std::time::Duration::from_secs(180));
    reconciler.prepare(&convoy).await.expect("prepare");
    assert_eq!(engine.memory.reading("ChangeRequest", &change_request_name(1)), ["c"]);
    assert_eq!(engine.memory.reading("Artifact", &ledger_name("c")), ["c"]);

    convoy.status.as_mut().expect("status").phase = ConvoyPhase::Failed;
    reconciler.prepare(&convoy).await.expect("prepare");
    assert!(engine.memory.reading("ChangeRequest", &change_request_name(1)).is_empty(), "an ended convoy reads no evidence");
    assert!(engine.memory.reading("Artifact", &ledger_name("c")).is_empty(), "an ended convoy reads no evidence");
}

// Behaviour: a pass notes the names it looks up, found or not, so a change request or ledger
// artifact created later under one of them wakes the convoy, and a convoy that read other records
// is not woken. A ledger's artifact is looked up by its slot's record name (convoy, role, kind
// and subject), so an artifact the convoy has never seen reaches it under that name.
#[tokio::test]
async fn a_record_created_after_the_pass_wakes_the_convoy_that_looked_for_it() {
    use flotilla_crew::convoy::EvidenceWake;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    // The convoy's view is built on a backend of its own, so neither its ledger nor its PR exists
    // where the pass looks.
    let convoy = pr_ledger_convoy(&ResourceBackend::new(InMemoryBackend::default()), 1).await;
    let engine = Arc::new(Recording { holds: true, ..Default::default() });

    // Probes a convoy reads, written until each wake has started and heard one.
    engine.memory.note_evidence("probe", "ChangeRequest", (10_000..10_500).map(change_request_name).collect());
    engine.memory.note_evidence("probe", "Artifact", (0..500).map(|n| ledger_name(&format!("probe-{n}"))).collect());
    // A queue holds a name until the controller takes it, which a test never does, so each wake
    // has its own queue, and the convoys under test are noted only once the wakes are open: a
    // wake queues every convoy already noted when it opens.
    let (change_request_sender, mut change_request_queue) = WorkQueueSender::channel();
    let (artifact_sender, mut artifact_queue) = WorkQueueSender::channel();
    let wakes: Vec<(Box<dyn SecondaryWatch<Primary = Convoy>>, WorkQueueSender)> = vec![
        (
            Box::new(EvidenceWake { evidence: backend.including_replicas::<ChangeRequest>(NAMESPACE), engine: engine_path(&engine) }),
            change_request_sender,
        ),
        (
            Box::new(EvidenceWake { evidence: backend.including_replicas::<Artifact>(NAMESPACE), engine: engine_path(&engine) }),
            artifact_sender,
        ),
    ];
    let watches =
        wakes.into_iter().map(|(wake, sender)| tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender))).collect::<Vec<_>>();
    let mut probe = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            publish_merged_change_request(&backend, 10_000 + probe).await;
            probe += 1;
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), change_request_queue.recv()).await {
                assert_eq!(name, "probe");
                return;
            }
        }
    })
    .await
    .expect("the change request wake heard a probe");
    let mut probe = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            publish_ledger(&backend, &format!("probe-{probe}")).await;
            probe += 1;
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), artifact_queue.recv()).await {
                assert_eq!(name, "probe");
                return;
            }
        }
    })
    .await
    .expect("the artifact wake heard a probe");

    engine.memory.note_evidence("unrelated", "ChangeRequest", [change_request_name(2)].into());
    engine.memory.note_evidence("unrelated", "Artifact", [ledger_name("unrelated")].into());
    let reconciler = reconciler(&backend, Arc::clone(&engine))
        .with_change_requests(backend.including_replicas::<ChangeRequest>(NAMESPACE), std::time::Duration::from_secs(180));
    reconciler.prepare(&convoy).await.expect("prepare");
    assert!(engine.promises.lock().expect("promises").is_empty(), "the pass found nothing to observe");

    publish_merged_change_request(&backend, 1).await;
    assert_eq!(woken(&mut change_request_queue).await, BTreeSet::from(["c".to_string()]), "the PR the convoy looked for wakes it alone");
    publish_ledger(&backend, "c").await;
    assert_eq!(woken(&mut artifact_queue).await, BTreeSet::from(["c".to_string()]), "the ledger the convoy looked for wakes it alone");
    watches.iter().for_each(tokio::task::JoinHandle::abort);
}

// Behaviour: after a restart the home's memory is empty, so a change before a convoy's first
// pass wakes nothing; that pass, which the controller queues for every convoy from its initial
// listing (store-testkit controller_loop_reconciles_existing_primary_objects_from_initial_list),
// reads the change, and notes the record so the next change wakes it.
#[tokio::test]
async fn after_a_restart_the_first_pass_reads_what_changed_before_it() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    // Only the PR promise, so the pass's one observation is its verdict.
    let mut convoy = ledger_convoy(&backend).await;
    convoy.status.as_mut().expect("status").promises.clear();
    submit_pr(&mut convoy, 1);
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    publish_merged_change_request(&backend, 1).await;
    assert!(engine.memory.reading("ChangeRequest", &change_request_name(1)).is_empty(), "nothing is noted before the first pass");

    let reconciler = reconciler(&backend, Arc::clone(&engine))
        .with_change_requests(backend.including_replicas::<ChangeRequest>(NAMESPACE), std::time::Duration::from_secs(180));
    reconciler.prepare(&convoy).await.expect("prepare");
    let verdicts = engine
        .promises
        .lock()
        .expect("promises")
        .iter()
        .filter_map(|(_, _, operation, _)| match operation {
            PromiseOperation::Verdict { verdict, .. } => Some((verdict.accepted, verdict.why.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(verdicts, [(true, "PR merged".to_string())], "the first pass reads the merge that came before it");
    assert_eq!(engine.memory.reading("ChangeRequest", &change_request_name(1)), ["c"]);
}

// Behaviour: after a restart, with the home's memory empty, the controller's start-up listing gives
// an open convoy its first pass, which notes the change request its PR names though it is not yet
// held, and that change request's later merge wakes the convoy with no other event.
#[tokio::test]
async fn after_a_restart_the_start_up_pass_lets_a_later_change_wake_the_convoy() {
    use flotilla_crew::convoy::EvidenceWake;
    use flotilla_store::controller::{ControllerLoop, SecondaryWatch};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut convoy = ledger_convoy(&backend).await;
    // Only the PR promise, so the merge is the pass's one observation.
    convoy.status.as_mut().expect("status").promises.clear();
    submit_pr(&mut convoy, 1);
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    convoys.update_status("c", &convoy.metadata.resource_version, convoy.status.as_ref().expect("status")).await.expect("status");
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let verdicts = || {
        engine
            .promises
            .lock()
            .expect("promises")
            .iter()
            .filter_map(|(_, _, operation, _)| match operation {
                PromiseOperation::Verdict { verdict, .. } => Some((verdict.accepted, verdict.why.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    let wake: Box<dyn SecondaryWatch<Primary = Convoy>> =
        Box::new(EvidenceWake { evidence: backend.including_replicas::<ChangeRequest>(NAMESPACE), engine: engine_path(&engine) });
    let mut harness = crate::common::ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: convoys,
            secondaries: vec![wake],
            reconciler: reconciler(&backend, Arc::clone(&engine))
                .with_change_requests(backend.including_replicas::<ChangeRequest>(NAMESPACE), std::time::Duration::from_secs(180)),
            backend: backend.clone(),
        }
        .run(),
    );
    harness
        .wait_until(std::time::Duration::from_secs(5), || async {
            engine.memory.reading("ChangeRequest", &change_request_name(1)) == ["c"]
        })
        .await;
    assert!(verdicts().is_empty(), "the first pass finds no change request to judge");

    publish_merged_change_request(&backend, 1).await;
    harness.wait_until(std::time::Duration::from_secs(5), || async { !verdicts().is_empty() }).await;
    // The change request's creation and its status each wake the convoy, so it may judge twice.
    assert!(verdicts().iter().all(|verdict| *verdict == (true, "PR merged".to_string())), "the merge woke the convoy: {:?}", verdicts());
    harness.shutdown().await;
}

/// What hearing one change of convoy c's change request reads, from when the wake has started,
/// and which convoys it wakes, with `others` convoys each reading another change request. Also
/// the Convoy reads from before the wake starts.
async fn evidence_wake_reads(others: usize) -> (ReadSnapshot, usize, BTreeSet<String>) {
    use flotilla_crew::convoy::EvidenceWake;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    for n in 0..others as u64 {
        publish_merged_change_request(&backend, 100 + n).await;
    }
    // A convoy reading the probes the test writes until the wake hears.
    engine.memory.note_evidence("probe", "ChangeRequest", (10_000..10_500).map(change_request_name).collect());

    let started = backend.read_snapshot();
    let (sender, mut queue) = WorkQueueSender::channel();
    let wake: Box<dyn SecondaryWatch<Primary = Convoy>> =
        Box::new(EvidenceWake { evidence: backend.including_replicas::<ChangeRequest>(NAMESPACE), engine: engine_path(&engine) });
    let watch = tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender));
    // The wake opens its watch when it starts: write probes until it hears one.
    let mut probe = 10_000;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            publish_merged_change_request(&backend, probe).await;
            probe += 1;
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
                assert_eq!(name, "probe");
                return;
            }
        }
    })
    .await
    .expect("woken");
    while let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
        assert_eq!(name, "probe");
    }
    // The convoys under test are noted once the wake is open, which queues every convoy noted
    // when it opens, and a queue holds a name until a controller takes it.
    engine.memory.note_evidence("c", "ChangeRequest", [change_request_name(1)].into());
    for n in 0..others as u64 {
        engine.memory.note_evidence(&format!("other-{n}"), "ChangeRequest", [change_request_name(100 + n)].into());
    }
    let convoy_reads = backend.read_snapshot().since(&started).calls_by_kind().get("Convoy").copied().unwrap_or_default();

    let before = backend.read_snapshot();
    publish_merged_change_request(&backend, 1).await;
    // The queue holds a name once until it is taken, so its creation and status may wake it once.
    let woken = woken(&mut queue).await;
    let reads = backend.read_snapshot().since(&before);
    watch.abort();
    (reads, convoy_reads, woken)
}

// Behaviour: a change of a change request wakes only the convoys whose pass read it, reading
// nothing to find them, at any number of other convoys and change requests.
#[tokio::test]
async fn an_evidence_change_wakes_only_the_convoys_that_read_it() {
    let (reads, convoy_reads, woken) = assert_flat_reads([0, 40], evidence_wake_reads).await;
    reads.assert_zero("hearing a change request change");
    assert_eq!(convoy_reads, 0, "the wake reads no convoy");
    assert_eq!(woken, BTreeSet::from(["c".to_string()]));
}

// Behaviour: a convoy whose pass read evidence before the wake opened its watch may have missed a
// change in between, so the wake queues every convoy noted for its kind once its watch is open,
// and none noted only for another kind, reading no convoy.
#[tokio::test]
async fn an_opening_wake_queues_the_convoys_already_reading_its_kind() {
    use flotilla_crew::convoy::EvidenceWake;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    engine.memory.note_evidence("c", "ChangeRequest", [change_request_name(1)].into());
    engine.memory.note_evidence("d", "ChangeRequest", [change_request_name(2)].into());
    engine.memory.note_evidence("ledger-only", "Artifact", [ledger_name("ledger-only")].into());

    let before = backend.read_snapshot();
    let (sender, mut queue) = WorkQueueSender::channel();
    let wake: Box<dyn SecondaryWatch<Primary = Convoy>> =
        Box::new(EvidenceWake { evidence: backend.including_replicas::<ChangeRequest>(NAMESPACE), engine: engine_path(&engine) });
    let watch = tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender));
    assert_eq!(woken(&mut queue).await, BTreeSet::from(["c".to_string(), "d".to_string()]));
    let reads = backend.read_snapshot().since(&before);
    watch.abort();
    assert_eq!(reads.calls_by_kind().get("Convoy").copied().unwrap_or_default(), 0, "{reads}");
}

/// The Forges the engine serves, or a refusal to list them while `failing`.
#[derive(Debug, Default)]
struct Forges {
    failing: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl flotilla_resources::engine_records::EngineRecords for Forges {
    fn kinds(&self) -> &'static [&'static str] {
        &["Forge"]
    }
    fn home(&self) -> String {
        "kiwi".into()
    }
    async fn get(&self, _: &str, _: &str) -> Result<Option<flotilla_resources::engine_records::EngineObject>, String> {
        self.list("Forge").await.map(|_| None)
    }
    async fn list(&self, kind: &str) -> Result<Vec<flotilla_resources::engine_records::EngineObject>, String> {
        self.sources(kind).await
    }
    async fn sources(&self, _: &str) -> Result<Vec<flotilla_resources::engine_records::EngineObject>, String> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the forges are unreadable".into());
        }
        Ok(Vec::new())
    }
    async fn apply(
        &self,
        _: &str,
        _: &str,
        _: &BTreeMap<String, String>,
        _: &BTreeMap<String, String>,
        _: serde_json::Value,
    ) -> Result<flotilla_resources::engine_records::EngineObject, String> {
        Err("read only".into())
    }
    async fn set_status(&self, _: &str, _: &str, _: serde_json::Value) -> Result<flotilla_resources::engine_records::EngineObject, String> {
        Err("read only".into())
    }
    async fn delete(&self, _: &str, _: &str) -> Result<(), String> {
        Err("read only".into())
    }
    async fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, String> {
        Ok(futures::StreamExt::boxed(futures::stream::pending()))
    }
}

// Behaviour: a convoy whose pass fails before it has noted which change requests it reads, and
// that no earlier pass noted, is woken by any change request, as every open convoy once was; a
// pass that completes notes only the ones it reads, and a later failing pass keeps that note.
#[tokio::test]
async fn a_convoy_failing_before_its_first_note_is_woken_by_any_change_request() {
    use flotilla_crew::convoy::EvidenceWake;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let forges = Arc::new(Forges { failing: true.into() });
    backend.install_engine_records(Arc::clone(&forges) as Arc<dyn flotilla_resources::engine_records::EngineRecords>);
    let mut convoy = ledger_convoy(&backend).await;
    convoy.status.as_mut().expect("status").promises.clear();
    submit_pr(&mut convoy, 1);
    let engine = Arc::new(Recording { holds: true, ..Default::default() });
    let reconciler = reconciler(&backend, Arc::clone(&engine))
        .with_change_requests(backend.including_replicas::<ChangeRequest>(NAMESPACE), std::time::Duration::from_secs(180))
        .with_forges(backend.definitions::<flotilla_resources::Forge>(NAMESPACE));

    // A probe convoy reads change requests, written until the wake has started and heard one.
    engine.memory.note_evidence("probe", "ChangeRequest", (10_000..10_500).map(change_request_name).collect());
    let (sender, mut queue) = WorkQueueSender::channel();
    let wake: Box<dyn SecondaryWatch<Primary = Convoy>> =
        Box::new(EvidenceWake { evidence: backend.including_replicas::<ChangeRequest>(NAMESPACE), engine: engine_path(&engine) });
    let watch = tokio::spawn(wake.spawn(backend.clone(), NAMESPACE.into(), sender));
    let mut probe = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            publish_merged_change_request(&backend, 10_000 + probe).await;
            probe += 1;
            if let Ok(Some(name)) = tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv()).await {
                assert_eq!(name, "probe");
                return;
            }
        }
    })
    .await
    .expect("the wake heard a probe");

    reconciler.prepare(&convoy).await.expect_err("the forges are unreadable");
    publish_merged_change_request(&backend, 2).await;
    assert_eq!(woken(&mut queue).await, BTreeSet::from(["c".to_string()]), "an unrelated change request wakes the failing convoy");
    watch.abort();

    // A queue holds "c" until a controller takes it, so what wakes it now is read from the notes
    // each change is matched against.
    forges.failing.store(false, std::sync::atomic::Ordering::SeqCst);
    reconciler.prepare(&convoy).await.expect("prepare");
    assert!(engine.memory.reading("ChangeRequest", &change_request_name(3)).is_empty(), "a completed pass reads only its own");
    assert_eq!(engine.memory.reading("ChangeRequest", &change_request_name(1)), ["c"]);

    forges.failing.store(true, std::sync::atomic::Ordering::SeqCst);
    reconciler.prepare(&convoy).await.expect_err("the forges are unreadable");
    assert!(engine.memory.reading("ChangeRequest", &change_request_name(3)).is_empty(), "a failed pass keeps the last note");
    assert_eq!(engine.memory.reading("ChangeRequest", &change_request_name(1)), ["c"]);
}

/// The engine Message path's one answer the hold reads: until when the crew's
/// undelivered turn holds delivery. The convoy reconciler asks it nothing else.
struct Turns {
    backend: ResourceBackend,
    held_until: Option<chrono::DateTime<chrono::Utc>>,
}

#[async_trait]
impl flotilla_messaging::EngineMessages for Turns {
    async fn view_to(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler reads no message views")
    }
    async fn view_for(
        &self,
        _: &str,
        _: &flotilla_resources::MessageSpec,
        _: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler reads no message views")
    }
    async fn open_to(
        &self,
        _: &str,
        _: &[String],
    ) -> Result<Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler reads no message views")
    }
    async fn seat_messages(
        &self,
        _: &str,
        _: &[String],
        _: &[(String, String)],
    ) -> Result<Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler reads no message views")
    }
    async fn pending_to(
        &self,
        _: &str,
        _: &[String],
    ) -> Result<Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler reads no message views")
    }
    async fn admit(
        &self,
        _: &str,
        _: &str,
        _: &flotilla_resources::MessageSpec,
    ) -> Result<ResourceObject<flotilla_resources::Message>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler admits nothing")
    }
    async fn spelled(
        &self,
        _: &str,
        _: &flotilla_resources::MessageSpec,
    ) -> Result<flotilla_resources::MessageSpec, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler compares no parties")
    }
    async fn resolve_in_reply_to(&self, _: &str, _: &flotilla_resources::MessageSpec) -> flotilla_messaging::MessageResolution {
        unreachable!("the convoy reconciler links no replies")
    }
    async fn withdraw(&self, _: &str, _: &str, _: &str, _: &str) -> Result<(), String> {
        unreachable!("the convoy reconciler withdraws nothing")
    }
    fn home(&self) -> String {
        "home-a".into()
    }
    async fn await_reply(
        &self,
        _: &str,
        _: &str,
        _: &flotilla_resources::MessageSpec,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<bool, String> {
        unreachable!("the convoy reconciler records no replies")
    }
    async fn end_stalls(&self, _: &str, _: &[String], _: Option<&str>, _: &str) -> Result<Vec<String>, String> {
        unreachable!("the convoy reconciler ends no stalls")
    }
    async fn delivery_held_until(&self, namespace: &str, receiver: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
        assert_eq!((namespace, receiver), (NAMESPACE, "flotilla/c/work/coder"));
        Ok(self.held_until)
    }
    async fn delivery_wait(&self, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, flotilla_resources::ResourceError> {
        Ok(Box::pin(futures::stream::pending()))
    }
    fn charter_notices(self: Arc<Self>, namespace: &str) -> Box<dyn flotilla_messaging::CharterNotices> {
        Box::new(flotilla_messaging::MessageInbox::new(self.backend.clone(), namespace))
    }
}

// Behaviour (#3894): a landing convoy is delivered only once no turn to a live crew session is
// undelivered; while one is, delivery holds, noted in its home's memory so a Message change wakes
// it, with the hold's end as its one deadline. The engine's reading of the session's launch
// decides whether it is live.
#[tokio::test]
async fn an_undelivered_turn_holds_delivery() {
    use flotilla_provisioning_testkit::{EngineFixture, TerminalsFixture};
    for (awaiting, live) in [(true, true), (false, true), (true, false)] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let mut convoy = ledger_convoy(&backend).await;
        let status = convoy.status.as_mut().expect("status");
        status.phase = ConvoyPhase::Landing;
        // With no subject and no checkout, the standard exit settles as a claim exit.
        status.workflow_snapshot.as_mut().expect("snapshot").exit = Some(flotilla_resources::ExitDeclaration::standard_table());
        for promise in status.promises.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("promises") {
            promise.state = PromiseState::Kept;
        }
        status.crew_work.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("coder").phase = CrewWorkPhase::Done;
        // The session's holder is the work's engine launch, which names its convoy and work.
        let engine = EngineFixture::default();
        let holder = engine.vessels.request("c", "work", "home");
        let fixture = engine.terminals;
        let mut request = TerminalsFixture::agent(&holder, "coder", "env");
        request.session = "coder-session".into();
        let launch = fixture.request(&request);
        fixture.ready(&launch, "sid", &Default::default());
        if !live {
            fixture.release(&launch);
        }
        let messages: flotilla_crew::ports::MessagePath = Arc::new(OnceLock::new());
        let held_until = awaiting.then(|| chrono::Utc::now() + chrono::Duration::minutes(5));
        let turns: Arc<dyn flotilla_messaging::EngineMessages> = Arc::new(Turns { backend: backend.clone(), held_until });
        assert!(messages.set(turns).is_ok());
        let engine = Arc::new(Recording { holds: true, ..Default::default() });
        let reconciler = reconciler_reading(&backend, Arc::clone(&engine), fixture.reads.clone()).with_messages(messages);
        let prepared = reconciler.prepare(&convoy).await.expect("prepare");
        let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
        let delivered = *engine.deliveries.lock().expect("deliveries") == 1;
        assert_eq!(delivered, !(awaiting && live), "awaiting {awaiting}, live {live}");
        let noted = engine.memory.get("c").delivery_held_until;
        if awaiting && live {
            let requeue = outcome.requeue_after.expect("the hold's end");
            assert!(requeue > std::time::Duration::from_secs(4 * 60) && requeue <= std::time::Duration::from_secs(5 * 60), "{requeue:?}");
            assert_eq!(noted, held_until, "the hold is noted for a Message change to wake");
        } else {
            assert_eq!(outcome.requeue_after, None);
            assert_eq!(noted, None);
        }
    }
}
