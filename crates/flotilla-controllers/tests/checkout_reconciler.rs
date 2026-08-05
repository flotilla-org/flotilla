// Happy-path checkout reconciler coverage lives in provisioning_in_memory.rs.
// Keep unit tests here only for edge cases, validation, or failure-mapping
// behavior that is clearer to assert directly than through controller-loop tests.
mod common;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use common::{create_ready_checkout, create_ready_clone, meta};
use flotilla_controllers::reconcilers::{
    checkout::CheckoutDeps, CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, PreparedCheckout,
};
use flotilla_protocol::NodeId;
use flotilla_resources::{
    apply_status_patch,
    controller::{Actuation, ControllerLoop, Reconciler},
    repo_key, Checkout, CheckoutBranchProvenance, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, Clone, CloneSpec,
    CloneStatusPatch, ConditionValue, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, FreshCloneCheckoutSpec, InMemoryBackend, InputMeta,
    IntegrationCondition, LifecycleAuthority, RepositoryKey, ResourceBackend, ResourceError, ResourceObject, StatusPatch, VirtualClock,
    ACTUATOR_SOURCE_ROOT_ANNOTATION, CHANGE_REQUEST_ID_LABEL, CONVOY_LABEL,
};
use tokio::time::timeout;

const NAMESPACE: &str = "flotilla";
const REPO_URL: &str = "https://github.com/flotilla-org/flotilla";

#[derive(Default)]
struct RecordingCheckoutRuntime {
    removals: Mutex<Vec<CheckoutRemoval>>,
    inspections: Mutex<usize>,
    failed_removal_target: Option<String>,
}

#[async_trait]
impl CheckoutRuntime for RecordingCheckoutRuntime {
    async fn create_worktree(
        &self,
        _clone_path: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        Err("creation is outside this test's scope".to_string())
    }

    async fn create_fresh_clone(
        &self,
        _repo_url: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        Err("creation is outside this test's scope".to_string())
    }

    async fn inspect_integration(
        &self,
        _checkout: &ResourceObject<Checkout>,
        _convoy: Option<&ResourceObject<flotilla_resources::Convoy>>,
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        *self.inspections.lock().expect("inspections lock") += 1;
        Ok(flotilla_resources::CheckoutIntegrationStatus {
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["1 unpushed commit".to_string()]).build(),
            landed: IntegrationCondition::builder()
                .value(ConditionValue::Unknown)
                .details(vec!["no change request provider".to_string()])
                .build(),
            landed_evidence: None,
            change_request: None,
        })
    }

    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.removals.lock().expect("removals lock").push(removal.clone());
        let target_path = match removal {
            CheckoutRemoval::Worktree { target_path, .. }
            | CheckoutRemoval::OrphanedWorktree { target_path }
            | CheckoutRemoval::FreshClone { target_path } => target_path,
        };
        if self.failed_removal_target.as_deref() == Some(target_path) {
            return Err("permission denied removing root-owned debris".to_string());
        }
        Ok(CheckoutRemovalOutcome::Removed)
    }
}

#[tokio::test]
async fn finalizer_error_maps_to_failed_checkout_status() {
    let backend = ResourceBackend::InMemory(Default::default());
    let checkout = create_ready_checkout(
        &backend,
        NAMESPACE,
        common::ReadyCheckoutFixture::builder()
            .name("checkout-a".to_string())
            .env_ref("host-direct-a".to_string())
            .git_ref("feature/cleanup".to_string())
            .path("/checkouts/convoy-a/repo.feature-cleanup".to_string())
            .fresh_clone(FreshCloneCheckoutSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "feature/cleanup".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
                url: REPO_URL.to_string(),
            })
            .build(),
    )
    .await;
    let reconciler = CheckoutReconciler::new(Arc::new(RecordingCheckoutRuntime::default()), backend, NAMESPACE);
    let error = ResourceError::other("remove worktree: permission denied");

    let patch =
        reconciler.finalizer_error_patch(&checkout, &error).expect("checkout finalizer errors should produce a failed status patch");
    let mut status = checkout.status.expect("ready checkout should have status");
    patch.apply(&mut status);

    assert_eq!(status.phase, CheckoutPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("checkout teardown failed: remove worktree: permission denied"));
}

