//! `flotilla image check --project X --ref <branch>` (ADR 0062 B3) through
//! its seams: the store is in-memory, the branch's layer directory comes from
//! a stand-in for the VCS read, and the builds it creates are executed by the
//! real ImageBuild reconciler over the build-service stand-in.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{atomic::Ordering, Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_admission::repository_inspection::RepositoryInspector;
use flotilla_charter::charter_store::{LayerDirectorySnapshot, PROJECT_IMAGE_LAYER_PATH, PROJECT_IMAGE_MANAGER};
use flotilla_core::in_process::check_image;
use flotilla_core::in_process::image_build_report;
use flotilla_core::in_process::ImageCheckSources;
use flotilla_core::repository_inspection::GitRepositoryInspector;
use flotilla_discovery_testkit::test_vcs_resolver;
use flotilla_protocol::image_check::{ImageBuildReportPhase, ImageCheckAgentSource, ImageCheckReport, ImageCheckRequest, ImageProbeReport};
use flotilla_provider_runtime::{ChannelLabel, CommandRunner, ProcessCommandRunner};
use flotilla_resources::{
    parse_layer_directory, resolve_image, CharterSource, ImageBuild, ImageBuildFailureClass, ImageLayer, ImageLayerSource, ImageLayerSpec,
    ImageLayerTier, ImageProbeOutcome, ImageRequest, ImageSelection, InputMeta, Project, ProjectRepositoryRole, ProjectRepositorySpec,
    ProjectSpec, Repository, RepositorySpec, RepositoryStatus, MANAGED_BY_LABEL,
};
use flotilla_store::ResourceBackend;
use flotilla_store_testkit::VirtualClock;

use super::image_build::{builder, host, reconciler, settle, Behaviour, BuildService, Materials, HOST, NS};

const PROJECT: &str = "app";
const REPO: &str = "https://github.com/example/app";

/// Stands in for fetching a branch and reading `.flotilla/image/` through the
/// VCS seam: one directory per branch (absent when unmapped), recording what
/// was read.
#[derive(Default)]
struct Branches {
    layers: Mutex<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    reads: Mutex<Vec<CharterSource>>,
}

#[async_trait]
impl ImageCheckSources for Branches {
    async fn layer_snapshot(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
        self.reads.lock().expect("reads").push(source.clone());
        let CharterSource::Repository { branch, .. } = source else { panic!("the primary repository is a repository source") };
        Ok(self
            .layers
            .lock()
            .expect("layers")
            .get(branch)
            .cloned()
            .map(|files| LayerDirectorySnapshot { revision: format!("{branch}-head"), files }))
    }

    async fn default_harness_adapters(&self, _: &str, project_ref: &str, _: &ProjectSpec) -> Result<BTreeSet<String>, String> {
        assert_eq!(project_ref, PROJECT);
        Ok(BTreeSet::from(["codex".to_string()]))
    }
}

fn shared(name: &str, tier: ImageLayerTier, provides: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0".into())
        .tier(tier)
        // Installation layers: the shipped stdlib is always in the catalogue (#3824 layer 2a), and
        // an installation layer of a stdlib name takes its place.
        .source(ImageLayerSource::Installation)
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu@sha256:{}", "1".repeat(64))))
        .fragment("FROM ${BASE}\nRUN true\n".into())
        .probe(format!("probe {name}"))
        .provides(provides.iter().map(|provide| provide.to_string()).collect())
        .build()
}

fn project_layer_files(version: &str, fragment: &str) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (
            "layer.yaml".to_string(),
            format!("name: app-native\nversion: {version}\ntier: project\nprovides: [project:app]\nrequires: [toolchain:rust]\nprobe: app --version\n")
                .into_bytes(),
        ),
        ("Dockerfile.fragment".to_string(), format!("FROM ${{BASE}}\n{fragment}\n").into_bytes()),
    ])
}

