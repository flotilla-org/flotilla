use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_controllers::reconcilers::{DockerEnvironmentRuntime, DockerProvisioning, EnvironmentReconciler};
use flotilla_resources::{
    controller::Reconciler, DockerEnvironmentSpec, Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus,
    EnvironmentStatusPatch, Host, HostDirectEnvironmentSpec, HostSpec, InputMeta, ResourceBackend, ResourceError, StatusPatch,
};

struct FailingDockerRuntime;

#[async_trait]
impl DockerEnvironmentRuntime for FailingDockerRuntime {
    async fn provision(&self, _name: &str, _spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        Err("central Codex credential is not provisioned on this host".to_string())
    }

    async fn destroy(&self, _environment_ref: &str, _container_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn cleanup(&self, _environment_ref: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingDockerRuntime {
    cleaned: Mutex<Vec<String>>,
}

#[async_trait]
impl DockerEnvironmentRuntime for RecordingDockerRuntime {
    async fn provision(&self, _name: &str, _spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        unreachable!("finalizer test does not provision")
    }

    async fn destroy(&self, _environment_ref: &str, _container_id: &str) -> Result<(), String> {
        unreachable!("failed environment has no container")
    }

    async fn cleanup(&self, environment_ref: &str) -> Result<(), String> {
        self.cleaned.lock().expect("cleanup log lock should be healthy").push(environment_ref.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn a_provisioning_failure_marks_the_environment_failed() {
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
    let reconciler = EnvironmentReconciler::new(Arc::new(FailingDockerRuntime), backend.clone(), "flotilla");

    let deps = reconciler.prepare(&environment).await.expect("attempt provisioning");
    let outcome = reconciler.reconcile(&environment, &deps, chrono::Utc::now());

    assert_eq!(outcome.requeue_after, None, "a failed provisioning attempt is terminal, not admission queueing");
    assert!(matches!(
        outcome.patch,
        Some(EnvironmentStatusPatch::MarkFailed { message }) if message == "central Codex credential is not provisioned on this host"
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
    let reconciler = EnvironmentReconciler::new(Arc::new(FailingDockerRuntime), backend.clone(), "flotilla");
    let error = ResourceError::other("failed to parse provisioned mount metadata: corrupt label");

    let patch = reconciler
        .finalizer_error_patch(&environment, &error)
        .expect("environment finalizer errors should produce a visible failed status");
    let mut status = EnvironmentStatus::default();
    patch.apply(&mut status);

    assert_eq!(status.phase, EnvironmentPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("environment teardown failed: failed to parse provisioned mount metadata: corrupt label"));
}

#[tokio::test]
async fn failed_environment_without_a_container_still_runs_terminal_cleanup() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<Environment>("flotilla");
    let environment = environments
        .create(&InputMeta::builder().name("env-failed".to_string()).build(), &EnvironmentSpec {
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "host-a".to_string(),
                image: "crew-image".to_string(),
                declared_agent_adapters: BTreeSet::new(),
                required_agent_adapters: BTreeSet::new(),
                pull_policy: Default::default(),
                mounts: Vec::new(),
                env: Default::default(),
            }),
        })
        .await
        .expect("create environment");
    let environment = environments
        .update_status("env-failed", &environment.metadata.resource_version, &EnvironmentStatus {
            phase: EnvironmentPhase::Failed,
            message: Some("provisioning failed".to_string()),
            ..EnvironmentStatus::default()
        })
        .await
        .expect("mark environment failed");
    let runtime = Arc::new(RecordingDockerRuntime::default());
    let reconciler = EnvironmentReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.run_finalizer(&environment).await.expect("failed environment cleanup");

    assert_eq!(*runtime.cleaned.lock().expect("cleanup log lock should be healthy"), ["env-failed"]);
}

struct ForeignEnvironmentRuntime;

#[async_trait]
impl DockerEnvironmentRuntime for ForeignEnvironmentRuntime {
    async fn provision(&self, _name: &str, _spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        panic!("a non-actuator must not provision a foreign environment")
    }

    async fn destroy(&self, _environment_ref: &str, _container_id: &str) -> Result<(), String> {
        panic!("a non-actuator must not destroy a foreign environment")
    }
}

#[tokio::test]
async fn foreign_environment_is_not_actuated_or_finalized() {
    let backend = ResourceBackend::InMemory(Default::default());
    let hosts = backend.using::<Host>("flotilla");
    for host in ["kiwi", "udder"] {
        hosts
            .create(&InputMeta::builder().name(host.to_string()).build(), &HostSpec {
                display_name: host.to_string(),
                connection: Default::default(),
            })
            .await
            .expect("create host identity");
    }
    let environments = backend.using::<Environment>("flotilla");
    let docker = environments
        .create(&InputMeta::builder().name("env-udder".to_string()).build(), &EnvironmentSpec {
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "udder".to_string(),
                image: "crew:latest".to_string(),
                declared_agent_adapters: BTreeSet::new(),
                required_agent_adapters: BTreeSet::new(),
                pull_policy: Default::default(),
                mounts: Vec::new(),
                env: Default::default(),
            }),
        })
        .await
        .expect("create foreign docker environment");
    let host_direct = environments
        .create(&InputMeta::builder().name("direct-udder".to_string()).build(), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "udder".to_string(), repo_default_dir: "/worktrees".to_string() }),
            docker: None,
        })
        .await
        .expect("create foreign host-direct environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(ForeignEnvironmentRuntime), backend.clone(), "flotilla")
        .with_local_host_ref(flotilla_protocol::CanonicalHostId::resolved("kiwi"));

    for environment in [&docker, &host_direct] {
        let prepared = reconciler.prepare(environment).await.expect("foreign environment should be skipped");
        assert!(reconciler.reconcile(environment, &prepared, chrono::Utc::now()).patch.is_none());
        reconciler.run_finalizer(environment).await.expect("foreign finalization should be skipped");
    }
}

#[tokio::test]
async fn orphaned_environment_can_finalize_after_its_host_disappears() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environment = backend
        .using::<Environment>("flotilla")
        .create(&InputMeta::builder().name("env-orphaned".to_string()).build(), &EnvironmentSpec {
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "deleted-host".to_string(),
                image: "crew:latest".to_string(),
                declared_agent_adapters: BTreeSet::new(),
                required_agent_adapters: BTreeSet::new(),
                pull_policy: Default::default(),
                mounts: Vec::new(),
                env: Default::default(),
            }),
        })
        .await
        .expect("create orphaned environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(ForeignEnvironmentRuntime), backend, "flotilla")
        .with_local_host_ref(flotilla_protocol::CanonicalHostId::resolved("kiwi"));

    let prepared = reconciler.prepare(&environment).await.expect("orphaned environment should be treated as foreign");
    assert!(reconciler.reconcile(&environment, &prepared, chrono::Utc::now()).patch.is_none());
    reconciler.run_finalizer(&environment).await.expect("orphaned environment finalizer should converge");
}