#[tokio::test]
async fn clone_failure_from_the_current_checkout_attempt_is_reported_as_a_historical_dependency() {
    let backend = ResourceBackend::InMemory(Default::default());
    let clones = backend.clone().using::<Clone>(NAMESPACE);
    clones
        .create(&meta("clone-a"), &CloneSpec {
            repo_ref: RepositoryKey(repo_key(REPO_URL)),
            url: REPO_URL.to_string(),
            env_ref: "host-direct-a".to_string(),
            path: "/clones/repo".to_string(),
        })
        .await
        .expect("clone should create");
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let checkout = checkouts
        .create(
            &meta("checkout-a"),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "fix/clone-failed-latch".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/repo.fix-clone-failed-latch".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout should create");
    let failed_at = checkout.metadata.creation_timestamp + chrono::Duration::seconds(1);
    apply_status_patch(&clones, "clone-a", &CloneStatusPatch::MarkFailed { message: "authentication failed".to_string(), failed_at })
        .await
        .expect("clone failure should apply");
    let reconciler = CheckoutReconciler::new(Arc::new(RecordingCheckoutRuntime::default()), backend, NAMESPACE);

    let deps = reconciler.fetch_dependencies(&checkout).await.expect("dependencies should load");
    let outcome = reconciler.reconcile(&checkout, &deps, failed_at);

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::CheckoutStatusPatch::MarkFailed { message })
            if message == format!("clone clone-a is Failed since {}: authentication failed", failed_at.to_rfc3339())
    ));
}

async fn create_deleting_checkout(backend: &ResourceBackend, name: &str, target_path: &str) {
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let meta = common::controller_meta()
        .name(name)
        .finalizers(vec!["flotilla.work/checkout-cleanup".to_string()])
        .deletion_timestamp(chrono::Utc::now())
        .call();
    let created = checkouts
        .create(
            &meta,
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: format!("feature/{name}"),
                base_ref: Some("main".to_string()),
                target_path: target_path.to_string(),
                url: REPO_URL.to_string(),
            }),
        )
        .await
        .expect("deleting checkout create should succeed");
    checkouts
        .update_status(name, &created.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some(target_path.to_string()),
            commit: Some("base-commit".to_string()),
            branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("deleting checkout status update should succeed");
}