async fn apply_layer(backend: &ResourceBackend, layer: &ImageLayerSpec, labels: BTreeMap<String, String>) {
    backend
        .definitions::<ImageLayer>(NS)
        .apply(&InputMeta::builder().name(layer.resource_name()).labels(labels).build(), &layer.clone().into())
        .await
        .expect("layer");
}

/// A project whose primary repository's default branch carries layer 1.0
/// (already synced), selecting `rust` over a stdlib catalogue with one
/// harness, on a single amd64 host that builds locally.
async fn fixture() -> (ResourceBackend, Arc<Branches>) {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "x86_64", builder(1)).await;
    let repository = RepositorySpec::remote(REPO).expect("repository");
    let created = backend
        .using::<Repository>(NS)
        .create(&InputMeta::builder().name(repository.key().0.clone()).build(), &repository)
        .await
        .expect("repository");
    backend
        .using::<Repository>(NS)
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &RepositoryStatus { default_branch: Some("main".into()), ..Default::default() },
        )
        .await
        .expect("default branch");
    let mut project = ProjectSpec::builder().display_name("App".into()).build();
    project.repositories =
        vec![ProjectRepositorySpec::builder().repo(repository.key()).roles([ProjectRepositoryRole::Code].into()).build()];
    project.image = Some(ImageSelection { base: None, layers: vec!["rust".into()] });
    backend.using::<Project>(NS).create(&InputMeta::builder().name(PROJECT.into()).build(), &project).await.expect("project");

    for layer in [
        shared("base-ubuntu-24.04", ImageLayerTier::Base, &[]),
        shared("rust", ImageLayerTier::Shared, &["toolchain:rust"]),
        shared("harness-codex", ImageLayerTier::AgentRuntime, &["harness:codex"]),
        shared("crew-utilities", ImageLayerTier::AgentRuntime, &["crew:utilities"]),
    ] {
        apply_layer(&backend, &layer, BTreeMap::new()).await;
    }
    let main = project_layer_files("1.0", "RUN app-install");
    let synced = parse_layer_directory(&main, ImageLayerSource::Project, Some(PROJECT)).expect("main layer");
    apply_layer(&backend, &synced, BTreeMap::from([(MANAGED_BY_LABEL.to_string(), PROJECT_IMAGE_MANAGER.to_string())])).await;
    let branches = Arc::new(Branches::default());
    branches.layers.lock().expect("layers").insert("main".into(), main);
    (backend, branches)
}

fn request(git_ref: &str) -> ImageCheckRequest {
    ImageCheckRequest::builder().project(PROJECT.into()).git_ref(git_ref.into()).build()
}

async fn check(backend: &ResourceBackend, branches: &Branches, request: &ImageCheckRequest) -> ImageCheckReport {
    check_image(backend, NS, HOST, branches, request, Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads)).await.expect("check")
}

async fn build_names(backend: &ResourceBackend) -> Vec<String> {
    backend.using::<ImageBuild>(NS).list().await.expect("builds").items.into_iter().map(|build| build.metadata.name).collect()
}

/// Run the check's build to completion through the reconciler.
async fn execute(backend: &ResourceBackend, service: &Arc<BuildService>, name: &str) {
    let spec = backend.using::<ImageBuild>(NS).get(name).await.expect("build").spec.current().expect("current").clone();
    service.expect(&spec);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(backend, service, &materials, &clock);
    settle(backend, &reconciler, &clock, name).await;
}

