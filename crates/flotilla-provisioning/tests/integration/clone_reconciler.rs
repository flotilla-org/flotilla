use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use common::meta;
use flotilla_provisioning::reconcilers::{CloneReconciler, CloneRuntime};
use flotilla_resources::{clone_key, CloneSpec, Repository, RepositorySpec};
use flotilla_store::{controller::Reconciler, ResourceBackend};

use crate::common;

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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let resolver = backend.using::<flotilla_resources::Clone>("flotilla");
    let clone = resolver
        .create(
            &meta("clone-wrong"),
            &CloneSpec {
                repo_ref: repository_key,
                url: "git@github.com:flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-01HXYZ".to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
        )
        .await
        .expect("create should succeed");
    let reconciler = CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.using("flotilla"));
    let deps = reconciler.prepare(&clone).await.expect("deps should load");
    let outcome = reconciler.reconcile(&clone, &deps, chrono::Utc::now());

    assert!(matches!(outcome.patch, Some(flotilla_resources::CloneStatusPatch::MarkFailed { .. })));
}

#[tokio::test]
async fn alias_transport_uses_typed_repository_identity_for_clone_name() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let env_ref = "host-direct-01HXYZ";
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", env_ref));
    let clone = backend
        .clone()
        .using::<flotilla_resources::Clone>("flotilla")
        .create(
            &meta(&clone_name),
            &CloneSpec {
                repo_ref: repository_key,
                url: "git@github.work:flotilla-org/flotilla.git".to_string(),
                env_ref: env_ref.to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
        )
        .await
        .expect("clone should create");
    let reconciler = CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.using("flotilla"));

    let deps = reconciler.prepare(&clone).await.expect("deps should load");
    let outcome = reconciler.reconcile(&clone, &deps, chrono::Utc::now());

    assert!(matches!(outcome.patch, Some(flotilla_resources::CloneStatusPatch::MarkReady { .. })));
}

#[tokio::test]
async fn clone_failures_remain_retryable() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/private").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let env_ref = "host-direct-01HXYZ";
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/private", env_ref));
    let clone = backend
        .clone()
        .using::<flotilla_resources::Clone>("flotilla")
        .create(
            &meta(&clone_name),
            &CloneSpec {
                repo_ref: repository_key,
                url: "git@github.com:flotilla-org/private.git".to_string(),
                env_ref: env_ref.to_string(),
                path: "/Users/alice/dev/private".to_string(),
            },
        )
        .await
        .expect("clone should create");
    let reconciler = CloneReconciler::new(Arc::new(FailingCloneRuntime), backend.using("flotilla"));

    let deps = reconciler.prepare(&clone).await.expect("first deps should load");
    let outcome = reconciler.reconcile(&clone, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch.as_ref(),
        Some(flotilla_resources::CloneStatusPatch::MarkRetrying { message }) if message == "authentication failed"
    ));
    assert!(outcome.requeue_after.is_some());

    let clones = backend.clone().using::<flotilla_resources::Clone>("flotilla");
    let retrying =
        flotilla_store::apply_status_patch(&clones, &clone_name, &outcome.patch.expect("first failure should record retry state"))
            .await
            .expect("retry state should apply");
    let repeated_deps = reconciler.prepare(&retrying).await.expect("repeated deps should load");
    let repeated_outcome = reconciler.reconcile(&retrying, &repeated_deps, chrono::Utc::now());

    assert!(matches!(
        repeated_outcome.patch,
        Some(flotilla_resources::CloneStatusPatch::MarkRetrying { message }) if message == "authentication failed"
    ));
    assert!(repeated_outcome.requeue_after.is_some());
}

#[tokio::test]
async fn transient_clone_failure_remains_retryable_and_converges() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let env_ref = "host-direct-01HXYZ";
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", env_ref));
    let clones = backend.clone().using::<flotilla_resources::Clone>("flotilla");
    let clone = clones
        .create(
            &meta(&clone_name),
            &CloneSpec {
                repo_ref: repository_key,
                url: "git@github.com:flotilla-org/flotilla.git".to_string(),
                env_ref: env_ref.to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
        )
        .await
        .expect("clone should create");
    let reconciler =
        CloneReconciler::new(Arc::new(FailOnceCloneRuntime { failed: AtomicBool::new(false) }), backend.clone().using("flotilla"));

    let failed_deps = reconciler.prepare(&clone).await.expect("first deps should load");
    let failed_outcome = reconciler.reconcile(&clone, &failed_deps, chrono::Utc::now());
    let retry_patch = failed_outcome.patch.expect("transient failure should record retry state");
    let retrying = flotilla_store::apply_status_patch(&clones, &clone_name, &retry_patch).await.expect("retry status should apply");

    let recovered_deps = reconciler.prepare(&retrying).await.expect("retry deps should load");
    let recovered_outcome = reconciler.reconcile(&retrying, &recovered_deps, chrono::Utc::now());

    assert!(matches!(
        recovered_outcome.patch,
        Some(flotilla_resources::CloneStatusPatch::MarkReady { default_branch }) if default_branch.as_deref() == Some("main")
    ));
}