#[tokio::test]
async fn failed_checkout_teardown_marks_only_that_checkout_failed_and_controller_continues() {
    let backend = ResourceBackend::InMemory(Default::default());
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    create_deleting_checkout(&backend, "alpha-failing", "/checkouts/alpha").await;
    create_deleting_checkout(&backend, "beta-healthy", "/checkouts/beta").await;
    let reconciler = CheckoutReconciler::new(
        Arc::new(RecordingCheckoutRuntime { failed_removal_target: Some("/checkouts/alpha".to_string()), ..Default::default() }),
        backend.clone(),
        NAMESPACE,
    );
    let controller = tokio::spawn(
        ControllerLoop {
            primary: checkouts.clone(),
            secondaries: Vec::new(),
            reconciler,
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let failing = checkouts.get("alpha-failing").await.expect("failing checkout should remain for retry");
            let failed_with_message = failing.status.as_ref().is_some_and(|status| {
                status.phase == CheckoutPhase::Failed
                    && status.message.as_deref() == Some("checkout teardown failed: permission denied removing root-owned debris")
            });
            let healthy_removed = matches!(checkouts.get("beta-healthy").await, Err(ResourceError::NotFound { .. }));
            if failed_with_message && healthy_removed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed checkout should degrade while healthy checkout teardown completes");

    controller.abort();
    let _ = controller.await;
}

#[tokio::test]
async fn worktree_finalizer_supplies_clone_branch_and_target_to_runtime() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_clone(&backend, NAMESPACE, "clone-a", REPO_URL, "host-direct-a", "/checkouts/repo").await;
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let created = checkouts
        .create(
            &meta("checkout-a"),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "feature/cleanup".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");
    checkouts
        .update_status("checkout-a", &created.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/checkouts/convoy-a/repo.feature-cleanup".to_string()),
            commit: Some("base-commit".to_string()),
            branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
    let checkout = checkouts.get("checkout-a").await.expect("checkout should exist");
    let runtime = Arc::new(RecordingCheckoutRuntime::default());
    let reconciler = CheckoutReconciler::new(Arc::clone(&runtime), backend, NAMESPACE);

    reconciler.run_finalizer(&checkout).await.expect("finalizer should succeed");

    assert_eq!(runtime.removals.lock().expect("removals lock").as_slice(), &[CheckoutRemoval::Worktree {
        clone_path: "/checkouts/repo".to_string(),
        branch: "feature/cleanup".to_string(),
        target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
    }]);
}

#[tokio::test]
async fn worktree_finalizer_removes_checkout_when_clone_resource_is_already_gone() {
    let backend = ResourceBackend::InMemory(Default::default());
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let created = checkouts
        .create(
            &meta("checkout-a"),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "feature/cleanup".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
                clone_ref: "missing-clone".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");
    let runtime = Arc::new(RecordingCheckoutRuntime::default());
    let reconciler = CheckoutReconciler::new(Arc::clone(&runtime), backend, NAMESPACE);

    reconciler.run_finalizer(&created).await.expect("missing clone must not wedge checkout deletion");

    assert_eq!(runtime.removals.lock().expect("removals lock").as_slice(), &[CheckoutRemoval::OrphanedWorktree {
        target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
    }]);
}

#[tokio::test]
async fn ready_checkout_reconciler_patches_integration_conditions() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_clone(&backend, NAMESPACE, "clone-a", REPO_URL, "host-direct-a", "/checkouts/repo").await;
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let created = checkouts
        .create(
            &meta("checkout-a"),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "feature/cleanup".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");
    checkouts
        .update_status("checkout-a", &created.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/checkouts/convoy-a/repo.feature-cleanup".to_string()),
            commit: Some("base-commit".to_string()),
            branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
    let checkout = checkouts.get("checkout-a").await.expect("checkout should exist");
    let runtime = Arc::new(RecordingCheckoutRuntime::default());
    let reconciler = CheckoutReconciler::new(Arc::clone(&runtime), backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&checkout).await.expect("fetch dependencies should succeed");

    let outcome = reconciler.reconcile(&checkout, &deps, chrono::Utc::now());

    match outcome.patch {
        Some(flotilla_resources::CheckoutStatusPatch::UpdateIntegration { integration }) => {
            assert_eq!(integration.clean.value, ConditionValue::True);
            assert_eq!(integration.pushed.value, ConditionValue::False);
            assert_eq!(integration.landed.value, ConditionValue::Unknown);
        }
        patch => panic!("expected integration patch, got {patch:?}"),
    }
}

#[tokio::test]
async fn ready_checkout_reconciler_skips_fresh_integration_probe() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_clone(&backend, NAMESPACE, "clone-a", REPO_URL, "host-direct-a", "/checkouts/repo").await;
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let created = checkouts
        .create(
            &meta("checkout-a"),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "feature/cleanup".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/convoy-a/repo.feature-cleanup".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");
    let observed_at = chrono::Utc::now().to_rfc3339();
    checkouts
        .update_status("checkout-a", &created.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/checkouts/convoy-a/repo.feature-cleanup".to_string()),
            commit: Some("base-commit".to_string()),
            branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
            integration: flotilla_resources::CheckoutIntegrationStatus {
                clean: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.clone()).build(),
                pushed: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.clone()).build(),
                landed: IntegrationCondition::builder().value(ConditionValue::False).observed_at(observed_at).build(),
                landed_evidence: None,
                change_request: None,
            },
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
    let checkout = checkouts.get("checkout-a").await.expect("checkout should exist");
    let runtime = Arc::new(RecordingCheckoutRuntime::default());
    let reconciler = CheckoutReconciler::new(Arc::clone(&runtime), backend, NAMESPACE);

    let deps = reconciler.fetch_dependencies(&checkout).await.expect("fetch dependencies should succeed");
    let outcome = reconciler.reconcile(&checkout, &deps, chrono::Utc::now());

    assert!(matches!(deps, CheckoutDeps::None));
    assert!(outcome.patch.is_none(), "fresh integration status should not be patched");
    assert_eq!(*runtime.inspections.lock().expect("inspections lock"), 0);
}

#[tokio::test]
async fn checkout_authority_observes_when_replicated_convoy_needs_terminal_evidence() {
    for phase in [ConvoyPhase::Landing, ConvoyPhase::Failed, ConvoyPhase::Cancelled] {
        let authority_root = NodeId::new("convoy-authority");
        let checkout_root = NodeId::new("checkout-authority");
        let authority = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(authority_root.clone());
        let checkout_host = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(checkout_root);
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-03T21:45:00Z").expect("timestamp").with_timezone(&chrono::Utc);
        let clock = Arc::new(VirtualClock::new(now));

        let convoys = authority.clone().using::<Convoy>(NAMESPACE);
        let convoy = convoys
            .create(
                &InputMeta::builder().name("cross-host".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review-and-fix".to_string()).build(),
            )
            .await
            .expect("create authority convoy");
        convoys
            .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus { phase, ..Default::default() })
            .await
            .expect("mark convoy in evidence-consuming phase");
        checkout_host
            .replica_writer::<Convoy>(authority_root, NAMESPACE)
            .replace(&convoys.list().await.expect("list authority convoys"), now)
            .await
            .expect("replicate convoy to checkout host");

        let checkouts = checkout_host.clone().using::<Checkout>(NAMESPACE);
        let checkout = checkouts
            .create(
                &InputMeta::builder()
                    .name("remote-checkout".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "cross-host".to_string())]))
                    .annotations(BTreeMap::from([(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "convoy-authority".to_string())]))
                    .build(),
                &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                    repo_ref: RepositoryKey(repo_key(REPO_URL)),
                    env_ref: "host-direct-checkout-authority".to_string(),
                    r#ref: "feature/cross-host".to_string(),
                    base_ref: Some("main".to_string()),
                    target_path: "/checkouts/cross-host".to_string(),
                    url: REPO_URL.to_string(),
                }),
            )
            .await
            .expect("create checkout on its authority host");
        let observed_at = (now - chrono::Duration::seconds(31)).to_rfc3339();
        checkouts
            .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                path: Some("/checkouts/cross-host".to_string()),
                integration: flotilla_resources::CheckoutIntegrationStatus {
                    clean: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.clone()).build(),
                    pushed: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at.clone()).build(),
                    landed: IntegrationCondition::builder().value(ConditionValue::True).observed_at(observed_at).build(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("record pre-Landing evidence");

        let checkout = checkouts.get("remote-checkout").await.expect("get checkout");
        let runtime = Arc::new(RecordingCheckoutRuntime::default());
        let reconciler = CheckoutReconciler::with_clock(runtime.clone(), checkout_host.clone(), NAMESPACE, clock)
            .with_federated_convoys(&checkout_host, NAMESPACE);

        let deps = reconciler.fetch_dependencies(&checkout).await.expect("resolve replicated evidence-consuming convoy");

        assert!(matches!(deps, CheckoutDeps::Integration { .. }), "{phase:?} must shorten the observation TTL to 30 seconds");
        assert_eq!(*runtime.inspections.lock().expect("inspections lock"), 1, "only the checkout authority should probe its checkout");
    }
}

