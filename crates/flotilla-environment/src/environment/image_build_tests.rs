use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_resources::{
    image_identity, HostImageAction, ImageBuildFailureClass, ImageContextFile, ImageLayerProbe, ImageLayerSource, ImageLayerSpec,
    ImageLayerTier, ImageProbeOutcome,
};
use hegel::generators as gs;

use super::{
    docker_image::{image_tag, OwnedBuildKitService, PROBE_TIMEOUT},
    image::{FailedImageBuild, ImageBuildRequest, ImageBuildService, ImageDeliveryTarget, OwnedBuilderSettings, SecretFile},
    image_dockerfile::generate_build,
    registry_auth::{RegistryAuthMaterial, RegistryConfig},
    RegistryAuth,
};

const REPOSITORY: &str = "registry.example/flotilla/crew-images";
const CACHE: &str = "registry.example/flotilla/crew-images/buildcache";
const WRAPPER: &str = "flotilla_run_command() {\nexec \"$@\"\n}\nflotilla_run_with_preludes flotilla_run_command \"$@\"";
const SECRET: &str = "s3cret-token";

fn digest(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
}

fn layer(name: &str, tier: ImageLayerTier, probe: &str) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0".into())
        .tier(tier)
        .source(ImageLayerSource::Stdlib)
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu:24.04@{}", digest('1'))))
        .fragment("FROM ${BASE}\nRUN true\n".into())
        .probe(probe.into())
        .build()
}

fn layers() -> Vec<ImageLayerSpec> {
    vec![
        layer("base", ImageLayerTier::Base, "probe base"),
        layer("rust", ImageLayerTier::Shared, "probe rust"),
        layer("harness-codex", ImageLayerTier::AgentRuntime, "probe codex"),
    ]
    .into_iter()
    .map(|mut layer| {
        if layer.name == "harness-codex" {
            layer.provides.insert("harness:codex".into());
        }
        layer
    })
    .collect()
}

#[derive(Clone)]
enum Step {
    /// Exit status and stderr.
    Exit(Option<i32>, String),
    /// Never completes; the caller's deadline applies.
    Hang,
}

