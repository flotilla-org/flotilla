use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    DockerEnvironmentRuntime, DockerProvisioning, EnvironmentReconciler, ImageBuildReconciler, ImageBuildResult, ImageBuildRunner,
};
use flotilla_core::image_build::{ImageBuildAdmission, ImageBuildInputResolver, ImageBuildSourceInputs};
use flotilla_protocol::{EnvironmentId, HostSummary, NodeId, NodeInfo, SystemInfo};
use flotilla_resources::{
    apply_status_patch, controller::Reconciler, DockerEnvironmentSpec, DockerImagePullPolicy, Environment, EnvironmentPhase,
    EnvironmentSpec, FrozenImageLayer, Host, HostSpec, HostStatus, ImageBuild, ImageBuildCapacity, ImageBuildFailure,
    ImageBuildFailureClass, ImageBuildPhase, ImageBuildReservation, ImageBuildSpec, ImageComposition, ImageInputStability,
    ImageLayerParent, ImageLayerSelection, ImageLayerSpec, ImageLayerStage, InputMeta, PlacedImageIdentity, ResourceBackend, ResourceError,
    VirtualClock,
};

struct Inputs;
#[async_trait]
impl ImageBuildInputResolver for Inputs {
    // Stands in for immutable Git source acquisition, a subprocess seam.
    async fn resolve(&self, _layer: &FrozenImageLayer, architecture: &str) -> Result<ImageBuildSourceInputs, String> {
        Ok(ImageBuildSourceInputs::builder()
            .content_hashes(vec![format!("sha256:{}", "2".repeat(64))])
            .architecture(architecture.into())
            .stability(ImageInputStability::Pinned)
            .build())
    }
}
struct Runner {
    calls: AtomicUsize,
    failure: Option<ImageBuildFailureClass>,
}
#[async_trait]
impl ImageBuildRunner for Runner {
    // Stands in for Buildx and verification processes; lifecycle uses the real backend.
    async fn build(&self, name: &str, _spec: &ImageBuildSpec, _parent: &str) -> Result<ImageBuildResult, String> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(match self.failure.filter(|_| attempt == 0) {
            Some(class) => ImageBuildResult::Failed {
                failure: ImageBuildFailure { class, reason: "test failure".into() },
                log_ref: format!("log-{name}"),
            },
            None => ImageBuildResult::Built {
                identity: PlacedImageIdentity { local_image_id: format!("sha256:{}", "3".repeat(64)), registry_digest: None },
                verified_provides: BTreeSet::new(),
                log_ref: format!("log-{name}"),
            },
        })
    }
}
struct Docker;
#[async_trait]
impl DockerEnvironmentRuntime for Docker {
    async fn provision(&self, _name: &str, spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        assert_eq!(spec.pull_policy, DockerImagePullPolicy::Never);
        Ok(DockerProvisioning {
            configured_limits: None,
            container_id: "container".into(),
            image_ref: spec.image.clone(),
            local_image_id: spec.image.clone(),
            registry_digest: None,
        })
    }
    async fn destroy(&self, _name: &str, _container: &str) -> Result<(), String> {
        Ok(())
    }
}
fn composition() -> ImageComposition {
    let layer = ImageLayerSpec::builder()
        .stage(ImageLayerStage::Base)
        .parent(ImageLayerParent::Image(format!("sha256:{}", "1".repeat(64))))
        .repository("https://example.test/repo".into())
        .revision("a".repeat(40))
        .fragment("Dockerfile".into())
        .build();
    ImageComposition::builder()
        .selection(ImageLayerSelection::builder().base("base".into()).build())
        .needs(BTreeSet::new())
        .layers(vec![FrozenImageLayer { name: "base".into(), spec: layer }])
        .build()
}
async fn host(backend: &ResourceBackend, name: &str, architecture: &str, capacity: Option<ImageBuildCapacity>) {
    let hosts = backend.using::<Host>("test");
    let obj = hosts
        .create(&InputMeta::builder().name(name.into()).build(), &HostSpec { image_build_capacity: capacity, ..Default::default() })
        .await
        .expect("host");
    let status = HostStatus::builder()
        .capabilities(Default::default())
        .ready(true)
        .description(
            HostSummary::builder()
                .environment_id(EnvironmentId::new(name))
                .node(NodeInfo::new(NodeId::new(name), name.to_string()))
                .system(SystemInfo { arch: Some(architecture.into()), ..Default::default() })
                .build(),
        )
        .build();
    hosts.update_status(name, &obj.metadata.resource_version, &status).await.expect("host status");
}
async fn tick<R: ImageBuildRunner + 'static>(backend: &ResourceBackend, reconciler: &ImageBuildReconciler<R>, name: &str) {
    let builds = backend.using::<ImageBuild>("test");
    let obj = builds.get(name).await.expect("build");
    let prepared = reconciler.prepare(&obj).await.expect("prepare");
    let outcome = reconciler.reconcile(&obj, &prepared, Utc::now());
    if let Some(patch) = outcome.patch {
        apply_status_patch(&builds, name, &patch).await.expect("status patch");
    }
}

