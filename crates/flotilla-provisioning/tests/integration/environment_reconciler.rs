use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_provisioning::reconcilers::{EnvironmentProvisioning, EnvironmentReconciler, EnvironmentRuntime};
use flotilla_resources::{
    DockerEnvironmentSpec, Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, Host,
    HostDirectEnvironmentSpec, HostSpec, InputMeta, ResourceError, StatusPatch,
};
use flotilla_store::{controller::Reconciler, ResourceBackend};

struct FailingEnvironmentRuntime;

#[async_trait]
impl EnvironmentRuntime for FailingEnvironmentRuntime {
    async fn provision_environment(
        &self,
        _name: &str,
        _spec: &flotilla_resources::EnvironmentSpec,
    ) -> Result<flotilla_provisioning::reconcilers::EnvironmentProvisioning, String> {
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
impl EnvironmentRuntime for RecordingDockerRuntime {
    async fn provision_environment(
        &self,
        _name: &str,
        spec: &flotilla_resources::EnvironmentSpec,
    ) -> Result<flotilla_provisioning::reconcilers::EnvironmentProvisioning, String> {
        let _spec = spec.docker.as_ref().expect("Docker scenario input");
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environments = backend.clone().using::<Environment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder().name("env-waiting".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "host-a".to_string(),
                    image: "crew-image".to_string(),
                    declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
                    required_agent_adapters: BTreeSet::from(["codex".to_string()]),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("create environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(FailingEnvironmentRuntime), backend.clone(), "flotilla");

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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environment = backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("env-corrupt-metadata".to_string()).build(),
            &EnvironmentSpec { cloud_sandbox: None, provider_address: None, host_direct: None, docker: None },
        )
        .await
        .expect("create environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(FailingEnvironmentRuntime), backend.clone(), "flotilla");
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environments = backend.clone().using::<Environment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder().name("env-failed".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "host-a".to_string(),
                    image: "crew-image".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("create environment");
    let environment = environments
        .update_status(
            "env-failed",
            &environment.metadata.resource_version,
            &EnvironmentStatus {
                phase: EnvironmentPhase::Failed,
                message: Some("provisioning failed".to_string()),
                ..EnvironmentStatus::default()
            },
        )
        .await
        .expect("mark environment failed");
    let runtime = Arc::new(RecordingDockerRuntime::default());
    let reconciler = EnvironmentReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.run_finalizer(&environment).await.expect("failed environment cleanup");

    assert_eq!(*runtime.cleaned.lock().expect("cleanup log lock should be healthy"), ["env-failed"]);
}

struct ForeignEnvironmentRuntime;

#[async_trait]
impl EnvironmentRuntime for ForeignEnvironmentRuntime {
    async fn provision_environment(
        &self,
        _name: &str,
        spec: &flotilla_resources::EnvironmentSpec,
    ) -> Result<flotilla_provisioning::reconcilers::EnvironmentProvisioning, String> {
        let _spec = spec.docker.as_ref().expect("Docker scenario input");
        panic!("a non-actuator must not provision a foreign environment")
    }

    async fn destroy(&self, _environment_ref: &str, _container_id: &str) -> Result<(), String> {
        panic!("a non-actuator must not destroy a foreign environment")
    }

    async fn destroy_unrecorded(&self, _environment_ref: &str) -> Result<(), String> {
        panic!("a non-actuator must not rediscover a foreign environment backing")
    }
}

#[tokio::test]
async fn foreign_environment_is_not_actuated_or_finalized() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let hosts = backend.using::<Host>("flotilla");
    for host in ["kiwi", "udder"] {
        hosts
            .create(
                &InputMeta::builder().name(host.to_string()).build(),
                &HostSpec { display_name: host.to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("create host identity");
    }
    let environments = backend.using::<Environment>("flotilla");
    let docker = environments
        .create(
            &InputMeta::builder().name("env-udder".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "udder".to_string(),
                    image: "crew:latest".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("create foreign docker environment");
    let host_direct = environments
        .create(
            &InputMeta::builder().name("direct-udder".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "udder".to_string(), repo_default_dir: "/worktrees".to_string() }),
                docker: None,
            },
        )
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environment = backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("env-orphaned".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "deleted-host".to_string(),
                    image: "crew:latest".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("create orphaned environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(ForeignEnvironmentRuntime), backend, "flotilla")
        .with_local_host_ref(flotilla_protocol::CanonicalHostId::resolved("kiwi"));

    let prepared = reconciler.prepare(&environment).await.expect("orphaned environment should be treated as foreign");
    assert!(reconciler.reconcile(&environment, &prepared, chrono::Utc::now()).patch.is_none());
    reconciler.run_finalizer(&environment).await.expect("orphaned environment finalizer should converge");
}

// Host-direct readiness requires a successful runtime adoption. The reconciler
// must not mark the resource ready merely because its spec names host-direct.
#[tokio::test]
async fn host_direct_provider_failure_prevents_readiness() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environment = backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("direct-adoption".into()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/".into() }),
                docker: None,
            },
        )
        .await
        .expect("create direct environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(FailingEnvironmentRuntime), backend, "flotilla");
    let prepared = reconciler.prepare(&environment).await.expect("prepare direct adoption");
    let result = reconciler.reconcile(&environment, &prepared, chrono::Utc::now());
    assert!(matches!(result.patch, Some(EnvironmentStatusPatch::MarkFailed { .. })));
}

// A lost Environment retains its durable container identity and never calls
// provision again. RecordingDockerRuntime stands in for the Docker process;
// its provisioning method panics if automatic rehydration is attempted.
#[tokio::test]
async fn lost_environment_is_not_reprovisioned() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environments = backend.using::<Environment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder().name("lost-environment".into()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    host_ref: "host-a".into(),
                    image: "crew-image".into(),
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    declared_agent_adapters: Default::default(),
                    required_agent_adapters: Default::default(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("environment");
    let environment = environments
        .update_status(
            "lost-environment",
            &environment.metadata.resource_version,
            &EnvironmentStatus {
                phase: EnvironmentPhase::Lost,
                docker_container_id: Some("retained-container".into()),
                message: Some("host reboot".into()),
                ..Default::default()
            },
        )
        .await
        .expect("lost backing");
    let reconciler = EnvironmentReconciler::new(Arc::new(RecordingDockerRuntime::default()), backend.clone(), "flotilla");
    for _ in 0..2 {
        let prepared = reconciler.prepare(&environment).await.expect("lost prepare");
        let outcome = reconciler.reconcile(&environment, &prepared, chrono::Utc::now());
        assert!(outcome.patch.is_none() && outcome.actuations.is_empty());
    }
    let after = environments.get("lost-environment").await.expect("retained environment");
    assert_eq!(after.status, environment.status);
    assert_eq!(after.metadata.resource_version, environment.metadata.resource_version);
}

/// Records the image and pull policy each provisioning attempt receives.
#[derive(Default)]
struct ProvisioningDockerRuntime {
    provisioned: Mutex<Vec<(String, flotilla_resources::DockerImagePullPolicy)>>,
}

#[async_trait]
impl EnvironmentRuntime for ProvisioningDockerRuntime {
    async fn provision_environment(
        &self,
        _name: &str,
        spec: &flotilla_resources::EnvironmentSpec,
    ) -> Result<flotilla_provisioning::reconcilers::EnvironmentProvisioning, String> {
        let spec = spec.docker.as_ref().expect("Docker scenario input");

        self.provisioned.lock().expect("provision log").push((spec.image.clone(), spec.pull_policy));
        Ok(EnvironmentProvisioning {
            provider_descriptor: None,
            survivors: None,
            configured_limits: None,
            backing_id: Some("container".into()),
            image_ref: Some(spec.image.clone()),
            local_image_id: Some(format!("sha256:{}", "9".repeat(64))),
            registry_digest: None,
        })
    }

    async fn destroy(&self, _environment_ref: &str, _container_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn composed_build(delivery: flotilla_resources::ImageDelivery) -> (String, flotilla_resources::ImageBuildSpec) {
    use flotilla_resources::{image_identity, ImageBuildReservation, ImageBuildSpec, ImageLayerSource, ImageLayerSpec, ImageLayerTier};
    let layers = vec![ImageLayerSpec::builder()
        .name("base".into())
        .version("1".into())
        .tier(ImageLayerTier::Base)
        .source(ImageLayerSource::Stdlib)
        .from(format!("ubuntu@sha256:{}", "1".repeat(64)))
        .fragment("FROM ${BASE}\n".into())
        .probe("true".into())
        .build()];
    let identity = image_identity(&layers);
    let spec = ImageBuildSpec::builder()
        .identity(identity.clone())
        .architecture("amd64".into())
        .layers(layers)
        .host_ref("builder".into())
        .delivery(delivery)
        .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
        .build();
    (spec.resource_name(), spec)
}

// ADR 0062: a composed Environment waits for its per-identity ImageBuild with
// the build's state and failure reason, never falling back to another image,
// then provisions the Built digest: a registry digest is digest-pinned and
// pulled IfNotPresent whatever the policy says (the provider pulls it with its
// pull credential); a local image ID uses pull policy Never.
#[rstest::rstest]
#[case::registry(flotilla_resources::ImageDelivery::Registry { repository: "registry.test/crew".into() })]
#[case::local(flotilla_resources::ImageDelivery::Local)]
#[tokio::test]
async fn composed_environment_waits_for_its_build_then_provisions_the_digest(#[case] delivery: flotilla_resources::ImageDelivery) {
    use flotilla_resources::{
        DockerImagePullPolicy, ImageBuild, ImageBuildFailure, ImageBuildFailureClass, ImageBuildPhase, ImageBuildStatus, ImageDelivery,
    };
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let (name, spec) = composed_build(delivery.clone());
    let environments = backend.clone().using::<Environment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder().name("env-composed".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: Some(name.clone()),
                    memory_policy: Default::default(),
                    host_ref: "host-a".to_string(),
                    image: String::new(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: DockerImagePullPolicy::Never,
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("create environment");
    let runtime = Arc::new(ProvisioningDockerRuntime::default());
    let reconciler = EnvironmentReconciler::new(Arc::clone(&runtime), backend.clone(), "flotilla");
    let waiting = |outcome: flotilla_store::controller::ReconcileOutcome<Environment>| match outcome.patch {
        Some(EnvironmentStatusPatch::WaitForImageBuild { message }) => message,
        other => panic!("expected to wait for the image build, got {other:?}"),
    };
    let prepared = reconciler.prepare(&environment).await.expect("prepare");
    assert!(waiting(reconciler.reconcile(&environment, &prepared, chrono::Utc::now())).contains("awaiting replicated build status"));

    let builds = backend.using::<ImageBuild>("flotilla");
    let created = builds.create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await.expect("build");
    let prepared = reconciler.prepare(&environment).await.expect("prepare");
    assert_eq!(waiting(reconciler.reconcile(&environment, &prepared, chrono::Utc::now())), format!("ImageBuild {name} Queued"));

    let failed = ImageBuildStatus {
        phase: ImageBuildPhase::Failed,
        failure: Some(ImageBuildFailure { class: ImageBuildFailureClass::Recipe, reason: "probe `harness-codex` failed".into() }),
        ..Default::default()
    };
    builds.update_status(&name, &created.metadata.resource_version, &failed).await.expect("failed build");
    let prepared = reconciler.prepare(&environment).await.expect("prepare");
    assert_eq!(
        waiting(reconciler.reconcile(&environment, &prepared, chrono::Utc::now())),
        format!("ImageBuild {name} Failed: probe `harness-codex` failed"),
        "a recipe failure waits with its reason and never falls back"
    );
    assert!(runtime.provisioned.lock().expect("provision log").is_empty());

    // Deleting a failed build is the operator retry; a fresh build succeeds.
    builds.delete(&name).await.expect("operator retry");
    let (_, spec) = composed_build(delivery.clone());
    let created = builds.create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await.expect("rebuild");
    let digest = match &delivery {
        ImageDelivery::Registry { repository } => format!("{repository}@sha256:{}", "e".repeat(64)),
        ImageDelivery::Local => format!("sha256:{}", "e".repeat(64)),
    };
    let built = ImageBuildStatus { phase: ImageBuildPhase::Built, digest: Some(digest.clone()), ..Default::default() };
    builds.update_status(&name, &created.metadata.resource_version, &built).await.expect("built");
    let prepared = reconciler.prepare(&environment).await.expect("prepare");
    let outcome = reconciler.reconcile(&environment, &prepared, chrono::Utc::now());
    assert!(matches!(outcome.patch, Some(EnvironmentStatusPatch::MarkReady { image_ref: Some(ref image), .. }) if *image == digest));
    let expected_policy = match delivery {
        ImageDelivery::Registry { .. } => DockerImagePullPolicy::IfNotPresent,
        ImageDelivery::Local => DockerImagePullPolicy::Never,
    };
    assert_eq!(*runtime.provisioned.lock().expect("provision log"), vec![(digest, expected_policy)]);
}

// ADR 0062: an Environment naming a previous-generation per-stage build can
// never be satisfied; it fails with an explicit reason so the vessel's
// recreation path relaunches it, instead of waiting forever.
#[tokio::test]
async fn retired_build_reference_fails_the_environment() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    flotilla_store::apply_resource_document(
        &backend,
        "flotilla",
        serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "ImageBuild", "metadata": {"name": "image-build-old-stage"},
            "spec": {"recipe_key": "sha256:old", "host_ref": "host-a", "attempt": 0}
        }),
    )
    .await
    .expect("previous-generation build record");
    let environment = backend
        .clone()
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("env-old".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: Some("image-build-old-stage".into()),
                    memory_policy: Default::default(),
                    host_ref: "host-a".to_string(),
                    image: String::new(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("create environment");
    let reconciler = EnvironmentReconciler::new(Arc::new(ProvisioningDockerRuntime::default()), backend.clone(), "flotilla");
    let prepared = reconciler.prepare(&environment).await.expect("prepare");
    assert!(matches!(
        reconciler.reconcile(&environment, &prepared, chrono::Utc::now()).patch,
        Some(EnvironmentStatusPatch::MarkFailed { message }) if message.contains("previous-generation image build; relaunch the vessel")
    ));
}

// Environment teardown must preserve the runner while a checkout still needs
// it for cleanup, including a checkout whose deletion/finalizer is in flight.
// The same contract runs on the in-memory and production SQLite stores.
#[tokio::test]
async fn environment_cleanup_waits_for_referencing_checkout_finalizers() {
    use flotilla_resources::{Checkout, CheckoutSpec, FreshCloneCheckoutSpec, RepositoryKey};
    use flotilla_store_sqlite::SqliteBackend;

    let directory = tempfile::tempdir().expect("SQLite test store");
    let sqlite = SqliteBackend::open_async(directory.path().join("environment-cleanup.sqlite")).await.expect("SQLite");
    for backend in [ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()), ResourceBackend::new(sqlite)] {
        let environments = backend.clone().using::<Environment>("flotilla");
        let environment = environments
            .create(
                &InputMeta::builder().name("env-cleanup".into()).build(),
                &EnvironmentSpec {
                    cloud_sandbox: None,
                    provider_address: None,
                    host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host-a".into(), repo_default_dir: "/workspace".into() }),
                    docker: None,
                },
            )
            .await
            .expect("environment");
        let checkouts = backend.clone().using::<Checkout>("flotilla");
        let spec = |environment: &str| {
            CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                source_clone_ref: None,
                repo_ref: RepositoryKey("repo".into()),
                env_ref: environment.into(),
                r#ref: "main".into(),
                base_ref: None,
                target_path: "/workspace".into(),
                url: "https://example.com/o/r".into(),
            })
        };
        checkouts
            .create(&InputMeta::builder().name("own".into()).finalizers(vec!["test-cleanup".into()]).build(), &spec("env-cleanup"))
            .await
            .expect("own checkout");
        checkouts.create(&InputMeta::builder().name("other".into()).build(), &spec("env-other")).await.expect("unrelated checkout");
        // Stands in for the provider/container destruction boundary.
        let runtime = Arc::new(RecordingDockerRuntime::default());
        let reconciler = EnvironmentReconciler::new(Arc::clone(&runtime), backend, "flotilla");
        assert!(matches!(reconciler.run_finalizer(&environment).await, Err(ResourceError::FinalizerPending)));
        checkouts.delete("own").await.expect("begin checkout cleanup");
        let checkout = checkouts.get("own").await.expect("checkout finalizer retains its record");
        assert!(checkout.metadata.deletion_timestamp.is_some());
        assert!(matches!(reconciler.run_finalizer(&environment).await, Err(ResourceError::FinalizerPending)));
        assert!(runtime.cleaned.lock().expect("cleanup log").is_empty());
        let metadata = InputMeta::from(&checkout.metadata).without_finalizer("test-cleanup");
        checkouts.update(&metadata, &checkout.metadata.resource_version, &checkout.spec).await.expect("finish checkout finalizer");
        match checkouts.delete("own").await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            other => panic!("checkout delete: {other:?}"),
        }
        reconciler.run_finalizer(&environment).await.expect("no checkout still needs the environment");
        assert_eq!(*runtime.cleaned.lock().expect("cleanup log"), ["env-cleanup"]);
    }
}

// Provider process boundary: this provider accepts no legacy Docker/host-direct
// input, and returns only an opaque backing identity.
struct NeutralRuntime {
    destroyed: Mutex<Vec<String>>,
}

#[async_trait]
impl EnvironmentRuntime for NeutralRuntime {
    async fn provision_environment(
        &self,
        _: &str,
        spec: &EnvironmentSpec,
    ) -> Result<flotilla_provisioning::reconcilers::EnvironmentProvisioning, String> {
        assert!(spec.docker.is_none() && spec.host_direct.is_none());
        assert!(spec.provider_address.is_some());
        Ok(flotilla_provisioning::reconcilers::EnvironmentProvisioning {
            backing_id: Some("opaque-provider-backing".into()),
            ..Default::default()
        })
    }
    async fn destroy(&self, _: &str, backing: &str) -> Result<(), String> {
        self.destroyed.lock().unwrap().push(backing.into());
        Ok(())
    }
}

// ADR 0058: reconciliation and teardown use provider facts and opaque backing
// IDs even when the provider has neither of the preceding-generation inputs.
#[tokio::test]
async fn neutral_provider_provisions_and_destroys_opaque_backing() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let runtime = Arc::new(NeutralRuntime { destroyed: Mutex::new(Vec::new()) });
    let reconciler = EnvironmentReconciler::new(runtime.clone(), backend.clone(), "flotilla");
    let mut environment = backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("neutral-provider".to_string()).build(),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: Some("account:cloud/provider:sandbox".parse().unwrap()),
                host_direct: None,
                docker: None,
            },
        )
        .await
        .unwrap();
    let prepared = reconciler.prepare(&environment).await.unwrap();
    let outcome = reconciler.reconcile(&environment, &prepared, chrono::Utc::now());
    let mut status = EnvironmentStatus::default();
    outcome.patch.expect("ready patch").apply(&mut status);
    assert_eq!(status.phase, EnvironmentPhase::Ready);
    assert_eq!(status.docker_container_id.as_deref(), Some("opaque-provider-backing"));
    assert!(status.image_ref.is_none());
    environment.status = Some(status);
    reconciler.run_finalizer(&environment).await.unwrap();
    assert_eq!(*runtime.destroyed.lock().unwrap(), ["opaque-provider-backing"]);
}

struct CloudRuntime {
    destroyed: Mutex<Vec<String>>,
}
#[async_trait]
impl EnvironmentRuntime for CloudRuntime {
    async fn provision_environment(&self, _: &str, spec: &EnvironmentSpec) -> Result<EnvironmentProvisioning, String> {
        assert!(spec.cloud_sandbox.is_some() && spec.docker.is_none() && spec.host_direct.is_none());
        Ok(EnvironmentProvisioning {
            backing_id: Some("opaque-cloud-backing".into()),
            image_ref: Some(spec.cloud_sandbox.as_ref().unwrap().image.clone()),
            provider_descriptor: Some(Box::new(flotilla_resources::EnvironmentProviderDescriptor {
                instance: "daytona".into(),
                kind: flotilla_protocol::provider::ProviderKind::new(
                    flotilla_protocol::provider::EnvironmentClass::CloudSandbox,
                    "daytona",
                ),
                image_digest: spec.cloud_sandbox.as_ref().unwrap().image.clone(),
                host_version: "1".into(),
                module_version: "1".into(),
                capabilities: Default::default(),
            })),
            survivors: Some(flotilla_resources::EnvironmentSurvivors {
                host_checkpoint: Some("controller-checkpoint".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    async fn destroy(&self, _: &str, backing: &str) -> Result<(), String> {
        self.destroyed.lock().unwrap().push(backing.into());
        Ok(())
    }
}
// #3620: cloud provisioning stores the opaque backing, descriptor and survivor
// references without populating legacy Docker identity. Finalization uses that
// same opaque identity through the reconciler's kind-neutral runtime port.
#[tokio::test]
async fn cloud_ready_and_finalization_use_generic_backing() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environments = backend.using::<Environment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder().name("cloud-environment".to_string()).build(),
            &EnvironmentSpec {
                provider_address: Some("account:daytona/provider:daytona".parse().unwrap()),
                cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
                    image: "image@sha256:digest".into(),
                    size: flotilla_resources::CloudSandboxSize { cpus: 2, memory_bytes: 1024, disk_bytes: 1024 },
                    lifetime_cap_seconds: None,
                    env: Default::default(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let runtime = Arc::new(CloudRuntime { destroyed: Default::default() });
    let reconciler = EnvironmentReconciler::new(runtime.clone(), backend.clone(), "flotilla");
    let prepared = reconciler.prepare(&environment).await.unwrap();
    let outcome = reconciler.reconcile(&environment, &prepared, chrono::Utc::now());
    let mut status = EnvironmentStatus::default();
    outcome.patch.unwrap().apply(&mut status);
    assert!(status.ready && status.phase == EnvironmentPhase::Ready);
    assert_eq!(status.backing_id.as_deref(), Some("opaque-cloud-backing"));
    assert!(status.docker_container_id.is_none());
    assert_eq!(status.provider_descriptor.as_ref().unwrap().instance, "daytona");
    assert_eq!(status.survivors.as_ref().unwrap().host_checkpoint.as_deref(), Some("controller-checkpoint"));
    environments.update_status(&environment.metadata.name, &environment.metadata.resource_version, &status).await.unwrap();
    let ready = environments.get(&environment.metadata.name).await.unwrap();
    reconciler.run_finalizer(&ready).await.unwrap();
    assert_eq!(*runtime.destroyed.lock().unwrap(), vec!["opaque-cloud-backing"]);
}
