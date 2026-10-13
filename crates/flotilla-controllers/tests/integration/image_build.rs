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
use flotilla_resources::DockerEnvironmentSpec;
use flotilla_resources::DockerImagePullPolicy;
use flotilla_resources::Environment;
use flotilla_resources::EnvironmentPhase;
use flotilla_resources::EnvironmentSpec;
use flotilla_resources::FrozenImageLayer;
use flotilla_resources::Host;
use flotilla_resources::HostSpec;
use flotilla_resources::HostStatus;
use flotilla_resources::ImageBuild;
use flotilla_resources::ImageBuildCapacity;
use flotilla_resources::ImageBuildFailure;
use flotilla_resources::ImageBuildFailureClass;
use flotilla_resources::ImageBuildPhase;
use flotilla_resources::ImageBuildReservation;
use flotilla_resources::ImageBuildSpec;
use flotilla_resources::ImageComposition;
use flotilla_resources::ImageInputStability;
use flotilla_resources::ImageLayerParent;
use flotilla_resources::ImageLayerSelection;
use flotilla_resources::ImageLayerSpec;
use flotilla_resources::ImageLayerStage;
use flotilla_resources::InputMeta;
use flotilla_resources::PlacedImageIdentity;
use flotilla_resources::ResourceError;
use flotilla_store::apply_status_patch;
use flotilla_store::controller::Reconciler;
use flotilla_store::ResourceBackend;
use flotilla_store_testkit::VirtualClock;

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
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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

// #3136 amendment: independently queued producers are shared across distinct
// parents and same-architecture hosts. Only splices depend on the runtime chain.
#[hegel::test]
fn link_producers_share_execution_across_parents_and_hosts(tc: hegel::TestCase) {
    // Both supported architectures and one/two link layers; host and parent
    // differ between two simultaneous demands. No process or network runs.
    let architecture = if tc.draw(hegel::generators::booleans()) { "amd64" } else { "arm64" };
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(2));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let capacity = Some(ImageBuildCapacity::Builder {
            architecture: architecture.into(),
            slots: 2,
            reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
        });
        host(&backend, "local", architecture, capacity.clone()).await;
        host(&backend, "other", architecture, capacity).await;
        let admission = ImageBuildAdmission::new(backend.clone(), "test", Arc::new(Inputs));
        let mut left = composition();
        for index in 0..count {
            let name = format!("harness-{index}");
            let mut layer = left.layers[0].clone();
            layer.name = name.clone();
            layer.spec.stage = ImageLayerStage::Capability;
            layer.spec.parent = ImageLayerParent::Rebasable;
            layer.spec.link = Some(flotilla_resources::LinkImageLayer {
                builder_image: format!("sha256:{}", "d".repeat(64)),
                outputs: BTreeSet::from([format!("/opt/flotilla/layers/{name}/"), format!("/opt/flotilla/bin/tool-{index}")]),
            });
            left.layers.push(layer);
        }
        let mut right = left.clone();
        for layer in right.layers.iter_mut().skip(1) {
            layer.spec.revision = "b".repeat(40);
        }
        right.layers[0].spec.parent = ImageLayerParent::Image(format!("sha256:{}", "e".repeat(64)));
        left.build_refs = admission.join("local", &left).await.expect("left demand");
        right.build_refs = admission.join("other", &right).await.expect("right demand");
        assert_eq!(&left.build_refs[..count], &right.build_refs[..count]);
        assert_ne!(left.build_refs.last(), right.build_refs.last());
        let builds = backend.using::<ImageBuild>("test");
        for reference in &left.build_refs[..count] {
            let producer = builds.get(reference).await.expect("producer");
            assert!(producer.spec.is_link_producer());
            assert_eq!(producer.spec.host_ref, "local");
            assert!(producer.spec.parent_build_ref.is_none());
            assert!(producer.spec.inputs.parent_recipe_key.is_none());
        }
        let runner = Arc::new(Runner { calls: AtomicUsize::new(0), failure: None });
        let local = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "local");
        let other = ImageBuildReconciler::new(runner.clone(), backend.clone(), "test", "other");
        for _ in 0..count + 2 {
            for build in builds.list().await.expect("executions").items {
                let reconciler = if build.spec.host_ref == "local" { &local } else { &other };
                tick(&backend, reconciler, &build.metadata.name).await;
                tick(&backend, reconciler, &build.metadata.name).await;
            }
            left.build_refs = admission.join("local", &left).await.expect("continue left");
            right.build_refs = admission.join("other", &right).await.expect("continue right");
        }
        assert_eq!(left.build_refs.len(), left.build_stage_count());
        assert_eq!(right.build_refs.len(), right.build_stage_count());
        let all = builds.list().await.expect("completed executions").items;
        assert_eq!(all.iter().filter(|build| build.spec.is_link_producer()).count(), count);
        for composition in [&left, &right] {
            for reference in &composition.build_refs[count + 1..] {
                let splice = builds.get(reference).await.expect("splice");
                assert!(splice.spec.link_source.is_some());
                let source = builds.get(&splice.spec.link_source.as_ref().expect("source").build_ref).await.expect("producer evidence");
                assert!(splice.spec.accepts_link_source(&source.spec));
                let mut changed = source.spec.clone();
                changed.inputs.content_hashes = vec![format!("sha256:{}", "f".repeat(64))];
                assert!(!splice.spec.accepts_link_source(&changed));
                assert!(splice.spec.parent_build_ref.is_some());
                assert_eq!(splice.spec.inputs.link_recipe_keys.len(), 1);
                assert_eq!(splice.status.expect("splice status").phase, ImageBuildPhase::Built);
            }
        }
        assert!(admission.completed(architecture, &left).await.expect("completed lookup").is_some());
        // Architecture is a producer input, so another architecture cannot reuse
        // the current producer or runtime chain.
        assert!(admission
            .completed(if architecture == "amd64" { "arm64" } else { "amd64" }, &left)
            .await
            .expect("architecture lookup")
            .is_none());
    });
}