// #2728: concurrent convoys join one pinned build, and restarting/reconciling
// completed executions never runs another process. Generate two through eight
// concurrent demands and zero through eight duplicate reconciliation steps.
#[hegel::test]
fn concurrent_demands_join_one_execution(tc: hegel::TestCase) {
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(2).max_value(8));
    let repeats = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(8));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = ResourceBackend::InMemory(Default::default());
        host(&backend, "local", "amd64", None).await;
        let admission = ImageBuildAdmission::new(backend.clone(), "test", Arc::new(Inputs));
        let composition = composition();
        let joined = futures::future::join_all((0..count).map(|_| admission.join("local", &composition))).await;
        let names = joined[0].as_ref().expect("join");
        for result in &joined {
            assert_eq!(result.as_ref().expect("join"), names);
        }
        assert_eq!(backend.using::<ImageBuild>("test").list().await.expect("list").items.len(), 1);
        let runner = Arc::new(Runner { calls: AtomicUsize::new(0), failure: None });
        let reconciler = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "local");
        tick(&backend, &reconciler, &names[0]).await;
        let reserved = backend.using::<ImageBuild>("test").get(&names[0]).await.expect("reservation");
        assert_eq!(reserved.status.expect("started").phase, ImageBuildPhase::Building);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
        tick(&backend, &reconciler, &names[0]).await;
        for _ in 0..repeats {
            tick(&backend, &reconciler, &names[0]).await;
        }
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    });
}