#[tokio::test]
async fn checkout_authority_reclaims_managed_checkout_when_replicated_convoy_is_landed() {
    let authority_root = NodeId::new("convoy-authority");
    let checkout_host = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("checkout-authority"));
    let authority = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(authority_root.clone());
    let convoys = authority.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name("cross-host".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review-and-fix".to_string()).build(),
        )
        .await
        .expect("create authority convoy");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Landed,
            ..Default::default()
        })
        .await
        .expect("mark convoy Landed");
    checkout_host
        .replica_writer::<Convoy>(authority_root, NAMESPACE)
        .replace(&convoys.list().await.expect("list authority convoys"), chrono::Utc::now())
        .await
        .expect("replicate convoy to checkout host");

    let checkout = checkout_host
        .clone()
        .using::<Checkout>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("remote-checkout".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "cross-host".to_string())]))
                .annotations(BTreeMap::from([(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "convoy-authority".to_string())]))
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Managed),
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-checkout-authority".to_string(),
                r#ref: "feature/cross-host".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/cross-host".to_string(),
                url: REPO_URL.to_string(),
            }),
        )
        .await
        .expect("create managed checkout");
    let runtime = Arc::new(RecordingCheckoutRuntime::default());
    let reconciler =
        CheckoutReconciler::new(Arc::clone(&runtime), checkout_host.clone(), NAMESPACE).with_federated_convoys(&checkout_host, NAMESPACE);

    let deps = reconciler.fetch_dependencies(&checkout).await.expect("resolve replicated Landed convoy");
    let outcome = reconciler.reconcile(&checkout, &deps, chrono::Utc::now());

    assert!(matches!(deps, CheckoutDeps::OwnerTerminal));
    assert!(matches!(outcome.actuations.as_slice(), [Actuation::DeleteCheckout { name }] if name == "remote-checkout"));
    assert_eq!(*runtime.inspections.lock().expect("inspections lock"), 0, "Landed is durable settlement evidence");
}