/// Credential-store stand-in: a private configuration holding the secret,
/// redacted from every diagnostic.
struct PushMaterial;
impl RegistryAuthMaterial for PushMaterial {
    fn config(&self) -> Result<RegistryConfig, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        std::fs::write(directory.path().join("config.json"), format!("{{\"auths\":{{\"registry.example\":\"{SECRET}\"}}}}"))
            .map_err(|error| error.to_string())?;
        Ok(RegistryConfig::new(directory.path().to_path_buf(), directory))
    }
    fn redact(&self, text: String) -> String {
        text.replace(SECRET, "[redacted]")
    }
    fn authorize(&self, _: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        panic!("builds do not use HTTP registry calls")
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Mode {
    Push,
    Load,
}

// Subprocess boundary stand-in for the Docker CLI. It enforces the documented
// owned-BuildKit argv contract exactly (flag order included), checks the
// materialised Dockerfile and contexts on disk, and models Buildx's metadata
// file, `docker pull`, `image inspect`, `run` and `rm --force`.
struct BuildKitStandIn {
    mode: Mode,
    architecture: String,
    layers: Vec<ImageLayerSpec>,
    secrets: BTreeMap<String, PathBuf>,
    scratch: PathBuf,
    state: PathBuf,
    build: Step,
    metadata_digest: String,
    probes: BTreeMap<String, Step>,
    calls: Mutex<Vec<Vec<String>>>,
    workspace: Mutex<Option<PathBuf>>,
    config: Mutex<Option<PathBuf>>,
    hung_probe: Mutex<Option<String>>,
}

impl BuildKitStandIn {
    fn new(mode: Mode, layers: Vec<ImageLayerSpec>, scratch: &Path) -> Self {
        let mut probes = BTreeMap::new();
        for layer in &layers {
            probes.insert(layer.probe.clone(), Step::Exit(Some(0), String::new()));
        }
        Self {
            mode,
            architecture: "amd64".into(),
            layers,
            secrets: BTreeMap::new(),
            scratch: scratch.join("scratch"),
            state: scratch.join("buildx-state"),
            build: Step::Exit(Some(0), String::new()),
            metadata_digest: digest('9'),
            probes,
            calls: Mutex::new(Vec::new()),
            workspace: Mutex::new(None),
            config: Mutex::new(None),
            hung_probe: Mutex::new(None),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls").clone()
    }

    fn tag(&self) -> String {
        image_tag(&image_identity(&self.layers), &self.architecture)
    }

    fn check_build(&self, args: &[&str]) -> PathBuf {
        let file = args.windows(2).find(|pair| pair[0] == "--file").expect("--file")[1];
        let workspace = Path::new(file).parent().expect("workspace").to_path_buf();
        assert_eq!(workspace.parent(), Some(self.scratch.as_path()), "builds materialise under the injected scratch directory");
        assert!(workspace.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("flotilla-image-build-")));
        let at = |relative: &str| workspace.join(relative).to_str().expect("UTF-8").to_string();
        let generated = generate_build(&self.layers, &self.architecture).expect("generate");
        let mut expected = vec![
            "buildx".to_string(),
            "build".into(),
            "--builder".into(),
            "owned-buildkit".into(),
            "--platform".into(),
            format!("linux/{}", self.architecture),
            "--file".into(),
            at("Dockerfile"),
        ];
        for context in &generated.contexts {
            expected.extend(["--build-context".to_string(), format!("{}={}", context.name, at(&format!("contexts/{}", context.name)))]);
            for (relative, file) in &context.files {
                let path = workspace.join("contexts").join(&context.name).join(relative);
                assert_eq!(std::fs::read_to_string(&path).expect("context file"), file.content);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
                    assert_eq!(mode, if file.executable { 0o755 } else { 0o644 }, "executable bits are materialised");
                }
            }
        }
        for name in &generated.secrets {
            expected.extend(["--secret".to_string(), format!("id={name},src={}", self.secrets[name].display())]);
        }
        expected.extend(["--provenance=false".to_string(), "--metadata-file".into(), at("metadata.json"), "--progress=plain".into()]);
        match self.mode {
            Mode::Push => expected.extend([
                "--push".to_string(),
                "--tag".into(),
                format!("{REPOSITORY}:{}", self.tag()),
                "--cache-to".into(),
                format!("type=registry,ref={CACHE},mode=max"),
                "--cache-from".into(),
                format!("type=registry,ref={CACHE}"),
            ]),
            Mode::Load => expected.extend(["--load".to_string(), "--tag".into(), format!("flotilla-image:{}", self.tag())]),
        }
        expected.push(at("root"));
        assert_eq!(args, expected, "buildx argv contract");
        assert_eq!(std::fs::read_to_string(workspace.join("Dockerfile")).expect("Dockerfile"), generated.dockerfile);
        assert_eq!(std::fs::read_dir(workspace.join("root")).expect("root context").count(), 0, "the main context is empty");
        workspace
    }

    fn check_probe(&self, args: &[&str]) -> String {
        let image = match self.mode {
            Mode::Push => format!("{REPOSITORY}@{}", self.metadata_digest),
            Mode::Load => digest('7'),
        };
        let name = args[3];
        assert!(name.starts_with("flotilla-probe-"), "probe containers are named for cleanup");
        let probe = *args.last().expect("probe");
        assert_eq!(
            args,
            [
                "run",
                "--rm",
                "--name",
                name,
                "--network=none",
                "--user",
                "65534:65534",
                "-e",
                "HOME=/tmp/flotilla-probe-home",
                "--tmpfs",
                "/tmp:exec,mode=1777",
                "--tmpfs",
                "/tmp/flotilla-probe-home:exec,uid=65534,gid=65534,mode=0700",
                "--pull=never",
                "--cpus",
                "2",
                "--memory",
                "512m",
                "--pids-limit",
                "256",
                "--entrypoint",
                "sh",
                image.as_str(),
                "-c",
                WRAPPER,
                "flotilla-layer-probe",
                "sh",
                "-c",
                probe,
            ],
            "probe argv contract: unprivileged, offline, through the prelude wrapper"
        );
        probe.to_string()
    }
}