// The ref's project layer replaces the synced one: main reproduces the
// identity admission resolves from the synced catalogue, a branch that
// changes the layer composes a different identity, and both build and probe
// through the build service, reported per layer.
#[tokio::test]
async fn a_branch_layer_change_builds_a_new_identity() {
    let (backend, branches) = fixture().await;
    branches.layers.lock().expect("layers").insert("feature/native".into(), project_layer_files("1.1", "RUN app-install --native"));

    let main = check(&backend, &branches, &request("main")).await;
    let admitted = resolve_image(
        &flotilla_admission::image_build::read_image_catalogue(&backend, NS).await.expect("catalogue"),
        &ImageRequest::builder()
            .project(PROJECT.into())
            .selection(ImageSelection { base: None, layers: vec!["rust".into()] })
            .harness_adapters(BTreeSet::from(["codex".to_string()]))
            .build(),
    )
    .expect("admission composition");
    let main_build = main.build.clone().expect("main build");
    assert_eq!(main_build.identity, admitted.identity, "checking the default branch reproduces admission's identity");
    assert_eq!((main.agents.as_slice(), main.agents_from), (["codex".to_string()].as_slice(), ImageCheckAgentSource::DefaultWorkflow));

    let feature = check(&backend, &branches, &request("feature/native")).await;
    assert_eq!(
        branches.reads.lock().expect("reads").last(),
        Some(&CharterSource::Repository { repo: REPO.into(), branch: "feature/native".into(), path: PROJECT_IMAGE_LAYER_PATH.into() }),
        "the primary repository's layer directory is read at the ref"
    );
    assert_eq!((feature.revision.as_deref(), feature.project_layer.as_deref()), (Some("feature/native-head"), Some("app-native@1.1")));
    assert!(!feature.reused);
    let build = feature.build.expect("feature build");
    assert_ne!(build.identity, main_build.identity, "a changed project layer is a new identity");
    assert_eq!(build_names(&backend).await.len(), 2);
    assert_eq!((build.builder_host.as_str(), build.delivery.as_str(), build.architecture.as_str()), (HOST, "local", "amd64"));
    assert!(build.layers.iter().all(|layer| layer.probe == ImageProbeReport::Pending));

    let service = BuildService::new(Behaviour::Succeed);
    execute(&backend, &service, &build.name).await;
    let built = image_build_report(&backend, NS, &build.name, Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads))
        .await
        .expect("report");
    assert_eq!((built.phase, built.settled), (ImageBuildReportPhase::Built, true));
    assert!(built.digest.is_some() && built.log_ref.is_some());
    let table = built.layers.iter().map(|layer| (layer.layer.as_str(), layer.source.as_str(), layer.version.as_str())).collect::<Vec<_>>();
    assert_eq!(
        table,
        [
            ("base-ubuntu-24.04", "installation", "1.0"),
            ("rust", "installation", "1.0"),
            ("app-native", "project", "1.1"),
            ("crew-utilities", "installation", "1.0"),
            ("harness-codex", "installation", "1.0")
        ]
    );
    assert!(built.layers.iter().all(|layer| layer.probe == ImageProbeReport::Passed));

    // A ref without `.flotilla/image/` composes no project layer, even
    // though the default branch's layer is synced.
    let bare = check(&backend, &branches, &request("no-layer")).await;
    assert_eq!(bare.project_layer, None);
    assert!(bare.build.expect("bare build").layers.iter().all(|layer| layer.layer != "app-native"));
}

