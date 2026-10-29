//! The convoy reconciler behind `[primitives]` (#3587 layer 6): with the
//! engine holding a convoy, an observation is the engine's edge and no old
//! promise patch is written; without it, the old path is unchanged; and an
//! engine refusal of admission fails the convoy with the refusal named.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_resources::{
    address::{artifact_address, record_name},
    convoy::engine::{Actor, Admission, EngineConvoyPath, EngineConvoys, OldPromises, SkipReason, VerdictScope},
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, PromiseState},
    Artifact, ArtifactSpec, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, ConvoyStatusPatch, CrewSource, CrewSpec, CrewWorkPhase,
    CrewWorkState, InputMeta, ResourceObject, StatusPatch, VesselRequirement, WorkflowSnapshot, WorkflowTemplate,
};
use flotilla_store::{controller::Reconciler, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

/// An engine that records what it is asked, holds what it is told to.
#[derive(Default)]
struct Recording {
    holds: bool,
    admission: Option<Admission>,
    promises: Mutex<Vec<(String, String, PromiseOperation, Actor)>>,
    views: Mutex<usize>,
    /// The engine's change feed, when the test publishes on one.
    changes: Mutex<Option<tokio::sync::broadcast::Sender<String>>>,
    /// The promises the engine holds, once handed over from the stored status.
    held: Option<OldPromises>,
}

#[async_trait]
impl EngineConvoys for Recording {
    fn holds(&self, _: &ResourceObject<Convoy>) -> bool {
        self.holds
    }
    async fn admit(&self, _: &ResourceObject<Convoy>) -> Result<Admission, String> {
        Ok(self.admission.clone().unwrap_or(Admission::Skipped(SkipReason::NotBootstrapped)))
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
    async fn deliver(&self, _: &ResourceObject<Convoy>) -> Result<(), String> {
        Ok(())
    }
    async fn abandon(&self, _: &ResourceObject<Convoy>, _: &str, _: &str) -> Result<(), String> {
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
    let name = record_name(&artifact_address("c", "coder", "decision-ledger", "c").expect("address")).expect("record name");
    backend
        .clone()
        .using::<Artifact>(NAMESPACE)
        .create(
            &InputMeta::builder().name(name).build(),
            &ArtifactSpec::builder()
                .convoy("c".to_string())
                .producer("coder".to_string())
                .kind("decision-ledger".to_string())
                .subject("c".to_string())
                .digest("sha256:test".to_string())
                .size(1)
                .media_type("text/markdown".to_string())
                .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("artifact");
    convoys.update_status("c", &created.metadata.resource_version, &status).await.expect("status")
}

fn reconciler(backend: &ResourceBackend, engine: Arc<Recording>) -> ConvoyReconciler {
    let path: EngineConvoyPath = Arc::new(OnceLock::new());
    let engine: Arc<dyn EngineConvoys> = engine;
    assert!(path.set(engine).is_ok());
    ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(NAMESPACE))
        .with_artifacts(backend.including_replicas::<Artifact>(NAMESPACE))
        .with_engine(path)
}

// Behaviour: with the engine holding the convoy, the reconciler reads the engine's promises, the
// observed ledger is the engine's Submit by an observer, and no old promise patch is written.
#[tokio::test]
async fn an_engine_held_convoy_observes_through_the_engine() {
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
    assert!(
        !matches!(outcome.patch, Some(ConvoyStatusPatch::ObservePromise { .. } | ConvoyStatusPatch::Promise { .. })),
        "no old promise write: {:?}",
        outcome.patch
    );
    assert_eq!(*engine.views.lock().expect("views"), 1, "one keyed read of the engine's promises per pass");
    // The stored status still holds bootstrap's seeds: the hand-over is retried.
    assert_eq!(outcome.patch, Some(ConvoyStatusPatch::HandPromisesToEngine));
}

// Behaviour: a convoy the engine does not hold stays on the old path: the observation is the old
// patch, and the engine is asked nothing about its promises.
#[tokio::test]
async fn a_convoy_the_engine_does_not_hold_keeps_the_old_path() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoy = ledger_convoy(&backend).await;
    let engine = Arc::new(Recording::default());
    let reconciler = reconciler(&backend, Arc::clone(&engine));
    let prepared = reconciler.prepare(&convoy).await.expect("prepare");
    let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
    assert!(engine.promises.lock().expect("promises").is_empty());
    let Some(ConvoyStatusPatch::ObservePromise { operation, .. }) = outcome.patch else { panic!("old patch: {:?}", outcome.patch) };
    let mut status = convoy.status.clone().expect("status");
    ConvoyStatusPatch::ObservePromise { vessel: "work".into(), role: "coder".into(), operation }.apply(&mut status);
    assert_eq!(promises::owned(&status, "work", "coder")[0].state, PromiseState::Submitted);
}

// Behaviour: an engine refusal of admission fails the convoy, naming the refusal.
#[tokio::test]
async fn an_engine_refusal_fails_the_convoy() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoy = ledger_convoy(&backend).await;
    let engine = Arc::new(Recording { admission: Some(Admission::Refused("NoObligations".into())), ..Default::default() });
    let reconciler = reconciler(&backend, engine);
    let prepared = reconciler.prepare(&convoy).await.expect("prepare");
    let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
    let Some(ConvoyStatusPatch::FailConvoy { message, .. }) = outcome.patch else { panic!("fail: {:?}", outcome.patch) };
    assert_eq!(message.as_deref(), Some("the engine refused this convoy's admission: NoObligations"));
}

// Behaviour (#3732 review): each engine edit's convoy name wakes the convoy reconciler through
// EngineConvoyWake; with the switch off, the watch has nothing to follow and ends.
#[tokio::test]
async fn engine_changes_wake_the_convoy_reconciler() {
    use flotilla_crew::convoy::EngineConvoyWake;
    use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
    let backend = ResourceBackend::new(InMemoryBackend::default());

    let (sender, mut queue) = WorkQueueSender::channel();
    let off: Box<dyn SecondaryWatch<Primary = Convoy>> = Box::new(EngineConvoyWake { engine: EngineConvoyPath::default() });
    off.spawn(backend.clone(), "flotilla".into(), sender).await.expect("switch off: ends at once");
    assert!(queue.try_recv().is_err(), "nothing queued");

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

/// The engine Message path's one answer the hold reads: whether the crew has
/// an undelivered turn. The convoy reconciler asks it nothing else.
struct Turns {
    backend: ResourceBackend,
    awaiting: bool,
}

#[async_trait]
impl flotilla_crew::ports::EngineMessages for Turns {
    async fn view(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>, flotilla_resources::ResourceError> {
        unreachable!("the convoy reconciler reads no message views")
    }
    async fn views(
        &self,
        _: &str,
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
    async fn withdraw(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
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
    async fn awaits_delivery(&self, namespace: &str, receiver: &str) -> Result<bool, String> {
        assert_eq!((namespace, receiver), (NAMESPACE, "flotilla/c/work/coder"));
        Ok(self.awaiting)
    }
    async fn delivery_wait(&self, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn charter_notices(self: Arc<Self>, namespace: &str) -> Box<dyn flotilla_messaging::CharterNotices> {
        Box::new(flotilla_messaging::MessageInbox::new(self.backend.clone(), namespace))
    }
}

// Behaviour (#3894): with the engine deciding, a crew whose promises are all kept
// derives Done only once no turn to its live session is undelivered; while one
// is, the reconciler holds the Done and rechecks soon, since no Message watch
// wakes it when the turn is delivered. The engine's reading of the session's
// launch decides whether it is live, whatever its stored phase says.
#[tokio::test]
async fn an_undelivered_turn_holds_the_engines_derived_done() {
    use flotilla_provisioning_testkit::{Ending, TerminalFixture};
    use flotilla_resources::{TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionStatus, CONVOY_LABEL};
    for (awaiting, ending, stored) in [
        (true, Ending::Ready("sid"), TerminalSessionPhase::Stopped),
        (false, Ending::Ready("sid"), TerminalSessionPhase::Stopped),
        (true, Ending::Released, TerminalSessionPhase::Running),
    ] {
        let live = matches!(ending, Ending::Ready(_));
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let mut convoy = ledger_convoy(&backend).await;
        // Handed over: the engine holds the kept promises; the stored status holds none.
        let status = convoy.status.as_mut().expect("status");
        let mut held = std::mem::take(&mut status.promises);
        for promise in held.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("promises") {
            promise.state = PromiseState::Kept;
        }
        let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("coder-session".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "c".into()),
                        (flotilla_resources::VESSEL_LABEL.into(), "work".into()),
                        (flotilla_resources::ROLE_LABEL.into(), "coder".into()),
                    ]))
                    .build(),
                &flotilla_resources::TerminalSessionSpec {
                    env_ref: "env".into(),
                    role: "coder".into(),
                    source: TerminalSessionSource::Tool { command: "test".into() },
                    cwd: "/workspace".into(),
                    env: Default::default(),
                    pool: "cleat".into(),
                },
            )
            .await
            .expect("session");
        let session = sessions
            .update_status(
                "coder-session",
                &session.metadata.resource_version,
                &TerminalSessionStatus { phase: stored, ..Default::default() },
            )
            .await
            .expect("stored phase");
        let fixture = TerminalFixture::new(false);
        fixture.request(&session);
        fixture.finish(&session, ending);
        let messages: flotilla_crew::ports::MessagePath = Arc::new(OnceLock::new());
        let turns: Arc<dyn flotilla_crew::ports::EngineMessages> = Arc::new(Turns { backend: backend.clone(), awaiting });
        assert!(messages.set(turns).is_ok());
        let engine = Arc::new(Recording { holds: true, held: Some(held), ..Default::default() });
        let reconciler = reconciler(&backend, engine)
            .with_terminal_sessions(sessions)
            .with_messages(messages)
            .with_lifecycle_reads(fixture.reads.clone());
        let prepared = reconciler.prepare(&convoy).await.expect("prepare");
        let outcome = reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
        let derived = matches!(outcome.patch, Some(ConvoyStatusPatch::DeriveCrewDone { .. }));
        assert_eq!(derived, !(awaiting && live), "awaiting {awaiting}, live {live}: {:?}", outcome.patch);
        if awaiting && live {
            assert_eq!(outcome.requeue_after, Some(std::time::Duration::from_secs(1)), "a held Done rechecks soon");
        }
    }
}