fn output(step: &Step) -> CommandOutput {
    match step {
        Step::Exit(code, stderr) => CommandOutput { stdout: "progress\n".into(), stderr: stderr.clone(), exit_code: *code },
        Step::Hang => unreachable!("hangs are awaited, not returned"),
    }
}

#[async_trait]
impl CommandRunner for BuildKitStandIn {
    async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        panic!("the build service preserves exit status through run_output")
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        panic!("the build service does not discover endpoints")
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "docker");
        assert_eq!(cwd, Path::new("/"));
        self.calls.lock().expect("calls").push(args.iter().map(|arg| arg.to_string()).collect());
        assert_eq!(args[0], "--config", "every operation uses the private configuration");
        let config = PathBuf::from(args[1]);
        let previous = self.config.lock().expect("config").replace(config.clone());
        assert!(previous.is_none_or(|previous| previous == config), "one configuration for the whole operation");
        assert_eq!(std::fs::read_link(config.join("buildx")).expect("builder state link"), self.state);
        assert_eq!(config.join("config.json").exists(), self.mode == Mode::Push, "only push mode carries the credential");
        let rest = &args[2..];
        let step = match rest {
            ["buildx", "build", ..] => {
                let workspace = self.check_build(rest);
                *self.workspace.lock().expect("workspace") = Some(workspace.clone());
                if matches!(self.build, Step::Exit(Some(0), _)) && self.mode == Mode::Push {
                    std::fs::write(
                        workspace.join("metadata.json"),
                        serde_json::json!({"containerimage.digest": self.metadata_digest}).to_string(),
                    )
                    .expect("metadata");
                }
                self.build.clone()
            }
            ["pull", reference] => {
                assert_eq!(self.mode, Mode::Push);
                assert_eq!(*reference, format!("{REPOSITORY}@{}", self.metadata_digest), "probes run on the pushed digest");
                Step::Exit(Some(0), String::new())
            }
            ["image", "inspect", "--format", "{{.Id}}", tag] => {
                assert_eq!(self.mode, Mode::Load);
                assert_eq!(*tag, format!("flotilla-image:{}", self.tag()));
                return Ok(CommandOutput { stdout: format!("{}\n", digest('7')), stderr: String::new(), exit_code: Some(0) });
            }
            ["run", ..] => {
                let probe = self.check_probe(rest);
                let step = self.probes[&probe].clone();
                if matches!(step, Step::Hang) {
                    *self.hung_probe.lock().expect("hung") = Some(rest[3].to_string());
                }
                step
            }
            ["rm", "--force", name] => {
                assert_eq!(self.hung_probe.lock().expect("hung").as_deref(), Some(*name), "only the timed-out probe is removed");
                Step::Exit(Some(0), String::new())
            }
            other => panic!("unexpected Docker invocation: {other:?}"),
        };
        match step {
            Step::Hang => std::future::pending().await,
            step => Ok(output(&step)),
        }
    }
}

fn settings(runner: &BuildKitStandIn) -> OwnedBuilderSettings {
    OwnedBuilderSettings { builder: "owned-buildkit".into(), state_directory: runner.state.clone(), scratch: runner.scratch.clone() }
}

fn delivery(mode: &Mode) -> ImageDeliveryTarget {
    match mode {
        Mode::Push => ImageDeliveryTarget::Push {
            repository: REPOSITORY.into(),
            cache_repository: CACHE.into(),
            auth: RegistryAuth::new(REPOSITORY.into(), HostImageAction::ImagePush, Arc::new(PushMaterial)),
        },
        Mode::Load => ImageDeliveryTarget::Load,
    }
}

