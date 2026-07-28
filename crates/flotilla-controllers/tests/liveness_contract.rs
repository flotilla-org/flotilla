mod common;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_controllers::reconcilers::{
    checkout::{CheckoutDeps, CheckoutReconciler},
    CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, PreparedCheckout,
};
use flotilla_resources::{
    controller::{Actuation, Reconciler},
    test_support::{
        assert_actuation_drop_recovery, assert_bounded_convergence, assert_degradation_not_wedging, assert_quiescence_at_fixpoint,
        assert_staleness_edges, FixpointPredicate, LivenessEnrollment, LivenessScenario, LivenessStep, ReconcileStep, WorldBuilder,
        WriteCountingBackend,
    },
    Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch, Clock,
    ConditionValue, Convoy, ConvoyPhase, ConvoyReconciler, ConvoyStatusPatch, ConvoyTeardownRuntime, FreshCloneCheckoutSpec, InputMeta,
    IntegrationCondition, RepositoryKey, ResourceObject, VirtualClock, WorkPhase, CONVOY_LABEL,
};

const NAMESPACE: &str = "flotilla";
const INTEGRATION_TTL: Duration = Duration::hours(6);

struct HarnessRuntime {
    clock: Arc<VirtualClock>,
    inspections: AtomicUsize,
    fail_inspection: bool,
}

#[async_trait]
impl CheckoutRuntime for HarnessRuntime {
    async fn create_worktree(
        &self,
        _clone_path: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        unreachable!("the liveness fixture uses a fresh clone")
    }

    async fn create_fresh_clone(
        &self,
        _repo_url: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        Ok(PreparedCheckout { commit: Some("abc123".to_string()), branch_provenance: CheckoutBranchProvenance::CreatedForConvoy })
    }

    async fn inspect_integration(&self, _checkout: &ResourceObject<Checkout>) -> Result<CheckoutIntegrationStatus, String> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        if self.fail_inspection {
            return Err("resource says Ready but checkout path is missing".to_string());
        }
        let observed_at = self.clock.now().to_rfc3339();
        Ok(CheckoutIntegrationStatus {
            clean: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.clone()).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.clone()).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::False).observed_at(observed_at).build(),
            landed_evidence: None,
        })
    }

    async fn remove_checkout(&self, _removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        Ok(CheckoutRemovalOutcome::Removed)
    }
}

struct CheckoutWorld {
    backend: WriteCountingBackend,
    current: ResourceObject<Checkout>,
    reconciler: CheckoutReconciler<HarnessRuntime>,
    runtime: Arc<HarnessRuntime>,
}

struct CheckoutWorldBuilder {
    clock: Arc<VirtualClock>,
}

#[async_trait]
impl WorldBuilder for CheckoutWorldBuilder {
    type World = CheckoutWorld;

    async fn build(&self, scenario: LivenessScenario) -> Result<Self::World, String> {
        self.clock.set(timestamp());
        let backend = WriteCountingBackend::in_memory();
        let resolver = backend.using::<Checkout>(NAMESPACE);
        let created = resolver
            .create(
                &InputMeta { name: "checkout-a".to_string(), ..Default::default() },
                &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                    repo_ref: RepositoryKey("repo-a".to_string()),
                    env_ref: "env-a".to_string(),
                    r#ref: "feature/liveness".to_string(),
                    base_ref: Some("main".to_string()),
                    target_path: "/work/checkout-a".to_string(),
                    url: "https://example.com/repo-a".to_string(),
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
        let fail_inspection = scenario == LivenessScenario::Contradictory;
        let current = if fail_inspection {
            resolver
                .update_status("checkout-a", &created.metadata.resource_version, &CheckoutStatus {
                    phase: CheckoutPhase::Ready,
                    path: None,
                    commit: Some("abc123".to_string()),
                    branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
                    integration: CheckoutIntegrationStatus::default(),
                    message: None,
                })
                .await
                .map_err(|error| error.to_string())?
        } else {
            created
        };
        backend.reset_writes();
        let runtime = Arc::new(HarnessRuntime { clock: Arc::clone(&self.clock), inspections: AtomicUsize::new(0), fail_inspection });
        let reconciler = CheckoutReconciler::with_clock(
            Arc::clone(&runtime),
            backend.inner(),
            NAMESPACE,
            self.clock.clone() as Arc<dyn flotilla_resources::Clock>,
        );
        Ok(CheckoutWorld { backend, current, reconciler, runtime })
    }
}

struct CheckoutStep {
    broken_freshness_latch: bool,
}

#[async_trait]
impl ReconcileStep<CheckoutWorld> for CheckoutStep {
    type Patch = ();
    type Actuation = CheckoutStatusPatch;