#[tokio::test]
async fn fresh_failed_change_request_lookup_waits_for_the_landing_ttl_before_retrying() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-03T21:45:00Z").expect("timestamp").with_timezone(&chrono::Utc);
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name("convoy-a".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review-and-fix".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Landing,
            ..Default::default()
        })
        .await
        .expect("mark convoy Landing");
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let checkout = checkouts
        .create(
            &InputMeta::builder()
                .name("checkout-a".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
                    (CHANGE_REQUEST_ID_LABEL.to_string(), "42".to_string()),
                ]))
                .build(),
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                repo_ref: RepositoryKey(repo_key(REPO_URL)),
                env_ref: "host-direct-a".to_string(),
                r#ref: "feature/a".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/a".to_string(),
                url: REPO_URL.to_string(),
            }),
        )
        .await
        .expect("create checkout");
    let failed_lookup = IntegrationCondition::builder()
        .value(ConditionValue::Unknown)
        .details(vec!["gh PR lookup failed".to_string()])
        .observed_at(now.to_rfc3339())
        .build();
    checkouts
        .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/checkouts/a".to_string()),
            integration: flotilla_resources::CheckoutIntegrationStatus {
                clean: failed_lookup.clone(),
                pushed: failed_lookup.clone(),
                landed: failed_lookup,
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("record failed lookup");

    let runtime = Arc::new(RecordingCheckoutRuntime::default());
    let reconciler = CheckoutReconciler::with_clock(runtime.clone(), backend, NAMESPACE, Arc::new(VirtualClock::new(now)));
    let checkout = checkouts.get("checkout-a").await.expect("get checkout");
    let deps = reconciler.fetch_dependencies(&checkout).await.expect("fetch dependencies");

    assert!(matches!(deps, CheckoutDeps::None));
    assert_eq!(*runtime.inspections.lock().expect("inspections lock"), 0, "forge failures should be rate-limited by the TTL");
}