async fn build(
    runner: &Arc<BuildKitStandIn>,
    secrets: BTreeMap<String, SecretFile>,
    log: &mut String,
) -> Result<super::image::BuiltImage, FailedImageBuild> {
    let service = OwnedBuildKitService::new(runner.clone(), settings(runner));
    let identity = image_identity(&runner.layers);
    service
        .build(
            ImageBuildRequest::builder()
                .identity(&identity)
                .architecture(&runner.architecture)
                .layers(&runner.layers)
                .delivery(delivery(&runner.mode))
                .secrets(secrets)
                .probe_wrapper(WRAPPER)
                .probe_cpus(2)
                .build(),
            log,
        )
        .await
}

fn outcomes(probes: &[ImageLayerProbe]) -> Vec<(&str, &ImageProbeOutcome)> {
    probes.iter().map(|probe| (probe.layer.as_str(), &probe.outcome)).collect()
}

fn assert_cleaned(runner: &BuildKitStandIn) {
    if let Some(workspace) = runner.workspace.lock().expect("workspace").as_ref() {
        assert!(!workspace.exists(), "the per-build directory is removed");
    }
    if let Some(config) = runner.config.lock().expect("config").as_ref() {
        assert!(!config.exists(), "the private configuration is removed");
    }
}

// Both delivery modes build exactly once through the owned builder with the
// documented argv, probe every layer in order on the delivered image, report
// the delivered digest, and leave no build directory or configuration behind.
// Generated span: push and load, both architectures, a context layer with or
// without executable files, and a declared build secret or none.
#[hegel::test]
fn delivery_modes_build_probe_and_clean_up(tc: hegel::TestCase) {
    let mode = if tc.draw(gs::booleans()) { Mode::Push } else { Mode::Load };
    let arm = tc.draw(gs::booleans());
    let context = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let secret = tc.draw(gs::booleans());
    let directory = tempfile::tempdir().expect("directory");
    let mut layers = layers();
    if context > 0 {
        layers[1]
            .context_files
            .insert("prelude.d/40-rust.sh".into(), ImageContextFile { content: "echo rust\n".into(), executable: false });
        layers[1].context_files.insert("bin/tool".into(), ImageContextFile { content: "#!/bin/sh\n".into(), executable: context == 2 });
    }
    let secret_path = directory.path().join("npm-token");
    std::fs::write(&secret_path, "token").expect("secret");
    if secret {
        layers[2].build_secrets.insert("npm-token".into());
    }
    let mut runner = BuildKitStandIn::new(mode.clone(), layers, directory.path());
    if arm {
        runner.architecture = "arm64".into();
    }
    // Extra grants are never passed to the builder.
    let secrets = BTreeMap::from([
        ("npm-token".to_string(), SecretFile::at(secret_path.clone())),
        ("unused".to_string(), SecretFile::at(directory.path().join("unused"))),
    ]);
    runner.secrets.insert("npm-token".into(), secret_path);
    let runner = Arc::new(runner);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let mut log = String::new();
    let built = runtime.block_on(build(&runner, secrets, &mut log)).expect("build");
    assert_eq!(
        built.digest,
        match mode {
            Mode::Push => format!("{REPOSITORY}@{}", digest('9')),
            Mode::Load => digest('7'),
        }
    );
    assert_eq!(
        outcomes(&built.probes),
        [("base", &ImageProbeOutcome::Passed), ("rust", &ImageProbeOutcome::Passed), ("harness-codex", &ImageProbeOutcome::Passed)]
    );
    let verbs: Vec<_> = runner.calls().iter().map(|call| call[2].clone()).collect();
    let expected: Vec<&str> = match mode {
        Mode::Push => vec!["buildx", "pull", "run", "run", "run"],
        Mode::Load => vec!["buildx", "image", "run", "run", "run"],
    };
    assert_eq!(verbs, expected);
    let probes: Vec<_> = runner.calls().iter().filter(|call| call[2] == "run").map(|call| call.last().expect("probe").clone()).collect();
    assert_eq!(probes, ["probe base", "probe rust", "probe codex"], "every layer's probe runs in layer order");
    assert!(log.contains("progress"));
    assert_cleaned(&runner);
}

