use std::sync::Arc;

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