// A recipe failure reports every layer's probe, naming the failing layer,
// with the failure class, reason and build log. Re-checking the same
// identity reuses that build and its result: nothing is retried.
#[tokio::test]
async fn a_recipe_failure_is_reported_per_layer_and_reused_on_recheck() {
    let (backend, branches) = fixture().await;
    branches.layers.lock().expect("layers").insert("broken".into(), project_layer_files("1.2", "RUN false"));
    let first = check(&backend, &branches, &request("broken")).await;
    let name = first.build.expect("build").name;
    let service = BuildService::new(Behaviour::Fail {
        class: ImageBuildFailureClass::Recipe,
        probes: vec![
            ImageProbeOutcome::Passed,
            ImageProbeOutcome::Passed,
            ImageProbeOutcome::Failed { reason: "app: not found".into() },
            ImageProbeOutcome::NotRun,
        ],
        failed_step: None,
    });
    execute(&backend, &service, &name).await;

    let again = check(&backend, &branches, &request("broken")).await;
    assert!(again.reused, "an identical identity reuses its build");
    assert!(again.failed());
    let report = again.build.expect("build");
    assert_eq!(report.name, name);
    assert_eq!((report.phase, report.settled), (ImageBuildReportPhase::Failed, true));
    let probes = report.layers.iter().map(|layer| (layer.layer.as_str(), layer.probe.clone())).collect::<Vec<_>>();
    assert_eq!(
        probes,
        [
            ("base-ubuntu-24.04", ImageProbeReport::Passed),
            ("rust", ImageProbeReport::Passed),
            ("app-native", ImageProbeReport::Failed { reason: "app: not found".into() }),
            ("crew-utilities", ImageProbeReport::NotRun),
            ("harness-codex", ImageProbeReport::Passed),
        ]
    );
    let failure = report.failure.expect("failure");
    assert_eq!(failure.class, "recipe");
    assert!(failure.reason.contains("layer `app-native` 1.2 from project `app`"), "{}", failure.reason);
    assert!(report.log_ref.is_some(), "the build log artifact is named");
    assert_eq!(build_names(&backend).await, [name]);
    assert_eq!(service.calls.load(Ordering::SeqCst), 1, "a re-check never retries a recipe failure");
}

// Refusals name what they refuse and create no build: an unresolvable
// `--need`, a need two layers provide, a host need, an invalid layer
// directory at the ref, and an unknown harness.
#[tokio::test]
async fn refusals_create_no_build() {
    let (backend, branches) = fixture().await;
    for name in ["dup-a", "dup-b"] {
        apply_layer(&backend, &shared(name, ImageLayerTier::Shared, &["toolchain:dup"]), BTreeMap::new()).await;
    }
    let mut invalid = project_layer_files("1.3", "RUN true");
    invalid.insert("Dockerfile.fragment".into(), b"FROM ubuntu\n".to_vec());
    branches.layers.lock().expect("layers").insert("invalid".into(), invalid);

    let cases: [(ImageCheckRequest, &[&str], &str); 5] = [
        (ImageCheckRequest { needs: vec!["toolchain:nope".into()], ..request("main") }, &[], "unresolvable image need `toolchain:nope`"),
        (
            ImageCheckRequest { needs: vec!["toolchain:dup".into()], ..request("main") },
            &["dup-a", "dup-b"],
            "duplicate providers for `toolchain:dup`: dup-a, dup-b",
        ),
        (ImageCheckRequest { needs: vec!["hardware:gpu".into()], ..request("main") }, &[], "is a host need, not an image need"),
        (request("invalid"), &[".flotilla/image/"], "FROM"),
        (ImageCheckRequest { agents: vec!["claude".into()], ..request("main") }, &[], "unresolvable image need `harness:claude`"),
    ];
    for (request, layers, message) in cases {
        let report = check(&backend, &branches, &request).await;
        assert!(report.failed() && report.build.is_none(), "{report:?}");
        let refusal =
            report.refusals.iter().find(|refusal| refusal.message.contains(message)).unwrap_or_else(|| panic!("{message}: {report:?}"));
        assert_eq!(refusal.layers, layers.iter().map(|layer| layer.to_string()).collect::<Vec<_>>());
    }
    assert!(build_names(&backend).await.is_empty(), "no refused check creates a build");

    // A duplicate provider the composition does not touch is not its refusal.
    let unrelated = check(&backend, &branches, &request("main")).await;
    assert!(unrelated.refusals.is_empty() && unrelated.build.is_some(), "{unrelated:?}");
}

// Without a fleet image cache, builds run on and for this host: another
// architecture is refused before anything is created.
#[tokio::test]
async fn another_architecture_needs_the_fleet_image_cache() {
    let (backend, branches) = fixture().await;
    let error = check_image(
        &backend,
        NS,
        HOST,
        &*branches,
        &ImageCheckRequest { architecture: Some("arm64".into()), ..request("main") },
        Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads),
    )
    .await
    .expect_err("refused");
    assert!(error.contains("no fleet image cache") && error.contains("arm64"), "{error}");
    assert!(build_names(&backend).await.is_empty());
}

