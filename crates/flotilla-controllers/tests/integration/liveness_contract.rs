use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_resources::checkout_records::{CheckoutRecords as _, Inspection};
use flotilla_resources::Actuation;
use flotilla_resources::ChangeRequest;
use flotilla_resources::ChangeRequestReviewObservation;
use flotilla_resources::ChangeRequestSpec;
use flotilla_resources::ChangeRequestStatus;
use flotilla_resources::CheckoutSpec;
use flotilla_resources::Clock;
use flotilla_resources::Convoy;
use flotilla_resources::ConvoyPhase;
use flotilla_resources::ConvoyStatusPatch;
use flotilla_resources::FreshCloneCheckoutSpec;
use flotilla_resources::InputMeta;
use flotilla_resources::Observation;
use flotilla_resources::ObservedChangeRequestState;
use flotilla_resources::ObservedChecks;
use flotilla_resources::ObservedMergeability;
use flotilla_resources::PlacementStatus;
use flotilla_resources::ResourceObject;
use flotilla_resources::WorkPhase;
use flotilla_store::controller::Reconciler;
use flotilla_store_testkit::assert_bounded_convergence;
use flotilla_store_testkit::assert_degradation_not_wedging;
use flotilla_store_testkit::assert_quiescence_at_fixpoint;
use flotilla_store_testkit::assert_staleness_edges;
use flotilla_store_testkit::FixpointPredicate;
use flotilla_store_testkit::LivenessEnrollment;
use flotilla_store_testkit::LivenessScenario;
use flotilla_store_testkit::LivenessStep;
use flotilla_store_testkit::ReconcileStep;
use flotilla_store_testkit::VirtualClock;
use flotilla_store_testkit::WorldBuilder;
use flotilla_store_testkit::WriteCountingBackend;

use crate::common;
use crate::common::cr_name;

const NAMESPACE: &str = "flotilla";

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).single().expect("valid timestamp")
}

const LANDING_TTL: Duration = Duration::seconds(30);
const CHANGE_REQUEST_NUMBER: u64 = 42;

struct ConvoyWorld {
    backend: WriteCountingBackend,
    /// The engine's checkouts the convoy reads.
    _checkouts: flotilla_provisioning_testkit::CheckoutFixture,
    current: ResourceObject<Convoy>,
    reconciler: ConvoyReconciler,
    clock: Arc<VirtualClock>,
    probes: usize,
    contradictory: bool,
    trust_observation_forever: bool,
    passes: usize,
}

struct ConvoyWorldBuilder {
    clock: Arc<VirtualClock>,
    trust_observation_forever: bool,
}

fn change_request_status(state: ObservedChangeRequestState, observed_at: DateTime<Utc>) -> ChangeRequestStatus {
    ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: Observation::known(state, observed_at),
        head_sha: Observation::known("abc123".to_string(), observed_at),
        checks: Observation::known(ObservedChecks::Pass, observed_at),
        review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
        mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
    }
}

#[async_trait]
impl WorldBuilder for ConvoyWorldBuilder {
    type World = ConvoyWorld;

    async fn build(&self, scenario: LivenessScenario) -> Result<Self::World, String> {
        self.clock.set(timestamp());
        let backend = WriteCountingBackend::in_memory();
        let inner = backend.inner();
        let created =
            common::create_convoy_with_single_task(&inner, NAMESPACE, "convoy-a", "work", "https://github.com/example/repo-a", "main")
                .await;
        let convoys = backend.using::<Convoy>(NAMESPACE);
        let mut status = created.status.expect("convoy fixture status");
        status.phase = ConvoyPhase::Landing;
        status.observed_workflow_ref = Some("wf".to_string());
        let mut work = common::work_state().phase(WorkPhase::Complete).call();
        work.placement = Some(PlacementStatus {
            fields: BTreeMap::from([(
                "checkout_refs".to_string(),
                serde_json::json!(BTreeMap::from([(created.spec.repositories[0].repo_ref.clone(), "checkout-a".to_string())])),
            )]),
        });
        status.work.insert("work".to_string(), work);
        let current =
            convoys.update_status("convoy-a", &created.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;

        // The engine holds the convoy's checkout, realised, its branch inspected clean and pushed
        // but not landed, associated with the change request.
        let checkouts = flotilla_provisioning_testkit::CheckoutFixture::default();
        checkouts.open_convoy("convoy-a");
        let launch = checkouts.put_ready(
            "checkout-a",
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                source_clone_ref: None,
                repo_ref: current.spec.repositories[0].repo_ref.clone(),
                env_ref: "env-a".to_string(),
                r#ref: "feature/liveness".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/work/checkout-a".to_string(),
                url: "https://github.com/example/repo-a".to_string(),
            }),
            Some(&checkouts.holder("convoy-a")),
        );
        checkouts
            .checkouts
            .inspected(
                &launch,
                Inspection {
                    clean: Some(true),
                    pushed: Some(true),
                    landed: Some(false),
                    change_request: Some(CHANGE_REQUEST_NUMBER),
                    ..Inspection::default()
                },
            )
            .map_err(|error| error.to_string())?;

