use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_controllers::reconcilers::{DockerEnvironmentRuntime, DockerProvisioning, DockerProvisioningError, EnvironmentReconciler};
use flotilla_resources::{
    controller::Reconciler, DockerEnvironmentSpec, Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus,
    EnvironmentStatusPatch, EnvironmentWaitReason, InputMeta, ResourceBackend, ResourceError, StatusPatch,
};

struct WaitingDockerRuntime;

#[async_trait]
impl DockerEnvironmentRuntime for WaitingDockerRuntime {
    async fn provision(&self, _name: &str, _spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, DockerProvisioningError> {
        Err(DockerProvisioningError::Waiting {
            message: "waiting for agent login material; 2 in pool, all leased".to_string(),
            reason: EnvironmentWaitReason::MaterialPoolExhausted { pool_ref: "agent-login".to_string() },
        })
    }

    async fn destroy(&self, _environment_ref: &str, _container_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn pool_exhaustion_stays_pending_with_a_legible_requeued_wait() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<Environment>("flotilla");
    let environment = environments
        .create(&InputMeta::builder().name("env-waiting".to_string()).build(), &EnvironmentSpec {
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "host-a".to_string(),
                image: "crew-image".to_string(),
                declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
                required_agent_adapters: BTreeSet::from(["codex".to_string()]),
                pull_policy: Default::default(),
                mounts: Vec::new(),
                env: Default::default(),
            }),
        })
        .await
        .expect("create environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(WaitingDockerRuntime));

    let deps = reconciler.fetch_dependencies(&environment).await.expect("fetch waiting state");
    let outcome = reconciler.reconcile(&environment, &deps, chrono::Utc::now());

    assert_eq!(outcome.requeue_after, Some(Duration::from_secs(5)));
    assert!(matches!(
        outcome.patch,
        Some(EnvironmentStatusPatch::MarkWaiting {
            message,
            reason: EnvironmentWaitReason::MaterialPoolExhausted { pool_ref },
        }) if message == "waiting for agent login material; 2 in pool, all leased" && pool_ref == "agent-login"
    ));
}

#[tokio::test]
async fn finalizer_error_surfaces_as_failed_environment_status() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environment = backend
        .using::<Environment>("flotilla")
        .create(&InputMeta::builder().name("env-corrupt-metadata".to_string()).build(), &EnvironmentSpec {
            host_direct: None,
            docker: None,
        })
        .await
        .expect("create environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(WaitingDockerRuntime));
    let error = ResourceError::other("failed to parse provisioned mount metadata: corrupt label");

    let patch = reconciler
        .finalizer_error_patch(&environment, &error)
        .expect("environment finalizer errors should produce a visible failed status");
    let mut status = EnvironmentStatus::default();
    patch.apply(&mut status);

    assert_eq!(status.phase, EnvironmentPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("environment teardown failed: failed to parse provisioned mount metadata: corrupt label"));
}