/// The daemon's real layer read (`GitRepositoryInspector`, which fetches the
/// branch into a bare object cache through the VCS seam) over a fixture
/// repository standing in for the project's primary repository.
struct GitSources {
    inspector: GitRepositoryInspector,
    repository: String,
}

#[async_trait]
impl ImageCheckSources for GitSources {
    async fn layer_snapshot(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
        let CharterSource::Repository { repo, branch, path } = source else { panic!("the primary repository is a repository source") };
        assert_eq!(repo, REPO);
        let local = CharterSource::Repository { repo: self.repository.clone(), branch: branch.clone(), path: path.clone() };
        self.inspector.image_layer_snapshot(&local).await
    }

    async fn default_harness_adapters(&self, _: &str, _: &str, _: &ProjectSpec) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::from(["codex".to_string()]))
    }
}

async fn fixture_git(root: &std::path::Path, args: &[&str]) {
    let args = [&["-c", "user.name=Test", "-c", "user.email=test@example.com"], args].concat();
    ProcessCommandRunner.run("git", &args, root, &ChannelLabel::Default).await.expect("fixture git");
}

// A ref that does not exist is an error naming the ref, never a branch that
// "composes no project layer": only a fetched branch without
// `.flotilla/image/` reports no project layer. Exercised through the real
// fetch-and-read the daemon uses.
#[tokio::test]
async fn a_missing_ref_is_an_error_not_an_absent_layer() {
    let (backend, _) = fixture().await;
    let repository = tempfile::tempdir().expect("repository");
    let cache = tempfile::tempdir().expect("cache");
    let root = repository.path();
    fixture_git(root, &["init", "-q", "-b", "main"]).await;
    let layer = root.join(PROJECT_IMAGE_LAYER_PATH);
    std::fs::create_dir_all(&layer).expect("layer directory");
    for (name, contents) in project_layer_files("1.0", "RUN app-install") {
        std::fs::write(layer.join(name), contents).expect("layer file");
    }
    fixture_git(root, &["add", "."]).await;
    fixture_git(root, &["commit", "-q", "-m", "layer"]).await;
    fixture_git(root, &["checkout", "-q", "-b", "no-layer"]).await;
    fixture_git(root, &["rm", "-rq", ".flotilla"]).await;
    fixture_git(root, &["commit", "-q", "-m", "no layer"]).await;
    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
    let sources = GitSources {
        inspector: GitRepositoryInspector::new(runner.clone(), test_vcs_resolver(runner), HOST).with_charter_cache(cache.path().into()),
        repository: root.to_str().expect("repository path").to_string(),
    };

    let main = check_image(&backend, NS, HOST, &sources, &request("main"), Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads))
        .await
        .expect("main");
    assert_eq!(main.project_layer.as_deref(), Some("app-native@1.0"));
    assert!(main.revision.is_some());

    let bare =
        check_image(&backend, NS, HOST, &sources, &request("no-layer"), Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads))
            .await
            .expect("a branch without a layer");
    assert_eq!(bare.project_layer, None);
    assert!(bare.revision.is_none() && bare.refusals.is_empty() && bare.build.is_some(), "{bare:?}");

    let builds = build_names(&backend).await;
    let error = check_image(
        &backend,
        NS,
        HOST,
        &sources,
        &request("no-such-branch"),
        Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads),
    )
    .await
    .expect_err("a missing ref is an error");
    assert!(error.contains(&format!("`{PROJECT_IMAGE_LAYER_PATH}/` at no-such-branch")), "{error}");
    assert!(error.contains("refs/heads/no-such-branch"), "the fetch failure is reported: {error}");
    assert_eq!(build_names(&backend).await, builds, "a missing ref creates no build");
}
