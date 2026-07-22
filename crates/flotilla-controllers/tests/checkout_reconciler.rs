// Happy-path checkout reconciler coverage lives in provisioning_in_memory.rs.
// Keep unit tests here only for edge cases, validation, or failure-mapping
// behavior that is clearer to assert directly than through controller-loop tests.
mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{create_ready_clone, meta};
use flotilla_controllers::reconcilers::{CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, PreparedCheckout};
use flotilla_resources::{
    controller::Reconciler, repo_key, Checkout, CheckoutBranchProvenance, CheckoutPhase, CheckoutSpec, CheckoutStatus,
    CheckoutWorktreeSpec, ConditionValue, IntegrationCondition, RepositoryKey, ResourceBackend, ResourceObject,
};

const NAMESPACE: &str = "flotilla";
const REPO_URL: &str = "https://github.com/flotilla-org/flotilla";

#[derive(Default)]
struct RecordingCheckoutRuntime {
    removals: Mutex<Vec<CheckoutRemoval>>,
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
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        Ok(flotilla_resources::CheckoutIntegrationStatus {
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["1 unpushed commit".to_string()]).build(),
            landed: IntegrationCondition::builder()
                .value(ConditionValue::Unknown)
                .details(vec!["no change request provider".to_string()])
                .build(),
            landed_evidence: None,
        })
    }

    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.removals.lock().expect("removals lock").push(removal.clone());
        Ok(CheckoutRemovalOutcome::Removed)
    }
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