// #2728: transient failures get bounded successor executions; deterministic
// failures remain terminal. Environments preserve the failure reason while
// waiting, and the original failed execution remains immutable.
#[tokio::test]
async fn failure_waits_and_retry_classification_preserves_execution_evidence() {
    for class in [ImageBuildFailureClass::Transient, ImageBuildFailureClass::Deterministic] {
        let backend = ResourceBackend::InMemory(Default::default());
        host(&backend, "local", "amd64", None).await;
        let names = ImageBuildAdmission::new(backend.clone(), "test", Arc::new(Inputs)).join("local", &composition()).await.expect("join");
        let runner = Arc::new(Runner { calls: AtomicUsize::new(0), failure: Some(class) });
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let reconciler = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "local").with_clock(clock.clone());
        tick(&backend, &reconciler, &names[0]).await;
        tick(&backend, &reconciler, &names[0]).await;
        let failed = backend.using::<ImageBuild>("test").get(&names[0]).await.expect("failed");
        let environments = backend.using::<Environment>("test");
        let docker =
            serde_json::from_value(serde_json::json!({"host_ref":"local", "image":"", "image_build_ref":names[0]})).expect("docker spec");
        let env = environments
            .create(&InputMeta::builder().name("env".into()).build(), &EnvironmentSpec { host_direct: None, docker: Some(docker) })
            .await
            .expect("environment");
        let environment = EnvironmentReconciler::new(Arc::new(Docker), backend.clone(), "test");
        let prepared = environment.prepare(&env).await.expect("environment prepare");
        let patch = environment.reconcile(&env, &prepared, Utc::now()).patch.expect("waiting patch");
        apply_status_patch(&environments, "env", &patch).await.expect("wait");
        let status = environments.get("env").await.expect("environment").status.expect("status");
        assert_eq!(status.phase, EnvironmentPhase::Provisioning);
        assert!(status.message.expect("reason").contains("test failure"));
        for _ in 0..3 {
            tick(&backend, &reconciler, &names[0]).await;
        }
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.using::<ImageBuild>("test").get(&names[0]).await.expect("old execution").status, failed.status);
        let retry = backend.using::<ImageBuild>("test").get(&format!("{}-retry", names[0])).await;
        if class == ImageBuildFailureClass::Transient {
            let retry = retry.expect("retry execution");
            assert!(retry.spec.not_before.expect("backoff") > failed.status.as_ref().expect("failed status").finished_at.expect("finish"));
            tick(&backend, &reconciler, &retry.metadata.name).await;
            assert_eq!(runner.calls.load(Ordering::SeqCst), 1, "backoff holds");
            clock.advance(chrono::Duration::seconds(31));
            tick(&backend, &reconciler, &retry.metadata.name).await;
            tick(&backend, &reconciler, &retry.metadata.name).await;
            assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
            let env = environments.get("env").await.expect("waiting environment");
            let prepared = environment.prepare(&env).await.expect("provision built retry");
            let patch = environment.reconcile(&env, &prepared, Utc::now()).patch.expect("ready");
            apply_status_patch(&environments, "env", &patch).await.expect("ready patch");
            assert_eq!(environments.get("env").await.expect("ready environment").status.expect("status").phase, EnvironmentPhase::Ready);
        } else {
            assert!(matches!(retry, Err(ResourceError::NotFound { .. })));
        }
    }
}

// Placement-host builds work without a registry; explicit refusal selects only
// a declared same-architecture builder. A wrong architecture is structural.
#[tokio::test]
async fn builder_selection_is_same_architecture_and_prefers_placement_host() {
    let backend = ResourceBackend::InMemory(Default::default());
    host(&backend, "local", "amd64", None).await;
    let admission = ImageBuildAdmission::new(backend.clone(), "test", Arc::new(Inputs));
    let names = admission.join("local", &composition()).await.expect("placement builds");
    assert_eq!(backend.using::<ImageBuild>("test").get(&names[0]).await.expect("build").spec.host_ref, "local");
    host(&backend, "small", "amd64", Some(ImageBuildCapacity::None)).await;
    host(
        &backend,
        "arm",
        "arm64",
        Some(ImageBuildCapacity::Builder {
            architecture: "arm64".into(),
            slots: 1,
            reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
        }),
    )
    .await;
    assert!(admission.join("small", &composition()).await.expect_err("no cross architecture").contains("no host"));
    host(
        &backend,
        "builder",
        "amd64",
        Some(ImageBuildCapacity::Builder {
            architecture: "amd64".into(),
            slots: 1,
            reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
        }),
    )
    .await;
    let names = admission.join("small", &composition()).await.expect("declared builder");
    assert_eq!(backend.using::<ImageBuild>("test").get(&names[0]).await.expect("build").spec.host_ref, "builder");
}