// A failing probe fails the build as a recipe failure, but every layer's
// probe still runs and is reported.
#[tokio::test]
async fn probe_failure_is_recipe_with_every_layer_reported() {
    let directory = tempfile::tempdir().expect("directory");
    let mut runner = BuildKitStandIn::new(Mode::Push, layers(), directory.path());
    runner.probes.insert("probe base".into(), Step::Exit(Some(1), "missing tool curl\n".into()));
    runner.probes.insert("probe rust".into(), Step::Exit(Some(127), "rustc: not found\n".into()));
    let runner = Arc::new(runner);
    let failed = build(&runner, BTreeMap::new(), &mut String::new()).await.expect_err("probe failure");
    assert_eq!(failed.failure.class, ImageBuildFailureClass::Recipe);
    assert!(failed.failure.reason.contains("base") && failed.failure.reason.contains("rust"));
    assert_eq!(
        outcomes(&failed.probes),
        [
            ("base", &ImageProbeOutcome::Failed { reason: "missing tool curl".into() }),
            ("rust", &ImageProbeOutcome::Failed { reason: "rustc: not found".into() }),
            ("harness-codex", &ImageProbeOutcome::Passed),
        ]
    );
    assert_cleaned(&runner);
}

// BuildKit step failures with an exit code are recipe failures; transport,
// registry, daemon, disk and kill evidence is infrastructure. No probe runs
// after a failed build, and push-credential material never reaches the
// reason or the log.
#[tokio::test]
async fn build_failures_are_classified_and_redacted() {
    let cases = [
        (
            Some(1),
            "ERROR: failed to solve: process \"/bin/sh -c make\" did not complete successfully: exit code: 2",
            ImageBuildFailureClass::Recipe,
        ),
        (Some(1), "ERROR: failed to solve: dial tcp: lookup registry.example: no such host", ImageBuildFailureClass::Infrastructure),
        (Some(1), "ERROR: failed to push: connection reset by peer", ImageBuildFailureClass::Infrastructure),
        (Some(1), "ERROR: write /var/lib/buildkit: no space left on device", ImageBuildFailureClass::Infrastructure),
        (Some(1), "Cannot connect to the Docker daemon at unix:///var/run/docker.sock", ImageBuildFailureClass::Infrastructure),
        (Some(137), "killed", ImageBuildFailureClass::Infrastructure),
        (None, "", ImageBuildFailureClass::Infrastructure),
        (Some(1), "ERROR: dockerfile parse error on line 3: unknown instruction: RUNN", ImageBuildFailureClass::Recipe),
    ];
    for (exit, stderr, class) in cases {
        let directory = tempfile::tempdir().expect("directory");
        let mut runner = BuildKitStandIn::new(Mode::Push, layers(), directory.path());
        runner.build = Step::Exit(exit, format!("#5 auth {SECRET}\n{stderr} {SECRET}\n"));
        let runner = Arc::new(runner);
        let mut log = String::new();
        let failed = build(&runner, BTreeMap::new(), &mut log).await.expect_err("build failure");
        assert_eq!(failed.failure.class, class, "{stderr}");
        assert!(!failed.failure.reason.contains(SECRET) && !log.contains(SECRET), "credential material is redacted");
        assert!(failed.probes.iter().all(|probe| probe.outcome == ImageProbeOutcome::NotRun));
        assert_eq!(runner.calls().len(), 1, "nothing runs after a failed build");
        assert_cleaned(&runner);
    }
}