// The engine decides whether cloning is still needed even when every old phase disagrees.
// The finite matrix covers all old phases, duplicate reads and all engine ending judgements.
#[tokio::test]
async fn clone_reconciliation_reads_the_engine_without_a_status_copy() {
    use flotilla_provisioning::engine::{clone::CloneBinding, Binding, Step};
    use flotilla_resources::{ClonePhase, CloneStatus, ControllerRetryDisposition};
    use flotilla_schema::Timestamp;
    use flotilla_state_machines::{Cause, CauseCategory, Outcome};
    for old_phase in [ClonePhase::Pending, ClonePhase::Cloning, ClonePhase::Ready, ClonePhase::Failed] {
        for ending in ["ready", "lost", "released", "gave_up"] {
            let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
            let spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
            flotilla_store::ensure_repository(&backend.clone().using::<Repository>("flotilla"), &spec.key(), &spec)
                .await
                .expect("repository");
            let env_ref = "env";
            let name = format!("clone-{}", spec.clone_key(env_ref).expect("key"));
            let mut clone = backend
                .clone()
                .using::<flotilla_resources::Clone>("flotilla")
                .create(
                    &meta(&name),
                    &CloneSpec {
                        repo_ref: spec.key(),
                        url: "https://github.com/flotilla-org/flotilla".into(),
                        env_ref: env_ref.into(),
                        path: "/repo".into(),
                    },
                )
                .await
                .expect("clone");
            clone.status = Some(CloneStatus { phase: old_phase, ..Default::default() });
            let (home, reads) = common::engine_clones();
            home.request(&name, CloneBinding::request(&clone, "home").expect("request"), Timestamp(1)).expect("requested");
            let reconciler = CloneReconciler::new(Arc::new(FailingCloneRuntime), backend.using("flotilla")).with_lifecycle_reads(reads);
            let prepared = reconciler.prepare(&clone).await.expect("prepare requested");
            assert!(
                matches!(
                    reconciler.reconcile(&clone, &prepared, chrono::Utc::now()).patch,
                    Some(flotilla_resources::CloneStatusPatch::MarkRetrying { .. })
                ),
                "engine Requested wins over {old_phase:?}"
            );
            for attempt in 1..=2 {
                home.step(
                    &name,
                    &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "offline"), retryable: true }),
                    Timestamp(attempt * 1_000_000),
                )
                .expect("failure");
            }
            assert_eq!(
                reconciler.retry_disposition(&clone),
                Some(ControllerRetryDisposition::Retryable { next_attempt_at: Timestamp(62_000_000).to_datetime().expect("time") })
            );
            let prepared = reconciler.prepare(&clone).await.expect("prepare retrying");
            assert_eq!(
                reconciler.reconcile(&clone, &prepared, chrono::Utc::now()).requeue_after,
                Some(std::time::Duration::from_secs(120)),
                "the engine tally sets the next delay"
            );
            match ending {
                "gave_up" => {
                    home.step(
                        &name,
                        &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Misconfigured, ""), retryable: false }),
                        Timestamp(3_000_000),
                    )
                    .expect("gave up");
                }
                "released" => {
                    home.step(&name, &Step::Released, Timestamp(3_000_000)).expect("released");
                }
                _ => {
                    home.step(&name, &Step::Realised { identity: "/repo".into() }, Timestamp(3_000_000)).expect("ready");
                    if ending == "lost" {
                        home.step(&name, &Step::Vanished, Timestamp(4_000_000)).expect("lost");
                    }
                }
            }
            assert!(CloneBinding::copy(&clone, &home.get(&name).expect("held")).is_none());
            for _ in 0..2 {
                let prepared = reconciler.prepare(&clone).await.expect("prepare ended or ready");
                assert!(
                    reconciler.reconcile(&clone, &prepared, chrono::Utc::now()).patch.is_none(),
                    "{ending} suppresses cloning over {old_phase:?}"
                );
            }
            assert_eq!(clone.status.as_ref().expect("unchanged").phase, old_phase);
        }
    }
}