        let change_requests = backend.using::<ChangeRequest>(NAMESPACE);
        let record_name = cr_name("github.com", "example/repo-a", CHANGE_REQUEST_NUMBER);
        let record = change_requests
            .create(
                &InputMeta::builder().name(record_name.clone()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".to_string())
                    .scope("example/repo-a".to_string())
                    .number(CHANGE_REQUEST_NUMBER)
                    .observing_authority("test".to_string())
                    .build(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let cr_state =
            if scenario == LivenessScenario::Contradictory { ObservedChangeRequestState::Open } else { ObservedChangeRequestState::Merged };
        change_requests
            .update_status(&record_name, &record.metadata.resource_version, &change_request_status(cr_state, self.clock.now()))
            .await
            .map_err(|error| error.to_string())?;
        backend.reset_writes();

        let reconciler = ConvoyReconciler::new(checkouts.reads.clone())
            .with_change_requests(inner.including_replicas(NAMESPACE), LANDING_TTL.to_std().expect("positive TTL"))
            .with_clock(self.clock.clone() as Arc<dyn Clock>);
        Ok(ConvoyWorld {
            backend,
            _checkouts: checkouts,
            current,
            reconciler,
            clock: Arc::clone(&self.clock),
            probes: 0,
            contradictory: scenario == LivenessScenario::Contradictory,
            trust_observation_forever: self.trust_observation_forever,
            passes: 0,
        })
    }
}

struct ConvoyStep;

#[async_trait]
impl ReconcileStep<ConvoyWorld> for ConvoyStep {
    type Patch = ConvoyStatusPatch;
    type Actuation = Actuation;

    async fn reconcile_step(&self, world: &mut ConvoyWorld) -> Result<LivenessStep<Self::Patch, Self::Actuation>, String> {
        let records = world.backend.using::<ChangeRequest>(NAMESPACE);
        let record_name = cr_name("github.com", "example/repo-a", CHANGE_REQUEST_NUMBER);
        let record = records.get(&record_name).await.map_err(|error| error.to_string())?;
        let stale =
            record.status.as_ref().is_some_and(|status| world.clock.now().signed_duration_since(status.state.observed_at) > LANDING_TTL);
        if stale && !world.trust_observation_forever {
            world.probes += 1;
            let state = record.status.as_ref().and_then(|status| status.state.value).unwrap_or(ObservedChangeRequestState::Open);
            records
                .update_status(&record_name, &record.metadata.resource_version, &change_request_status(state, world.clock.now()))
                .await
                .map_err(|error| error.to_string())?;
        }
        let deps = world.reconciler.prepare(&world.current).await.map_err(|error| error.to_string())?;
        let outcome = world.reconciler.reconcile(&world.current, &deps, world.clock.now());
        world.passes += 1;
        Ok(LivenessStep::new(outcome.patch, outcome.actuations))
    }

    async fn apply_patch(&self, world: &mut ConvoyWorld, patch: Self::Patch) -> Result<(), String> {
        world.current =
            world.backend.using::<Convoy>(NAMESPACE).apply_status_patch("convoy-a", &patch).await.map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn apply_actuation(&self, _world: &mut ConvoyWorld, actuation: Self::Actuation) -> Result<(), String> {
        Err(format!("landing fixture unexpectedly emitted {actuation:?}"))
    }
}

struct ConvoyFixpoint;

impl FixpointPredicate<ConvoyWorld> for ConvoyFixpoint {
    fn at_fixpoint(&self, world: &ConvoyWorld) -> bool {
        world.passes > 0
            && world.current.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing | ConvoyPhase::Landed))
    }

    fn write_count(&self, world: &ConvoyWorld) -> usize {
        world.backend.writes()
    }

    fn reset_write_count(&self, world: &ConvoyWorld) {
        world.backend.reset_writes();
    }

    fn held(&self, world: &ConvoyWorld) -> bool {
        world.contradictory && world.passes > 0 && world.current.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landing)
    }

    fn probe_count(&self, world: &ConvoyWorld) -> usize {
        world.probes
    }
}

fn convoy_enrollment(trust_observation_forever: bool) -> LivenessEnrollment<ConvoyWorldBuilder, ConvoyStep, ConvoyFixpoint> {
    let clock = Arc::new(VirtualClock::new(timestamp()));
    LivenessEnrollment::new(ConvoyWorldBuilder { clock: Arc::clone(&clock), trust_observation_forever }, ConvoyStep, ConvoyFixpoint, clock)
        .with_staleness_edges([LANDING_TTL])
}

#[tokio::test]
async fn convoy_landing_satisfies_supported_liveness_battery() {
    let enrollment = convoy_enrollment(false);
    assert_bounded_convergence(&enrollment).await;
    assert_quiescence_at_fixpoint(&enrollment).await;
    assert_staleness_edges(&enrollment).await;
    assert_degradation_not_wedging(&enrollment).await;
}

#[tokio::test]
#[should_panic(expected = "did not re-probe stale state")]
async fn convoy_staleness_property_bites_a_rebroken_trust_forever_edge() {
    assert_staleness_edges(&convoy_enrollment(true)).await;
}
