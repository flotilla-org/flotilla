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
    fn promises(&self, convoy: &ResourceObject<Convoy>, _: Option<&str>) -> Option<OldPromises> {
        *self.views.lock().expect("views") += 1;
        // The engine's view: here, the stored promises as they are.
        self.holds.then(|| convoy.status.as_ref().map(|status| status.promises.clone()).unwrap_or_default())
    }
    fn changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        tokio::sync::broadcast::channel(1).1
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