// Provisioning must not bind the completed ordinary parent while independent
// producers or splices remain unfinished. Each transition uses the real store.
#[hegel::test]
fn environment_waits_for_producers_and_every_splice(tc: hegel::TestCase) {
    // One/two producers and both architectures, crossing the former logical
    // layer-count completion boundary before any producer has completed.
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(2));
    let fail_audit = tc.draw(hegel::generators::booleans());
    let arch = if tc.draw(hegel::generators::booleans()) { "amd64" } else { "arm64" };
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        host(&backend, "local", arch, None).await;
        let inputs = Arc::new(Inputs);
        let admission = ImageBuildAdmission::new(backend.clone(), "test", inputs.clone());
        let mut composition = composition();
        for index in 0..count {
            let name = format!("harness-{index}");
            let mut layer = composition.layers[0].clone();
            layer.name = name.clone();
            layer.spec.stage = ImageLayerStage::Capability;
            layer.spec.parent = ImageLayerParent::Rebasable;
            layer.spec.link = Some(flotilla_resources::LinkImageLayer {
                builder_image: format!("sha256:{}", "d".repeat(64)),
                outputs: BTreeSet::from([format!("/opt/flotilla/layers/{name}/")]),
            });
            composition.layers.push(layer);
        }
        composition.build_refs = admission.join("local", &composition).await.expect("initial demand");
        let producer_refs = composition.build_refs[..count].to_vec();
        let root = composition.build_refs.last().expect("root").clone();
        let docker = serde_json::from_value(
            serde_json::json!({"host_ref":"local", "image":"", "image_build_ref":root, "image_composition":composition}),
        )
        .expect("Docker spec");
        let environments = backend.using::<Environment>("test");
        environments
            .create(&InputMeta::builder().name("linked".into()).build(), &EnvironmentSpec { host_direct: None, docker: Some(docker) })
            .await
            .expect("environment");
        let environment = EnvironmentReconciler::new(Arc::new(Docker), backend.clone(), "test").with_image_build_inputs(Some(inputs));
        let runner = Arc::new(Runner { calls: AtomicUsize::new(0), failure: None });
        let reconciler = ImageBuildReconciler::new(runner, backend.clone(), "test", "local");
        tick(&backend, &reconciler, &root).await;
        tick(&backend, &reconciler, &root).await;
        let obj = environments.get("linked").await.expect("environment");
        let prepared = environment.prepare(&obj).await.expect("wait for producers");
        let patch = environment.reconcile(&obj, &prepared, Utc::now()).patch.expect("wait patch");
        apply_status_patch(&environments, "linked", &patch).await.expect("wait status");
        assert_eq!(environments.get("linked").await.expect("waiting").status.expect("status").phase, EnvironmentPhase::Provisioning);
        if fail_audit {
            let producer = &producer_refs[0];
            apply_status_patch(
                &backend.using::<ImageBuild>("test"),
                producer,
                &flotilla_resources::ImageBuildStatusPatch::Failed {
                    at: Utc::now(),
                    failure: ImageBuildFailure { class: ImageBuildFailureClass::Deterministic, reason: "output audit refused".into() },
                    log_ref: "audit-log".into(),
                },
            )
            .await
            .expect("failed audit");
            let obj = environments.get("linked").await.expect("environment");
            let prepared = environment.prepare(&obj).await.expect("failed producer wait");
            let patch = environment.reconcile(&obj, &prepared, Utc::now()).patch.expect("failed producer patch");
            apply_status_patch(&environments, "linked", &patch).await.expect("waiting status");
            let status = environments.get("linked").await.expect("waiting").status.expect("status");
            assert_eq!(status.phase, EnvironmentPhase::Provisioning);
            assert!(status.message.expect("failure message").contains("output audit refused"));
            return;
        }
        for producer in producer_refs {
            tick(&backend, &reconciler, &producer).await;
            tick(&backend, &reconciler, &producer).await;
        }
        for _ in 0..count {
            let obj = environments.get("linked").await.expect("environment");
            let prepared = environment.prepare(&obj).await.expect("wait for splice");
            let patch = environment.reconcile(&obj, &prepared, Utc::now()).patch.expect("splice wait patch");
            apply_status_patch(&environments, "linked", &patch).await.expect("wait status");
            let status = environments.get("linked").await.expect("waiting").status.expect("status");
            assert_eq!(status.phase, EnvironmentPhase::Provisioning);
            let splice = status.image_build_refs.last().expect("splice");
            tick(&backend, &reconciler, splice).await;
            tick(&backend, &reconciler, splice).await;
        }
        let obj = environments.get("linked").await.expect("environment");
        let prepared = environment.prepare(&obj).await.expect("ready");
        let patch = environment.reconcile(&obj, &prepared, Utc::now()).patch.expect("ready patch");
        apply_status_patch(&environments, "linked", &patch).await.expect("ready status");
        assert_eq!(environments.get("linked").await.expect("ready").status.expect("status").phase, EnvironmentPhase::Ready);
    });
}
