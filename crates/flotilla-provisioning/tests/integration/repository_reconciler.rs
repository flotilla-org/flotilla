use std::sync::Arc;

use async_trait::async_trait;
use common::meta;
use flotilla_provisioning::engine::checkout::CheckoutBinding;
use flotilla_provisioning::reconcilers::{ForgeDefaultBranchResolver, RepositoryReconciler};
use flotilla_resources::{
    Checkout, CheckoutSpec, CheckoutWorktreeSpec, CloneSpec, EnvironmentSpec, ForgeIdentity, FreshCloneCheckoutSpec,
    HostDirectEnvironmentSpec, LifecycleAuthority, ObservedCheckoutSpec, Repository, RepositoryCheckoutKind, RepositorySpec,
};
use flotilla_store::{controller::Reconciler, ResourceBackend};

use crate::common;

const NAMESPACE: &str = "flotilla";

struct FixedForgeDefaultBranch(&'static str);

#[async_trait]
impl ForgeDefaultBranchResolver for FixedForgeDefaultBranch {
    async fn default_branch(&self, _forge: &ForgeIdentity) -> Result<Option<String>, String> {
        Ok(Some(self.0.to_string()))
    }
}

#[tokio::test]
async fn repository_status_groups_typed_checkout_associations_by_explicit_host() {
    let durable = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let observed = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::observed());
    let repository_spec = RepositorySpec::remote("https://github.com/org/repo.git").expect("repository spec");
    let repository_key = repository_spec.key();
    let repository = durable
        .clone()
        .using::<Repository>(NAMESPACE)
        .create(&meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("repository create");
    let lifecycles = flotilla_provisioning_testkit::LifecycleFixture::<CheckoutBinding>::new(false);
    lifecycles.environments.request(
        "env-host-a",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host-a".to_string(), repo_default_dir: "/repos".to_string() }),
            ..Default::default()
        },
        None,
    );
    durable
        .clone()
        .using::<Checkout>(NAMESPACE)
        .create(
            &meta("managed-worktree"),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: repository_key.clone(),
                env_ref: "env-host-a".to_string(),
                r#ref: "feature".to_string(),
                base_ref: None,
                target_path: "/repos/repo.feature".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("worktree create");
    durable
        .clone()
        .using::<Checkout>(NAMESPACE)
        .create(
            &meta("transient-clone"),
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                source_clone_ref: None,
                repo_ref: repository_key.clone(),
                env_ref: "env-host-a".to_string(),
                r#ref: "main".to_string(),
                base_ref: None,
                target_path: "/workspace".to_string(),
                url: "https://github.com/org/repo.git".to_string(),
            }),
        )
        .await
        .expect("fresh clone checkout create");
    observed
        .clone()
        .using::<Checkout>(NAMESPACE)
        .create(
            &meta("observed-main").with_lifecycle_authority(LifecycleAuthority::Observed),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                repo_ref: repository_key.clone(),
                host_ref: "host-b".to_string(),
                r#ref: "trunk".to_string(),
                path: "/work/repo".to_string(),
                is_main: true,
            }),
        )
        .await
        .expect("observed checkout create");
    lifecycles.clones.put_named_ready(
        "clone-a",
        &CloneSpec {
            repo_ref: repository_key.clone(),
            url: "https://github.com/org/repo.git".to_string(),
            env_ref: "env-host-a".to_string(),
            path: "/repos/repo".to_string(),
        },
        Some("main"),
    );

    let reconciler = RepositoryReconciler::new(durable, observed, NAMESPACE)
        .with_lifecycle_reads(lifecycles.reads.clone())
        .with_forge_default_branch_resolver(Arc::new(FixedForgeDefaultBranch("stable")));
    let status = reconciler.prepare(&repository).await.expect("status projection");

    assert_eq!(status.checkouts_by_host["host-a"][0].checkout_ref, "managed-worktree");
    assert_eq!(status.checkouts_by_host["host-a"][0].kind, RepositoryCheckoutKind::Worktree);
    assert_eq!(status.checkouts_by_host["host-a"][1].kind, RepositoryCheckoutKind::FreshClone);
    assert_eq!(status.checkouts_by_host["host-b"][0].checkout_ref, "observed-main");
    assert_eq!(status.checkouts_by_host["host-b"][0].authority, LifecycleAuthority::Observed);
    assert_eq!(status.default_branch.as_deref(), Some("stable"), "forge metadata outranks remote symbolic HEAD and local trunk");
    assert!(!status.diagnostics.is_empty(), "branch disagreement should be retained as a diagnostic");
}

#[tokio::test]
async fn repository_with_no_checkouts_projects_empty_status_without_deletion() {
    let durable = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let observed = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::observed());
    let spec = RepositorySpec::remote("https://github.com/org/empty.git").expect("repository spec");
    let repository =
        durable.clone().using::<Repository>(NAMESPACE).create(&meta(&spec.key().to_string()), &spec).await.expect("repository create");

    let clones = flotilla_provisioning_testkit::CloneFixture::default();
    let reconciler = RepositoryReconciler::new(durable, observed, NAMESPACE).with_lifecycle_reads(clones.reads.clone());
    let status = reconciler.prepare(&repository).await.expect("status projection");

    assert!(status.checkouts_by_host.is_empty());
    assert_eq!(status.default_branch, None);
}
