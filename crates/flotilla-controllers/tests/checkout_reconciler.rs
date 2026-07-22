// Happy-path checkout reconciler coverage lives in provisioning_in_memory.rs.
// Keep unit tests here only for edge cases, validation, or failure-mapping
// behavior that is clearer to assert directly than through controller-loop tests.
mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{create_ready_clone, meta};
use flotilla_controllers::reconcilers::{CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime};
use flotilla_resources::{
    controller::Reconciler, repo_key, Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, RepositoryKey,
    ResourceBackend,
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
    ) -> Result<Option<String>, String> {
        Err("creation is outside this test's scope".to_string())
    }

    async fn create_fresh_clone(
        &self,
        _repo_url: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<Option<String>, String> {
        Err("creation is outside this test's scope".to_string())
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