// A child stage is created only after its parent's realised digest is known;
// that digest is frozen in both its input key and execution evidence. Generate
// chains of one through three stages and check every provisioning transition.
#[hegel::test]
fn stages_freeze_realised_parent_identity_before_building(tc: hegel::TestCase) {
    let stages = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = ResourceBackend::InMemory(Default::default());
        host(&backend, "local", "amd64", None).await;
        let inputs = Arc::new(Inputs);
        let admission = ImageBuildAdmission::new(backend.clone(), "test", inputs.clone());
        let mut composition = composition();
        for index in 1..stages {
            let mut layer = composition.layers[0].clone();
            layer.name = format!("stage-{index}");
            layer.spec.stage = if index == 1 { ImageLayerStage::Utilities } else { ImageLayerStage::Harness };
            layer.spec.parent = if index == 1 { ImageLayerParent::Layer("base".into()) } else { ImageLayerParent::Rebasable };
            composition.layers.push(layer);
        }
        composition.build_refs = admission.join("local", &composition).await.expect("admit root");
        assert_eq!(composition.build_refs.len(), 1, "unborn parent digests cannot be recipe inputs");
        let docker = serde_json::from_value(serde_json::json!({"host_ref":"local","image":"", "image_build_ref":composition.build_refs[0], "image_composition":composition})).expect("Docker spec");
        let environments = backend.using::<Environment>("test");
        environments.create(&InputMeta::builder().name("chain".into()).build(), &EnvironmentSpec { host_direct:None,docker:Some(docker) }).await.expect("environment");
        let environment = EnvironmentReconciler::new(Arc::new(Docker), backend.clone(), "test").with_image_build_inputs(Some(inputs));
        let runner = Arc::new(Runner {calls:AtomicUsize::new(0),failure:None});
        let reconciler = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "local");
        for index in 0..stages {
            let env = environments.get("chain").await.expect("environment");
            let prepared = environment.prepare(&env).await.expect("prepare stage");
            let patch = environment.reconcile(&env,&prepared,Utc::now()).patch.expect("wait");
            apply_status_patch(&environments,"chain",&patch).await.expect("wait patch");
            let env = environments.get("chain").await.expect("waiting environment");
            let status = env.status.expect("waiting status");
            assert_eq!(status.phase,EnvironmentPhase::Provisioning);
            assert_eq!(status.image_build_refs.len(),index+1);
            let name = status.image_build_refs.last().expect("stage");
            let build = backend.using::<ImageBuild>("test").get(name).await.expect("stage inputs");
            if index > 0 {
                assert_eq!(build.spec.inputs.parent_digest,format!("sha256:{}","3".repeat(64)));
                assert_eq!(build.spec.recipe_key,build.spec.inputs.recipe_key().expect("resolved key"));
            }
            tick(&backend,&reconciler,name).await;
            tick(&backend,&reconciler,name).await;
        }
        let env = environments.get("chain").await.expect("environment");
        let prepared = environment.prepare(&env).await.expect("ready prepare");
        let patch = environment.reconcile(&env,&prepared,Utc::now()).patch.expect("ready");
        apply_status_patch(&environments,"chain",&patch).await.expect("ready patch");
        assert_eq!(environments.get("chain").await.expect("ready environment").status.expect("ready status").phase,EnvironmentPhase::Ready);
        assert_eq!(runner.calls.load(Ordering::SeqCst),stages);
    });
}