// Deadlines are infrastructure failures. A timed-out probe's container is
// force-removed by name, later probes are not run, and the directory is cleaned.
#[tokio::test(start_paused = true)]
async fn timeouts_are_infrastructure_and_remove_probe_containers() {
    let directory = tempfile::tempdir().expect("directory");
    let mut runner = BuildKitStandIn::new(Mode::Load, layers(), directory.path());
    runner.build = Step::Hang;
    let runner = Arc::new(runner);
    let failed = build(&runner, BTreeMap::new(), &mut String::new()).await.expect_err("build timeout");
    assert_eq!(failed.failure.class, ImageBuildFailureClass::Infrastructure);
    assert!(failed.failure.reason.contains("deadline"));
    assert_cleaned(&runner);

    let mut runner = BuildKitStandIn::new(Mode::Load, layers(), directory.path());
    runner.probes.insert("probe rust".into(), Step::Hang);
    let runner = Arc::new(runner);
    let started = tokio::time::Instant::now();
    let failed = build(&runner, BTreeMap::new(), &mut String::new()).await.expect_err("probe timeout");
    assert!(started.elapsed() >= PROBE_TIMEOUT);
    assert_eq!(failed.failure.class, ImageBuildFailureClass::Infrastructure);
    assert_eq!(
        outcomes(&failed.probes),
        [
            ("base", &ImageProbeOutcome::Passed),
            ("rust", &ImageProbeOutcome::Failed { reason: "probe exceeded its 60 second deadline".into() }),
            ("harness-codex", &ImageProbeOutcome::NotRun),
        ]
    );
    let last = runner.calls().last().expect("cleanup").clone();
    assert_eq!(last[2..4], ["rm", "--force"]);
    assert_cleaned(&runner);
}

// Inputs that cannot change without new definitions or grants are refused as
// recipe failures before any subprocess: an ungranted secret (named), a
// mismatched identity, and a push credential for another repository.
#[tokio::test]
async fn refusals_before_building() {
    let directory = tempfile::tempdir().expect("directory");
    let mut secret_layers = layers();
    secret_layers[1].build_secrets.insert("forgejo-token".into());
    let runner = Arc::new(BuildKitStandIn::new(Mode::Push, secret_layers, directory.path()));
    let failed = build(&runner, BTreeMap::new(), &mut String::new()).await.expect_err("ungranted");
    assert_eq!(failed.failure.class, ImageBuildFailureClass::Recipe);
    assert!(failed.failure.reason.contains("`forgejo-token`"));

    let service = OwnedBuildKitService::new(runner.clone(), settings(&runner));
    let request = |identity, delivery| {
        ImageBuildRequest::builder()
            .identity(identity)
            .architecture("amd64")
            .layers(&runner.layers)
            .delivery(delivery)
            .probe_wrapper(WRAPPER)
            .probe_cpus(1)
            .build()
    };
    let wrong = digest('0');
    let failed = service.build(request(&wrong, ImageDeliveryTarget::Load), &mut String::new()).await.expect_err("identity");
    assert!(failed.failure.reason.contains("identity"));
    let identity = image_identity(&runner.layers);
    let foreign = ImageDeliveryTarget::Push {
        repository: "registry.example/other".into(),
        cache_repository: CACHE.into(),
        auth: RegistryAuth::new(REPOSITORY.into(), HostImageAction::ImagePush, Arc::new(PushMaterial)),
    };
    let failed = service.build(request(&identity, foreign), &mut String::new()).await.expect_err("foreign repository");
    assert_eq!(failed.failure.class, ImageBuildFailureClass::Recipe);
    assert!(runner.calls().is_empty(), "nothing runs for refused inputs");
}

// Buildx metadata without a valid pushed digest is never admitted as a
// build result; it is an infrastructure failure.
#[tokio::test]
async fn invalid_push_metadata_is_infrastructure() {
    let directory = tempfile::tempdir().expect("directory");
    let mut runner = BuildKitStandIn::new(Mode::Push, layers(), directory.path());
    runner.metadata_digest = "sha256:short".into();
    let runner = Arc::new(runner);
    let failed = build(&runner, BTreeMap::new(), &mut String::new()).await.expect_err("metadata");
    assert_eq!(failed.failure.class, ImageBuildFailureClass::Infrastructure);
    assert!(failed.failure.reason.contains("digest"));
    assert_cleaned(&runner);
}
