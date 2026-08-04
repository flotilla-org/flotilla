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
    controller::Reconciler,
    test_support::{
        assert_actuation_drop_recovery, assert_bounded_convergence, assert_degradation_not_wedging, assert_quiescence_at_fixpoint,
        assert_staleness_edges, FixpointPredicate, LivenessEnrollment, LivenessScenario, LivenessStep, ReconcileStep, WorldBuilder,
        WriteCountingBackend,
    },
    Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch, Clock,
    ConditionValue, Convoy, FreshCloneCheckoutSpec, InputMeta, IntegrationCondition, RepositoryKey, ResourceObject, VirtualClock,
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

    async fn inspect_integration(
        &self,
        _checkout: &ResourceObject<Checkout>,
        _convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<CheckoutIntegrationStatus, String> {
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
            change_request: None,
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