    async fn reconcile_step(&self, world: &mut CheckoutWorld) -> Result<LivenessStep<Self::Patch, Self::Actuation>, String> {
        let has_observation = world.current.status.as_ref().is_some_and(|status| status.integration.clean.observed_at.is_some());
        let deps = if self.broken_freshness_latch && has_observation {
            // Deliberately re-broken #1163-style variant: once an observation
            // exists, trust it forever instead of consulting the injected clock.
            CheckoutDeps::None
        } else {
            world.reconciler.fetch_dependencies(&world.current).await.map_err(|error| error.to_string())?
        };
        let outcome = world.reconciler.reconcile(&world.current, &deps, world.reconciler_clock_now());
        Ok(LivenessStep::new(None, outcome.patch.into_iter().collect()))
    }

    async fn apply_patch(&self, _world: &mut CheckoutWorld, _patch: Self::Patch) -> Result<(), String> {
        Ok(())
    }

    async fn apply_actuation(&self, world: &mut CheckoutWorld, actuation: Self::Actuation) -> Result<(), String> {
        world.current = world
            .backend
            .using::<Checkout>(NAMESPACE)
            .apply_status_patch("checkout-a", &actuation)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

trait CheckoutWorldClock {
    fn reconciler_clock_now(&self) -> DateTime<Utc>;
}

impl CheckoutWorldClock for CheckoutWorld {
    fn reconciler_clock_now(&self) -> DateTime<Utc> {
        self.runtime.clock.now()
    }
}

struct CheckoutFixpoint;

impl FixpointPredicate<CheckoutWorld> for CheckoutFixpoint {
    fn at_fixpoint(&self, world: &CheckoutWorld) -> bool {
        world.current.status.as_ref().is_some_and(|status| {
            status.phase == CheckoutPhase::Ready
                && status.integration.clean.observed_at.as_deref().is_some_and(|observed_at| {
                    DateTime::parse_from_rfc3339(observed_at)
                        .is_ok_and(|observed_at| world.runtime.clock.now().signed_duration_since(observed_at) < INTEGRATION_TTL)
                })
        })
    }

    fn write_count(&self, world: &CheckoutWorld) -> usize {
        world.backend.writes()
    }

    fn reset_write_count(&self, world: &CheckoutWorld) {
        world.backend.reset_writes();
    }

    fn held(&self, world: &CheckoutWorld) -> bool {
        world.current.status.as_ref().is_some_and(|status| {
            status.integration.clean.value == ConditionValue::Unknown
                && status.integration.clean.details.iter().any(|detail| detail.contains("path is missing"))
        })
    }

    fn probe_count(&self, world: &CheckoutWorld) -> usize {
        world.runtime.inspections.load(Ordering::SeqCst)
    }
}

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).single().expect("valid timestamp")
}

fn checkout_enrollment(broken_freshness_latch: bool) -> LivenessEnrollment<CheckoutWorldBuilder, CheckoutStep, CheckoutFixpoint> {
    let clock = Arc::new(VirtualClock::new(timestamp()));
    LivenessEnrollment::new(
        CheckoutWorldBuilder { clock: Arc::clone(&clock) },
        CheckoutStep { broken_freshness_latch },
        CheckoutFixpoint,
        clock,
    )
    .with_staleness_edges([INTEGRATION_TTL])
}

#[tokio::test]
async fn checkout_reconciler_satisfies_liveness_battery() {
    let enrollment = checkout_enrollment(false);
    assert_bounded_convergence(&enrollment).await;
    assert_quiescence_at_fixpoint(&enrollment).await;
    assert_staleness_edges(&enrollment).await;
    assert_actuation_drop_recovery(&enrollment).await;
    assert_degradation_not_wedging(&enrollment).await;
}

#[tokio::test]
#[should_panic(expected = "did not re-probe stale state")]
async fn checkout_staleness_property_bites_a_rebroken_freshness_latch() {
    assert_staleness_edges(&checkout_enrollment(true)).await;
}

const LANDING_TTL: Duration = Duration::seconds(30);

struct LandingRuntime {
    clock: Arc<VirtualClock>,
    probes: AtomicUsize,
    contradictory: bool,
    trust_observation_forever: bool,
}

#[async_trait]
impl ConvoyTeardownRuntime for LandingRuntime {
    async fn no_change_request_outstanding(
        &self,
        _convoy: &ResourceObject<Convoy>,
        checkouts: &[ResourceObject<Checkout>],
    ) -> Result<bool, String> {
        let landed =
            &checkouts.first().ok_or_else(|| "checkout missing".to_string())?.status.as_ref().ok_or("status missing")?.integration.landed;
        let observed_at = landed
            .observed_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .ok_or_else(|| "landing observation missing".to_string())?;
        let fresh = self.clock.now().signed_duration_since(observed_at) < LANDING_TTL;
        if fresh || self.trust_observation_forever {
            return Ok(landed.value == ConditionValue::True);
        }
        self.probes.fetch_add(1, Ordering::SeqCst);
        Ok(!self.contradictory)
    }

