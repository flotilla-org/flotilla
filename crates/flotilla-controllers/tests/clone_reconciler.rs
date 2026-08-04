use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use flotilla_controllers::reconcilers::{CloneReconciler, CloneRuntime};
use flotilla_resources::{clone_key, controller::Reconciler, CloneSpec, Repository, RepositorySpec, ResourceBackend};

mod common;
use common::meta;

#[derive(Default)]
struct FakeCloneRuntime;

#[async_trait]
impl CloneRuntime for FakeCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }
}

struct FailingCloneRuntime;

#[async_trait]
impl CloneRuntime for FailingCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        Err("authentication failed".to_string())
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Err("clone does not exist".to_string())
    }
}

struct FailOnceCloneRuntime {
    failed: AtomicBool,
}

#[async_trait]
impl CloneRuntime for FailOnceCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            Err("simulated interrupted clone".to_string())
        } else {
            Ok(Some("main".to_string()))
        }
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }
}

#[tokio::test]
async fn mismatched_clone_name_fails() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let resolver = backend.using::<flotilla_resources::Clone>("flotilla");
    let clone = resolver
        .create(&meta("clone-wrong"), &CloneSpec {
            repo_ref: repository_key,
            url: "git@github.com:flotilla-org/flotilla.git".to_string(),
            env_ref: "host-direct-01HXYZ".to_string(),
            path: "/Users/alice/dev/flotilla".to_string(),
        })
        .await
        .expect("create should succeed");
    let reconciler = CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.using("flotilla"));
    let deps = reconciler.fetch_dependencies(&clone).await.expect("deps should load");
    let outcome = reconciler.reconcile(&clone, &deps, chrono::Utc::now());

    assert!(matches!(outcome.patch, Some(flotilla_resources::CloneStatusPatch::MarkFailed { .. })));
}

#[tokio::test]
async fn alias_transport_uses_typed_repository_identity_for_clone_name() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let env_ref = "host-direct-01HXYZ";
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", env_ref));
    let clone = backend
        .clone()
        .using::<flotilla_resources::Clone>("flotilla")
        .create(&meta(&clone_name), &CloneSpec {
            repo_ref: repository_key,
            url: "git@github.work:flotilla-org/flotilla.git".to_string(),
            env_ref: env_ref.to_string(),
            path: "/Users/alice/dev/flotilla".to_string(),
        })
        .await
        .expect("clone should create");
    let reconciler = CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.using("flotilla"));

    let deps = reconciler.fetch_dependencies(&clone).await.expect("deps should load");
    let outcome = reconciler.reconcile(&clone, &deps, chrono::Utc::now());

    assert!(matches!(outcome.patch, Some(flotilla_resources::CloneStatusPatch::MarkReady { .. })));
}

#[tokio::test]
async fn clone_failure_retries_once_before_marking_failed() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/private").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let env_ref = "host-direct-01HXYZ";
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/private", env_ref));
    let clone = backend
        .clone()
        .using::<flotilla_resources::Clone>("flotilla")
        .create(&meta(&clone_name), &CloneSpec {
            repo_ref: repository_key,
            url: "git@github.com:flotilla-org/private.git".to_string(),
            env_ref: env_ref.to_string(),
            path: "/Users/alice/dev/private".to_string(),
        })
        .await
        .expect("clone should create");
    let reconciler = CloneReconciler::new(Arc::new(FailingCloneRuntime), backend.using("flotilla"));

    let deps = reconciler.fetch_dependencies(&clone).await.expect("first deps should load");
    let outcome = reconciler.reconcile(&clone, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch.as_ref(),
        Some(flotilla_resources::CloneStatusPatch::MarkRetrying { message }) if message == "authentication failed"
    ));
    assert!(outcome.requeue_after.is_some());

    let clones = backend.clone().using::<flotilla_resources::Clone>("flotilla");
    let retrying =
        flotilla_resources::apply_status_patch(&clones, &clone_name, &outcome.patch.expect("first failure should record retry state"))
            .await
            .expect("retry state should apply");
    let repeated_deps = reconciler.fetch_dependencies(&retrying).await.expect("repeated deps should load");
    let repeated_outcome = reconciler.reconcile(&retrying, &repeated_deps, chrono::Utc::now());

    assert!(matches!(
        repeated_outcome.patch,
        Some(flotilla_resources::CloneStatusPatch::MarkFailed { message, .. }) if message == "authentication failed"
    ));
    assert!(repeated_outcome.requeue_after.is_none());
}

#[tokio::test]
async fn transient_clone_failure_remains_retryable_and_converges() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let env_ref = "host-direct-01HXYZ";
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", env_ref));
    let clones = backend.clone().using::<flotilla_resources::Clone>("flotilla");
    let clone = clones
        .create(&meta(&clone_name), &CloneSpec {
            repo_ref: repository_key,
            url: "git@github.com:flotilla-org/flotilla.git".to_string(),
            env_ref: env_ref.to_string(),
            path: "/Users/alice/dev/flotilla".to_string(),
        })
        .await
        .expect("clone should create");
    let reconciler =
        CloneReconciler::new(Arc::new(FailOnceCloneRuntime { failed: AtomicBool::new(false) }), backend.clone().using("flotilla"));

    let failed_deps = reconciler.fetch_dependencies(&clone).await.expect("first deps should load");
    let failed_outcome = reconciler.reconcile(&clone, &failed_deps, chrono::Utc::now());
    let retry_patch = failed_outcome.patch.expect("transient failure should record retry state");
    let retrying = flotilla_resources::apply_status_patch(&clones, &clone_name, &retry_patch).await.expect("retry status should apply");

    let recovered_deps = reconciler.fetch_dependencies(&retrying).await.expect("retry deps should load");
    let recovered_outcome = reconciler.reconcile(&retrying, &recovered_deps, chrono::Utc::now());

    assert!(matches!(
        recovered_outcome.patch,
        Some(flotilla_resources::CloneStatusPatch::MarkReady { default_branch }) if default_branch.as_deref() == Some("main")
    ));
}