// #2729: a completed pinned execution can serve a non-builder host, while a
// vessel waits visibly without availability, then pins an injected verified ID.
#[tokio::test]
async fn remote_completed_digest_is_reused_and_delivery_freezes_environment_identity() {
    struct Delivered(bool);
    #[async_trait]
    impl DockerEnvironmentRuntime for Delivered {
        async fn ensure_image(
            &self,
            build: &flotilla_resources::ResourceObject<ImageBuild>,
            host: &str,
        ) -> Result<Option<PlacedImageIdentity>, String> {
            assert_eq!(host, "small");
            if !self.0 {
                return Err("requested digest is not held on this host and no fleet registry cache is declared".into());
            }
            Ok(build.status.as_ref().and_then(|status| status.identity.clone()))
        }
        async fn provision(&self, name: &str, spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
            Docker.provision(name, spec).await
        }
        async fn destroy(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let backend = ResourceBackend::InMemory(Default::default());
    host(&backend, "builder", "amd64", None).await;
    host(&backend, "small", "amd64", Some(ImageBuildCapacity::None)).await;
    let admission = ImageBuildAdmission::new(backend.clone(), "test", Arc::new(Inputs));
    let names = admission.join("builder", &composition()).await.expect("build");
    let runner = Arc::new(Runner { calls: AtomicUsize::new(0), failure: None });
    let reconciler = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "builder");
    tick(&backend, &reconciler, &names[0]).await;
    for _ in 0..16 {
        tick(&backend, &reconciler, &names[0]).await;
        if backend
            .using::<ImageBuild>("test")
            .get(&names[0])
            .await
            .expect("build")
            .status
            .as_ref()
            .is_some_and(|status| status.phase == ImageBuildPhase::Built)
        {
            break;
        }
    }
    assert_eq!(admission.join("small", &composition()).await.expect("reuse"), names);
    assert_eq!(backend.using::<ImageBuild>("test").list().await.expect("executions").items.len(), 1);
    let spec =
        serde_json::from_value(serde_json::json!({"host_ref":"small", "image":"", "image_build_ref":names[0]})).expect("Docker spec");
    let environments = backend.using::<Environment>("test");
    let env = environments
        .create(&InputMeta::builder().name("remote".into()).build(), &EnvironmentSpec { host_direct: None, docker: Some(spec) })
        .await
        .expect("environment");
    let unavailable = EnvironmentReconciler::new(Arc::new(Delivered(false)), backend.clone(), "test");
    let prepared = unavailable.prepare(&env).await.expect("wait");
    let patch = unavailable.reconcile(&env, &prepared, Utc::now()).patch.expect("waiting");
    apply_status_patch(&environments, "remote", &patch).await.expect("visible wait");
    let env = environments.get("remote").await.expect("waiting environment");
    let status = env.status.as_ref().expect("status");
    assert_eq!(status.phase, EnvironmentPhase::Provisioning);
    assert!(status.message.as_deref().expect("reason").contains("no fleet registry cache"));
    assert!(status.local_image_id.is_none());
    let environment = EnvironmentReconciler::new(Arc::new(Delivered(true)), backend.clone(), "test");
    let prepared = environment.prepare(&env).await.expect("deliver");
    let patch = environment.reconcile(&env, &prepared, Utc::now()).patch.expect("ready");
    apply_status_patch(&environments, "remote", &patch).await.expect("freeze");
    let ready = environments.get("remote").await.expect("ready").status.expect("status");
    assert_eq!(ready.phase, EnvironmentPhase::Ready);
    assert_eq!(ready.local_image_id, Some(format!("sha256:{}", "3".repeat(64))));
    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
}

// #2733: collection leaves immutable execution evidence, but future demand must
// build a fresh execution instead of forever joining an unavailable old digest.
#[tokio::test]
async fn retired_recipe_demands_join_a_rebuilt_successor() {
    let backend = ResourceBackend::InMemory(Default::default());
    host(&backend, "local", "amd64", None).await;
    let admission = ImageBuildAdmission::new(backend.clone(), "test", Arc::new(Inputs));
    let composition = composition();
    let original = admission.join("local", &composition).await.expect("original");
    let runner = Arc::new(Runner { calls: AtomicUsize::new(0), failure: None });
    let reconciler = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "local");
    tick(&backend, &reconciler, &original[0]).await;
    tick(&backend, &reconciler, &original[0]).await;
    let builds = backend.using::<ImageBuild>("test");
    let built = builds.get(&original[0]).await.expect("built");
    let mut status = built.status.clone().expect("status");
    assert_eq!(status.phase, ImageBuildPhase::Built);
    status.availability.retired = true;
    status.availability.retired_at = Some(Utc::now());
    builds.update_status(&original[0], &built.metadata.resource_version, &status).await.expect("retire");
    assert!(admission.completed("amd64", &composition).await.expect("lookup").is_none());
    let (first, second) = tokio::join!(admission.join("local", &composition), admission.join("local", &composition));
    let successor = first.expect("successor");
    assert_eq!(successor, second.expect("concurrent successor"));
    assert_ne!(successor, original);
    assert_eq!(successor[0], format!("{}-rebuilt", original[0]));
    tick(&backend, &reconciler, &successor[0]).await;
    tick(&backend, &reconciler, &successor[0]).await;
    assert!(admission.completed("amd64", &composition).await.expect("new image").is_some());
    assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
    assert_eq!(builds.get(&original[0]).await.expect("original evidence").status, Some(status));
}