    async fn verify_reclaim(&self, _convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        Err("not terminal".to_string())
    }
}

struct ConvoyWorld {
    backend: WriteCountingBackend,
    current: ResourceObject<Convoy>,
    reconciler: ConvoyReconciler,
    runtime: Arc<LandingRuntime>,
    contradictory: bool,
    passes: usize,
}

struct ConvoyWorldBuilder {
    clock: Arc<VirtualClock>,
    trust_observation_forever: bool,
}

#[async_trait]
impl WorldBuilder for ConvoyWorldBuilder {
    type World = ConvoyWorld;

    async fn build(&self, scenario: LivenessScenario) -> Result<Self::World, String> {
        self.clock.set(timestamp());
        let backend = WriteCountingBackend::in_memory();
        let inner = backend.inner();
        let created =
            common::create_convoy_with_single_task(&inner, NAMESPACE, "convoy-a", "work", "https://example.com/repo-a", "main").await;
        let convoys = backend.using::<Convoy>(NAMESPACE);
        let mut status = created.status.expect("convoy fixture status");
        status.phase = ConvoyPhase::Landing;
        status.observed_workflow_ref = Some("wf".to_string());
        status.work.insert("work".to_string(), common::work_state().phase(WorkPhase::Complete).call());
        let current =
            convoys.update_status("convoy-a", &created.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;

        let checkouts = backend.using::<Checkout>(NAMESPACE);
        let checkout = checkouts
            .create(
                &InputMeta {
                    name: "checkout-a".to_string(),
                    labels: [(CONVOY_LABEL.to_string(), "convoy-a".to_string())].into_iter().collect(),
                    ..Default::default()
                },
                &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                    repo_ref: current.spec.repositories[0].repo_ref.clone(),
                    env_ref: "env-a".to_string(),
                    r#ref: "feature/liveness".to_string(),
                    base_ref: Some("main".to_string()),
                    target_path: "/missing/checkout-a".to_string(),
                    url: "https://example.com/repo-a".to_string(),
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
        let observation_time = if scenario == LivenessScenario::Contradictory {
            self.clock.now() - LANDING_TTL - Duration::seconds(1)
        } else {
            self.clock.now()
        };
        checkouts
            .update_status("checkout-a", &checkout.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                path: if scenario == LivenessScenario::Contradictory { None } else { Some("/work/checkout-a".to_string()) },
                commit: Some("abc123".to_string()),
                branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
                integration: CheckoutIntegrationStatus {
                    clean: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observation_time.to_rfc3339()).build(),
                    pushed: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observation_time.to_rfc3339()).build(),
                    landed: IntegrationCondition::builder().value(ConditionValue::False).observed_at(observation_time.to_rfc3339()).build(),
                    landed_evidence: None,
                },
                message: None,
            })
            .await
            .map_err(|error| error.to_string())?;
        backend.reset_writes();

        let contradictory = scenario == LivenessScenario::Contradictory;
        let runtime = Arc::new(LandingRuntime {
            clock: Arc::clone(&self.clock),
            probes: AtomicUsize::new(0),
            contradictory,
            trust_observation_forever: self.trust_observation_forever,
        });
        let reconciler = ConvoyReconciler::new(inner.using(NAMESPACE))
            .with_checkouts(inner.using(NAMESPACE))
            .with_teardown_runtime(Arc::clone(&runtime) as Arc<dyn ConvoyTeardownRuntime>);
        Ok(ConvoyWorld { backend, current, reconciler, runtime, contradictory, passes: 0 })
    }
}

struct ConvoyStep;

#[async_trait]
impl ReconcileStep<ConvoyWorld> for ConvoyStep {
    type Patch = ConvoyStatusPatch;
    type Actuation = Actuation;

    async fn reconcile_step(&self, world: &mut ConvoyWorld) -> Result<LivenessStep<Self::Patch, Self::Actuation>, String> {
        let deps = world.reconciler.fetch_dependencies(&world.current).await.map_err(|error| error.to_string())?;
        let outcome = world.reconciler.reconcile(&world.current, &deps, world.runtime.clock.now());
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
        !world.contradictory
            && world.passes > 0
            && world.current.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing | ConvoyPhase::Landed))
    }

    fn write_count(&self, world: &ConvoyWorld) -> usize {
        world.backend.writes()
    }

    fn reset_write_count(&self, world: &ConvoyWorld) {
        world.backend.reset_writes();
    }

    fn held(&self, world: &ConvoyWorld) -> bool {
        world.contradictory
            && world.runtime.probes.load(Ordering::SeqCst) > 0
            && world.current.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landing)
    }

    fn probe_count(&self, world: &ConvoyWorld) -> usize {
        world.runtime.probes.load(Ordering::SeqCst)
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

    // Landing's integration probe is an imperative runtime effect, not an
    // Actuation value. Actuation-drop recovery is therefore intentionally not
    // claimed for this enrollee; value-shaped cleanup actuations are covered
    // independently by convoy reconcile tests.
}

#[tokio::test]
#[should_panic(expected = "did not re-probe stale state")]
async fn convoy_staleness_property_bites_a_rebroken_trust_forever_edge() {
    assert_staleness_edges(&convoy_enrollment(true)).await;
}
