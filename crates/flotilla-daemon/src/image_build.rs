//! Host-local image build coordinator. Lifecycle policy lives in the controller and
//! source acquisition stays behind the checkout-scoped VCS seam.
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{ImageBuildResult, ImageBuildRunner};
use flotilla_core::{
    image_build::{ImageBuildInputResolver, ImageBuildSourceInputs},
    providers::{
        container::{ContainerProbe, ImageBuildOptions, ImageOperation, ImageStore, ProbeNetwork},
        environment::ContainerRuntime,
    },
    vcs::Vcs,
};
use flotilla_resources::{
    Artifact, ArtifactSpec, FrozenImageLayer, ImageBuildFailure, ImageBuildFailureClass, ImageBuildSpec, ImageLayerParent, InputMeta,
    PlacedImageIdentity, ResourceBackend,
};
use sha2::{Digest, Sha256};

use crate::blob_store::{BlobStore, TieredBlobStore};

#[derive(bon::Builder)]
pub(crate) struct HostImageBuildRunner {
    pub distributor: Option<Arc<crate::image_distribution::ImageDistributor<crate::image_distribution::ProviderImageIo>>>,
    pub images: Arc<dyn ImageStore>,
    pub runtime: Option<Arc<dyn ContainerRuntime>>,
    pub vcs: Arc<dyn Vcs>,
    pub directory: PathBuf,
    pub backend: ResourceBackend,
    pub namespace: String,
    pub blobs: Arc<TieredBlobStore>,
}

fn hash_context(directory: &Path) -> Result<Vec<String>, String> {
    fn visit(root: &Path, path: &Path, hashes: &mut Vec<String>) -> Result<(), String> {
        let mut entries = std::fs::read_dir(path)
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if meta.is_dir() {
                visit(root, &path, hashes)?;
                continue;
            }
            let relative = path.strip_prefix(root).map_err(|error| error.to_string())?.to_str().ok_or("image context path is not UTF-8")?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0_u32;
            let mut hash = Sha256::new();
            hash.update(relative.as_bytes());
            hash.update([0]);
            hash.update(mode.to_be_bytes());
            hash.update([u8::from(meta.is_symlink())]);
            if meta.is_symlink() {
                let target = std::fs::read_link(&path).map_err(|error| error.to_string())?;
                hash.update(target.to_str().ok_or("image symlink is not UTF-8")?.as_bytes());
            } else {
                let mut file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let length = file.read(&mut buffer).map_err(|error| error.to_string())?;
                    if length == 0 {
                        break;
                    }
                    hash.update(&buffer[..length]);
                }
            }
            hashes.push(format!("sha256:{:x}", hash.finalize()));
        }
        Ok(())
    }
    let mut hashes = Vec::new();
    visit(directory, directory, &mut hashes)?;
    Ok(hashes)
}

fn hash_layer_inputs_blocking(context: &Path, layer: &FrozenImageLayer) -> Result<Vec<String>, String> {
    let mut hashes = hash_context(context)?;
    let verification = serde_json::to_vec(&(
        &layer.spec.fragment,
        &layer.spec.provides,
        &layer.spec.requires,
        &layer.spec.probes,
        flotilla_core::agent_process::with_preludes("exec \"$@\""),
    ))
    .map_err(|error| error.to_string())?;
    hashes.push(format!("sha256:{:x}", Sha256::digest(verification)));
    Ok(hashes)
}

const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REASON_CHARS: usize = 2048;

async fn hash_layer_inputs(context: &Path, layer: &FrozenImageLayer) -> Result<Vec<String>, String> {
    let context = context.to_path_buf();
    let layer = layer.clone();
    tokio::task::spawn_blocking(move || hash_layer_inputs_blocking(&context, &layer)).await.map_err(|error| error.to_string())?
}

// Status holds a bounded diagnostic, while the Artifact retains the full output.
// BuildKit's final ERROR summary follows the progress stream. Never classify
// arbitrary Dockerfile/probe output as infrastructure evidence.
fn failure_summary(stderr: &str, exit_code: Option<i32>) -> String {
    let tail = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let summary = if tail.is_empty() { format!("Docker process exited with {exit_code:?}") } else { tail.to_string() };
    summary.chars().take(MAX_REASON_CHARS).collect()
}

fn classify_build_failure(exit_code: Option<i32>, reason: &str) -> ImageBuildFailureClass {
    let reason = failure_summary(reason, exit_code).to_ascii_lowercase();
    if exit_code.is_some() && exit_code != Some(137) && reason.contains("did not complete successfully") && reason.contains("exit code:") {
        return ImageBuildFailureClass::Deterministic;
    }
    let infrastructure = [
        "connection reset",
        "connection refused",
        "no space left on device",
        "temporary failure in name resolution",
        "no such host",
        "tls handshake timeout",
        "cannot connect to the docker daemon",
        "network is unreachable",
        "service unavailable",
        "code = unavailable",
        "unexpected eof",
        "i/o timeout",
        "context deadline exceeded",
        "http 429",
        "status code: 429",
        "429 too many requests",
    ];
    if exit_code.is_none() || exit_code == Some(137) || infrastructure.iter().any(|signal| reason.contains(signal)) {
        ImageBuildFailureClass::Transient
    } else {
        ImageBuildFailureClass::Deterministic
    }
}

impl HostImageBuildRunner {
    async fn context(&self, layer: &FrozenImageLayer) -> Result<PathBuf, String> {
        let directory =
            self.directory.join(format!("source-{:x}", Sha256::digest(format!("{}\0{}", layer.spec.repository, layer.spec.revision))));
        let lock = flotilla_core::charter_store::reconciliation_lock(&directory.to_string_lossy());
        let _guard = lock.lock().await;
        if !tokio::fs::try_exists(directory.join("complete")).await.map_err(|error| error.to_string())? {
            let _ = tokio::fs::remove_dir_all(directory.join("context")).await;
            self.vcs.image_build_context(&directory, &layer.spec.repository, &layer.spec.revision).await?;
            tokio::fs::write(directory.join("complete"), b"complete").await.map_err(|error| error.to_string())?;
        }
        let context = directory.join("context");
        let fragment = context.join(&layer.spec.fragment);
        let canonical = tokio::fs::canonicalize(fragment).await.map_err(|error| error.to_string())?;
        if !canonical.starts_with(tokio::fs::canonicalize(&context).await.map_err(|error| error.to_string())?) {
            return Err("Dockerfile fragment escapes immutable build context".into());
        }
        Ok(context)
    }

    async fn execute(
        &self,
        name: &str,
        spec: &ImageBuildSpec,
        parent_digest: &str,
        log: &mut String,
    ) -> Result<(PlacedImageIdentity, BTreeSet<String>), ImageBuildFailure> {
        let deterministic = |reason| ImageBuildFailure { class: ImageBuildFailureClass::Deterministic, reason };
        let transient = |reason| ImageBuildFailure { class: ImageBuildFailureClass::Transient, reason };
        if let (Some(distributor), Some(parent)) = (&self.distributor, &spec.parent_build_ref) {
            let mut parent = flotilla_resources::read_image_build(&self.backend, &self.namespace, parent)
                .await
                .map_err(|error| transient(error.to_string()))?;
            for _ in 0..3 {
                match flotilla_resources::read_image_build(&self.backend, &self.namespace, &format!("{}-retry", parent.metadata.name)).await
                {
                    Ok(next) => parent = next,
                    Err(flotilla_resources::ResourceError::NotFound { .. }) => break,
                    Err(error) => return Err(transient(error.to_string())),
                }
            }
            distributor.ensure(&parent).await.map_err(transient)?;
        }
        let context = self.context(&spec.layer).await.map_err(transient)?;
        let hashes = hash_layer_inputs(&context, &spec.layer).await.map_err(deterministic)?;
        if hashes != spec.inputs.content_hashes {
            return Err(deterministic("image context differs from frozen input hashes".into()));
        }
        let config = self.directory.join(format!("config-{:x}", Sha256::digest(name)));
        tokio::fs::create_dir_all(&config).await.map_err(|error| transient(error.to_string()))?;
        let architecture = match spec.inputs.architecture.as_str() {
            "x86_64" | "amd64" => "amd64",
            "aarch64" | "arm64" => "arm64",
            other => other,
        };
        let tag = format!("flotilla-build:{:x}", Sha256::digest(format!("{name}\0{}", spec.recipe_key)));
        let operation = ImageOperation { directory: &config, context: &context };
        let base = match &spec.layer.spec.parent {
            ImageLayerParent::Image(image) => image.clone(),
            _ => {
                // Buildx's default Docker driver consumes images from the host
                // store. Give the exact local ID a content-derived label for FROM;
                // record the ID, never this label, as the parent identity.
                let label = format!("flotilla-parent:{:x}", Sha256::digest(parent_digest));
                self.images.tag(operation, parent_digest, &label).await.map_err(transient)?;
                label
            }
        };
        let mut build_args = spec.layer.spec.args.clone();
        build_args.extend(spec.layer.spec.pins.iter().map(|(name, pin)| (name.clone(), pin.value.clone())));
        build_args.insert("BASE".into(), base);
        if self.images.inspect(operation, &tag).await.is_err() {
            let platform = format!("linux/{architecture}");
            let output = self
                .images
                .build(
                    operation,
                    ImageBuildOptions::builder()
                        .tag(&tag)
                        .file(&spec.layer.spec.fragment)
                        .platform(&platform)
                        .args(&build_args)
                        .timeout(BUILD_TIMEOUT)
                        .build(),
                )
                .await
                .map_err(transient)?;
            log.push_str(&output.stdout);
            log.push_str(&output.stderr);
            if !output.success() {
                let reason = failure_summary(&output.stderr, output.exit_code);
                let class = classify_build_failure(output.exit_code, &reason);
                return Err(ImageBuildFailure { class, reason });
            }
        }
        let identity = self.images.inspect(operation, &tag).await.map_err(transient)?;
        identity.validate().map_err(deterministic)?;
        let mut verified = BTreeSet::new();
        for provide in &spec.layer.spec.provides {
            let command = spec
                .layer
                .spec
                .probes
                .get(provide)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| deterministic(format!("no verification probe declared for {provide}")))?;
            let probe_name = format!("flotilla-probe-{:x}", Sha256::digest(format!("{name}\0{provide}")));
            // Positional arguments preserve arbitrary probe argv without shell
            // interpolation. Preludes and the probe share their export scope.
            let script = flotilla_core::agent_process::with_preludes("exec \"$@\"");
            let mut argv = vec!["sh".into(), "-c".into(), script, "flotilla-provide-probe".into()];
            argv.extend(command.iter().cloned());
            let output = self
                .runtime
                .as_ref()
                .ok_or_else(|| transient("container runtime unavailable".into()))?
                .probe(
                    operation,
                    ContainerProbe::builder()
                        .image(&identity.local_image_id)
                        .name(&probe_name)
                        .command(&argv)
                        .cpu(spec.reservation.cpu)
                        .memory_bytes(512 * 1024 * 1024)
                        .pids_limit(128)
                        .network(ProbeNetwork::Isolated)
                        .timeout(PROBE_TIMEOUT)
                        .build(),
                )
                .await
                .map_err(transient)?;
            log.push_str(&output.stdout);
            log.push_str(&output.stderr);
            if !output.success() {
                let reason = failure_summary(&output.stderr, output.exit_code);
                // 125 is Docker's own failure, 126/127 are an unusable probe
                // command, and other nonzero codes belong to the probe itself.
                let class = if output.exit_code == Some(125) || output.exit_code.is_none() || output.exit_code == Some(137) {
                    classify_build_failure(output.exit_code, &reason)
                } else {
                    ImageBuildFailureClass::Deterministic
                };
                return Err(ImageBuildFailure {
                    class,
                    reason: format!("provide verification failed: {provide}: {reason}").chars().take(MAX_REASON_CHARS).collect(),
                });
            }
            verified.insert(provide.clone());
        }
        Ok((identity, verified))
    }
}

#[async_trait]
impl ImageBuildInputResolver for HostImageBuildRunner {
    async fn resolve(&self, layer: &FrozenImageLayer, architecture: &str) -> Result<ImageBuildSourceInputs, String> {
        let context = self.context(layer).await?;
        Ok(ImageBuildSourceInputs::builder()
            .content_hashes(hash_layer_inputs(&context, layer).await?)
            .architecture(architecture.into())
            .args(layer.spec.args.clone())
            .pins(layer.spec.pins.iter().map(|(name, pin)| (name.clone(), pin.value.clone())).collect::<BTreeMap<_, _>>())
            .stability(layer.spec.input_stability)
            .build())
    }
}

#[async_trait]
impl ImageBuildRunner for HostImageBuildRunner {
    async fn build(&self, name: &str, spec: &ImageBuildSpec, parent_digest: &str) -> Result<ImageBuildResult, String> {
        let mut log = String::new();
        let mut result = self.execute(name, spec, parent_digest, &mut log).await;
        let _ = tokio::fs::remove_dir_all(self.directory.join(format!("config-{:x}", Sha256::digest(name)))).await;
        if let Err(failure) = &mut result {
            log.push_str(&format!("\n{}\n", failure.reason));
            failure.reason = failure.reason.chars().take(MAX_REASON_CHARS).collect();
        }
        let artifact_name = format!("image-build-log-{name}");
        let saved: Result<(), String> = async {
            let digest = self.blobs.put_with_media_type(log.as_bytes(), "text/plain").await?;
            let artifact = ArtifactSpec::builder()
                .convoy(String::new())
                .producer("image-build".into())
                .kind("build-log".into())
                .subject(name.into())
                .digest(digest.as_str().into())
                .size(log.len() as u64)
                .media_type("text/plain".into())
                .recorded_at(Utc::now())
                .expires_at(Utc::now() + chrono::Duration::days(30))
                .build();
            let artifacts = self.backend.using::<Artifact>(&self.namespace);
            match artifacts.create(&InputMeta::builder().name(artifact_name.clone()).build(), &artifact).await {
                Ok(_) => Ok(()),
                Err(flotilla_resources::ResourceError::Conflict { .. }) => {
                    tracing::debug!(artifact = %artifact_name, "preserving original execution log during recovery");
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            }
        }
        .await;
        saved?;
        Ok(match result {
            Ok((identity, verified_provides)) => ImageBuildResult::Built { identity, verified_provides, log_ref: artifact_name },
            Err(failure) => ImageBuildResult::Failed { failure, log_ref: artifact_name },
        })
    }
}

/// Mirror build demands onto their actuator host, following Vessel placement's
/// projection convention. Execution evidence is replicated back to consumers.
pub(crate) async fn project_builds(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
) -> Result<(), flotilla_resources::ResourceError> {
    use flotilla_resources::{ImageBuild, ResourceProvenance, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION};
    let builds = backend.using::<ImageBuild>(namespace);
    for source in backend.including_replicas::<ImageBuild>(namespace).list().await?.items {
        let ResourceProvenance::Replica { origin_root, .. } = source.provenance else {
            continue;
        };
        let obj = source.object;
        if obj.spec.host_ref != host_ref
            || obj.metadata.deletion_timestamp.is_some()
            || obj.metadata.annotations.contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION)
        {
            continue;
        }
        match builds.get(&obj.metadata.name).await {
            Ok(current) => {
                if current.spec.inputs != obj.spec.inputs || current.spec.recipe_key != obj.spec.recipe_key {
                    tracing::error!(role = "infra", image_build = %obj.metadata.name, "image build execution name collision; skipping demand");
                    continue;
                }
            }
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let mut annotations = obj.metadata.annotations;
                annotations.insert(ACTUATOR_HOST_REF_ANNOTATION.into(), host_ref.into());
                annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.into(), origin_root.to_string());
                builds.create(&InputMeta::builder().name(obj.metadata.name).annotations(annotations).build(), &obj.spec).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::NodeId;
    use flotilla_resources::{
        apply_status_patch, read_image_build, ImageBuild, ImageBuildPhase, ImageBuildReason, ImageBuildReservation, ImageBuildStatusPatch,
        ImageInputStability, ImageLayerSpec, ImageLayerStage, ResolvedImageInputs,
    };

    use super::*;

    // A declared builder projects a remote demand only once, writes evidence
    // at its own authority, and replication returns that evidence to admission.
    #[tokio::test]
    async fn declared_builder_projects_demand_and_returns_execution_evidence() {
        let home = ResourceBackend::InMemory(Default::default()).with_local_root(NodeId::new("home"));
        let builder = ResourceBackend::InMemory(Default::default()).with_local_root(NodeId::new("builder-root"));
        let inputs = ResolvedImageInputs::builder()
            .parent_digest(format!("sha256:{}", "1".repeat(64)))
            .content_hashes(vec![format!("sha256:{}", "2".repeat(64))])
            .architecture("amd64".into())
            .stability(ImageInputStability::Pinned)
            .build();
        let layer = FrozenImageLayer {
            name: "base".into(),
            spec: ImageLayerSpec::builder()
                .stage(ImageLayerStage::Base)
                .parent(ImageLayerParent::Image(inputs.parent_digest.clone()))
                .repository("https://example.test/repo".into())
                .revision("a".repeat(40))
                .fragment("Dockerfile".into())
                .build(),
        };
        let spec = ImageBuildSpec::builder()
            .recipe_key(inputs.recipe_key().expect("key"))
            .inputs(inputs)
            .layer(layer)
            .host_ref("builder".into())
            .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
            .attempt(0)
            .reason(ImageBuildReason { description: "demand".into(), old_inputs: BTreeMap::new(), new_inputs: BTreeMap::new() })
            .build();
        home.using::<ImageBuild>("test").create(&InputMeta::builder().name("build".into()).build(), &spec).await.expect("demand");
        builder
            .using::<ImageBuild>("test")
            .create(&InputMeta::builder().name("bad".into()).build(), &spec)
            .await
            .expect("existing collision");
        let mut conflicting = spec.clone();
        conflicting.inputs.args.insert("DIFFERENT".into(), "yes".into());
        conflicting.recipe_key = conflicting.inputs.recipe_key().expect("different key");
        home.using::<ImageBuild>("test")
            .create(&InputMeta::builder().name("bad".into()).build(), &conflicting)
            .await
            .expect("colliding demand");
        builder
            .replica_writer::<ImageBuild>(NodeId::new("home"), "test")
            .replace(&home.using::<ImageBuild>("test").list().await.expect("demand snapshot"), Utc::now())
            .await
            .expect("replicate demand");
        for _ in 0..2 {
            project_builds(&builder, "test", "builder").await.expect("project");
        }
        let builds = builder.using::<ImageBuild>("test");
        assert_eq!(builds.list().await.expect("list").items.len(), 2);
        apply_status_patch(&builds, "build", &ImageBuildStatusPatch::Start {
            at: Utc::now(),
            parent_digest: spec.inputs.parent_digest.clone(),
        })
        .await
        .expect("start");
        apply_status_patch(&builds, "build", &ImageBuildStatusPatch::Built {
            at: Utc::now(),
            identity: PlacedImageIdentity { local_image_id: format!("sha256:{}", "3".repeat(64)), registry_digest: None },
            verified_provides: BTreeSet::new(),
            log_ref: "log".into(),
        })
        .await
        .expect("built");
        home.replica_writer::<ImageBuild>(NodeId::new("builder-root"), "test")
            .replace(&builds.list().await.expect("execution snapshot"), Utc::now())
            .await
            .expect("replicate evidence");
        assert_eq!(
            read_image_build(&home, "test", "build").await.expect("read execution").status.expect("evidence").phase,
            ImageBuildPhase::Built
        );
        assert!(home.using::<ImageBuild>("test").get("build").await.expect("original demand").status.is_none());
    }
}

#[cfg(test)]
mod runner_contract {
    use flotilla_core::{
        path_context::ExecutionEnvironmentPath,
        providers::{vcs::git_worktree::GitWorktreeStrategy, ChannelLabel, CommandOutput, CommandRunner},
        vcs::{FlotillaVcs, GitCheckoutStrategy},
    };
    use flotilla_resources::{
        ImageBuildReason, ImageBuildReservation, ImageInputStability, ImageLayerSpec, ImageLayerStage, ResolvedImageInputs,
    };

    use super::*;
    use crate::blob_store::{BlobDigest, MemoryBlobStore};

    // Stands in for Docker processes and enforces the documented Buildx/run
    // request contract; source contents and log/artifact storage remain real.
    #[derive(Default)]
    struct DockerProcess {
        builds: std::sync::atomic::AtomicUsize,
        removed: std::sync::atomic::AtomicUsize,
        build_failure: Option<CommandOutput>,
        probe_failure: Option<CommandOutput>,
        hang_build: bool,
        hang_probe: bool,
    }
    #[async_trait]
    impl CommandRunner for DockerProcess {
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "docker");
            if args[2] == "rm" {
                assert_eq!(args[3], "--force");
                assert!(args[4].starts_with("flotilla-probe-"));
                self.removed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(String::new());
            }
            assert_eq!(&args[2..6], &["image", "inspect", "--format", "{{json .}}"]);
            assert_eq!(args[0], "--config");
            if self.builds.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err("image not found".into());
            }
            Ok(serde_json::json!({"Id": format!("sha256:{}", "3".repeat(64)), "RepoDigests": []}).to_string())
        }
        async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            assert_eq!(cmd, "docker");
            assert_eq!(args[0], "--config");
            assert!(Path::new(args[1]).file_name().expect("config directory").to_str().expect("UTF-8").starts_with("config-"));
            match args[2] {
                "buildx" => {
                    self.builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(args[3], "build");
                    for pair in [["--builder", "default"], ["--platform", "linux/amd64"]] {
                        assert!(args.windows(2).any(|window| window == pair));
                    }
                    assert!(args.contains(&"--load"));
                    assert!(!args.contains(&"--push"));
                    if self.hang_build {
                        std::future::pending::<()>().await;
                    }
                    if let Some(output) = &self.build_failure {
                        return Ok(CommandOutput {
                            stdout: output.stdout.clone(),
                            stderr: output.stderr.clone(),
                            exit_code: output.exit_code,
                        });
                    }
                    Ok(CommandOutput { stdout: "build log\n".into(), stderr: String::new(), exit_code: Some(0) })
                }
                "run" => {
                    assert!(args.windows(2).any(|window| window == ["--entrypoint", "sh"]));
                    let shell = args.iter().position(|arg| *arg == "-c").expect("probe shell");
                    assert_eq!(args[shell + 1], flotilla_core::agent_process::with_preludes("exec \"$@\""));
                    assert_eq!(args[shell + 2], "flotilla-provide-probe");
                    assert!(args.contains(&"--pull=never"));
                    assert!(args.contains(&"--network=none"));
                    for pair in [["--cpus", "1"], ["--memory", "536870912"], ["--pids-limit", "128"]] {
                        assert!(args.windows(2).any(|window| window == pair));
                    }
                    if self.hang_probe {
                        std::future::pending::<()>().await;
                    }
                    if let Some(output) = &self.probe_failure {
                        return Ok(CommandOutput {
                            stdout: output.stdout.clone(),
                            stderr: output.stderr.clone(),
                            exit_code: output.exit_code,
                        });
                    }
                    assert!(args[shell + 3..] == ["probe", "--version"] || args[shell + 3..] == ["xdpyinfo"]);
                    Ok(CommandOutput { stdout: "probe log\n".into(), stderr: String::new(), exit_code: Some(0) })
                }
                unexpected => panic!("unexpected Docker process {unexpected}"),
            }
        }
    }

    // Generated digest-heavy progress and arbitrary failing command text must
    // never turn a recipe exit into an infrastructure retry.
    #[hegel::test]
    fn progress_digests_and_command_text_do_not_trigger_retries(tc: hegel::TestCase) {
        let offset = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(61));
        let lines = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(100));
        let digest = format!("{}429{}", "a".repeat(offset), "b".repeat(61 - offset));
        let stderr = format!("{}ERROR: failed to solve: process \"/bin/sh -c echo timeout HTTP 429 connection refused\" did not complete successfully: exit code: 1\n",
            format!("#1 sha256:{digest} timeout test output\n").repeat(lines));
        assert_eq!(classify_build_failure(Some(1), &stderr), ImageBuildFailureClass::Deterministic);
        for reason in
            ["Cannot connect to the Docker daemon", "TLS handshake timeout", "HTTP 429 Too Many Requests", "rpc error: code = Unavailable"]
        {
            assert_eq!(classify_build_failure(Some(1), reason), ImageBuildFailureClass::Transient);
        }
        assert_eq!(classify_build_failure(None, ""), ImageBuildFailureClass::Transient);
        assert_eq!(classify_build_failure(Some(137), ""), ImageBuildFailureClass::Transient);
    }

    // Buildx loads the host-local image without a registry, overrides arbitrary
    // image ENTRYPOINTs to run each declared probe, and stores the actual log.
    async fn fixture(processes: Arc<DockerProcess>) -> (tempfile::TempDir, HostImageBuildRunner, ImageBuildSpec) {
        let temp = tempfile::tempdir().expect("state directory");
        let directory = temp.path().join("image-builds");
        let layer = FrozenImageLayer {
            name: "base".into(),
            spec: ImageLayerSpec::builder()
                .stage(ImageLayerStage::Base)
                .parent(ImageLayerParent::Image(format!("sha256:{}", "1".repeat(64))))
                .repository("https://example.test/images".into())
                .revision("a".repeat(40))
                .fragment("Dockerfile".into())
                .input_stability(ImageInputStability::Pinned)
                .provides(BTreeSet::from(["test:probe".into()]))
                .probes(BTreeMap::from([("test:probe".into(), vec!["probe".into(), "--version".into()])]))
                .build(),
        };
        let source = directory.join(format!("source-{:x}", Sha256::digest(format!("{}\0{}", layer.spec.repository, layer.spec.revision))));
        std::fs::create_dir_all(source.join("context")).expect("context directory");
        std::fs::write(source.join("context/Dockerfile"), "ARG BASE\nFROM ${BASE}\n").expect("immutable source");
        std::fs::write(source.join("complete"), b"complete").expect("cached source marker");
        let runner: Arc<dyn CommandRunner> = processes.clone();
        let vcs = Arc::new(FlotillaVcs::new(
            ExecutionEnvironmentPath::new(temp.path()),
            runner.clone(),
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), runner.clone()))),
        ));
        let backend = ResourceBackend::InMemory(Default::default());
        let blobs = Arc::new(TieredBlobStore::new(temp.path(), vec![("memory".into(), Arc::new(MemoryBlobStore::default()))]));
        let builder = HostImageBuildRunner {
            distributor: None,
            images: Arc::new(flotilla_core::providers::container::docker::DockerImageStore::new(runner.clone())),
            runtime: Some(Arc::new(flotilla_core::providers::environment::docker::DockerEnvironmentProvider::new(runner))),
            vcs,
            directory,
            backend: backend.clone(),
            namespace: "test".into(),
            blobs: blobs.clone(),
        };
        let inputs = builder.resolve(&layer, "amd64").await.expect("resolve");
        let inputs = ResolvedImageInputs::builder()
            .parent_digest(format!("sha256:{}", "1".repeat(64)))
            .content_hashes(inputs.content_hashes)
            .architecture(inputs.architecture)
            .args(inputs.args)
            .pins(inputs.pins)
            .stability(inputs.stability)
            .build();
        let spec = ImageBuildSpec::builder()
            .recipe_key(inputs.recipe_key().expect("key"))
            .inputs(inputs)
            .layer(layer)
            .host_ref("local".into())
            .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
            .attempt(0)
            .reason(ImageBuildReason { description: "demand".into(), old_inputs: BTreeMap::new(), new_inputs: BTreeMap::new() })
            .build();
        (temp, builder, spec)
    }

    #[tokio::test]
    async fn buildx_verifies_provides_and_persists_its_process_log() {
        let processes = Arc::new(DockerProcess::default());
        let (_temp, builder, spec) = fixture(processes.clone()).await;
        let result = builder.build("operation", &spec, &spec.inputs.parent_digest).await.expect("build and log");
        let ImageBuildResult::Built { identity, verified_provides, log_ref } = result else { panic!("successful build expected") };
        assert_eq!(identity.local_image_id, format!("sha256:{}", "3".repeat(64)));
        assert_eq!(verified_provides, BTreeSet::from(["test:probe".into()]));
        let artifact = builder.backend.using::<Artifact>("test").get(&log_ref).await.expect("log artifact");
        assert_eq!(
            builder.blobs.get(&BlobDigest::parse(&artifact.spec.digest).expect("digest")).await.expect("log body").expect("stored log"),
            b"build log\nprobe log\n"
        );
        let service = crate::artifact::ArtifactService { backend: &builder.backend, blobs: &*builder.blobs, namespace: "test" };
        assert!(artifact.spec.convoy.is_empty());
        assert_eq!(service.list(None, Some("build-log"), None).await.expect("execution logs").len(), 1);
        assert!(service.list(Some("a-convoy"), None, None).await.expect("convoy logs").is_empty());
        assert!(service.reap_expired().await.expect("retention").contains(&BlobDigest::parse(&artifact.spec.digest).expect("digest")));
        assert!(!builder.directory.join(format!("config-{:x}", Sha256::digest("operation"))).exists());
        let again = builder.build("operation", &spec, &spec.inputs.parent_digest).await.expect("idempotent recovery");
        assert!(matches!(again, ImageBuildResult::Built { .. }));
        assert_eq!(processes.builds.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // #2730: display provides are credited only after a prelude-wrapped display
    // open succeeds; a mismatch preserves probe output and fails the build.
    #[tokio::test]
    async fn display_probe_mismatch_fails_the_build() {
        for succeeds in [false, true] {
            let processes = Arc::new(DockerProcess {
                probe_failure: (!succeeds).then(|| CommandOutput {
                    stdout: "display diagnostic\n".into(),
                    stderr: "xdpyinfo: unable to open display :99\n".into(),
                    exit_code: Some(1),
                }),
                ..Default::default()
            });
            let (_temp, builder, mut spec) = fixture(processes).await;
            spec.layer.spec.provides = BTreeSet::from(["display:headless-x11".into()]);
            spec.layer.spec.probes = BTreeMap::from([("display:headless-x11".into(), vec!["xdpyinfo".into()])]);
            spec.inputs.content_hashes = builder.resolve(&spec.layer, "amd64").await.expect("display inputs").content_hashes;
            spec.recipe_key = spec.inputs.recipe_key().expect("display recipe");
            let result = builder.build("display", &spec, &spec.inputs.parent_digest).await.expect("display build");
            if succeeds {
                let ImageBuildResult::Built { verified_provides, .. } = result else { panic!("display should verify") };
                assert_eq!(verified_provides, BTreeSet::from(["display:headless-x11".into()]));
            } else {
                let ImageBuildResult::Failed { failure, log_ref } = result else { panic!("mismatch must fail") };
                assert_eq!(failure.class, ImageBuildFailureClass::Deterministic);
                assert!(failure.reason.contains("display:headless-x11"));
                assert!(failure.reason.contains("unable to open display"));
                let artifact = builder.backend.using::<Artifact>("test").get(&log_ref).await.expect("failure artifact");
                let body = builder.blobs.get(&BlobDigest::parse(&artifact.spec.digest).expect("digest")).await.expect("blob").expect("log");
                assert!(String::from_utf8(body).expect("log text").contains("display diagnostic"));
            }
        }
    }

    // A changed fragment, prelude input or probe must invalidate the recipe;
    // repeated resolution of identical inputs must preserve its identity.
    #[tokio::test]
    async fn fragment_and_verification_changes_invalidate_recipe_keys() {
        let (_temp, builder, spec) = fixture(Arc::new(DockerProcess::default())).await;
        let context = builder.context(&spec.layer).await.expect("context");
        let original = builder.resolve(&spec.layer, "amd64").await.expect("original inputs");
        assert_eq!(original.content_hashes, spec.inputs.content_hashes);
        for (path, contents) in [("Dockerfile", "ARG BASE\nFROM ${BASE}\nRUN echo changed\n"), ("prelude.sh", "export DISPLAY=:99\n")] {
            std::fs::write(context.join(path), contents).expect("changed input");
            let changed = builder.resolve(&spec.layer, "amd64").await.expect("changed inputs");
            let mut inputs = spec.inputs.clone();
            inputs.content_hashes = changed.content_hashes;
            assert_ne!(inputs.recipe_key().expect("changed key"), spec.recipe_key);
        }
        let mut layer = spec.layer.clone();
        layer.spec.probes.insert("test:probe".into(), vec!["xdpyinfo".into()]);
        let unchanged = builder.resolve(&spec.layer, "amd64").await.expect("same context");
        let changed = builder.resolve(&layer, "amd64").await.expect("probe inputs");
        let mut before = spec.inputs.clone();
        before.content_hashes = unchanged.content_hashes;
        let mut after = before.clone();
        after.content_hashes = changed.content_hashes;
        assert_ne!(before.recipe_key().expect("before"), after.recipe_key().expect("after"));
    }
    // Real source hashes and Artifact storage surround injected Docker failures.
    #[tokio::test]
    async fn failures_keep_full_logs_but_bound_status_and_classify_probe_infrastructure() {
        let stderr = format!("{}\nERROR: failed to solve: recipe error", "#1 sha256:429 timeout test output\n".repeat(50_000));
        let processes = Arc::new(DockerProcess {
            build_failure: Some(CommandOutput { stdout: String::new(), stderr: stderr.clone(), exit_code: Some(1) }),
            ..Default::default()
        });
        let (_temp, builder, spec) = fixture(processes).await;
        let ImageBuildResult::Failed { failure, log_ref } =
            builder.build("failure", &spec, &spec.inputs.parent_digest).await.expect("failure log")
        else {
            panic!("expected failure")
        };
        assert_eq!(failure.class, ImageBuildFailureClass::Deterministic);
        assert!(failure.reason.chars().count() <= MAX_REASON_CHARS);
        let artifact = builder.backend.using::<Artifact>("test").get(&log_ref).await.expect("artifact");
        let body = builder.blobs.get(&BlobDigest::parse(&artifact.spec.digest).expect("digest")).await.expect("blob").expect("log");
        assert!(body.starts_with(stderr.as_bytes()));
        for (code, reason, expected) in [
            (125, "Cannot connect to the Docker daemon", ImageBuildFailureClass::Transient),
            (125, "no space left on device", ImageBuildFailureClass::Transient),
            (1, "HTTP 429 in a failing version check", ImageBuildFailureClass::Deterministic),
            (127, "probe executable is missing", ImageBuildFailureClass::Deterministic),
        ] {
            let processes = Arc::new(DockerProcess {
                probe_failure: Some(CommandOutput { stdout: String::new(), stderr: reason.into(), exit_code: Some(code) }),
                ..Default::default()
            });
            let (_temp, builder, spec) = fixture(processes).await;
            let ImageBuildResult::Failed { failure, .. } =
                builder.build("probe", &spec, &spec.inputs.parent_digest).await.expect("probe log")
            else {
                panic!("expected probe failure")
            };
            assert_eq!(failure.class, expected);
            assert!(failure.reason.contains(reason));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hung_processes_release_the_execution_and_remove_timed_out_probes() {
        for probe in [false, true] {
            let processes = Arc::new(DockerProcess { hang_build: !probe, hang_probe: probe, ..Default::default() });
            let (_temp, builder, spec) = fixture(processes.clone()).await;
            let started = tokio::time::Instant::now();
            let ImageBuildResult::Failed { failure, .. } =
                builder.build("hung", &spec, &spec.inputs.parent_digest).await.expect("timeout log")
            else {
                panic!("expected timeout")
            };
            assert_eq!(failure.class, ImageBuildFailureClass::Transient);
            assert!(failure.reason.contains("wall-clock deadline"));
            assert!(started.elapsed() >= if probe { PROBE_TIMEOUT } else { BUILD_TIMEOUT });
            assert_eq!(processes.removed.load(std::sync::atomic::Ordering::SeqCst), usize::from(probe));
        }
    }

    #[tokio::test]
    async fn frozen_context_mismatch_stops_before_any_docker_process() {
        let processes = Arc::new(DockerProcess::default());
        let (_temp, builder, mut spec) = fixture(processes.clone()).await;
        spec.inputs.content_hashes = vec![format!("sha256:{}", "f".repeat(64))];
        let ImageBuildResult::Failed { failure, .. } =
            builder.build("mismatch", &spec, &spec.inputs.parent_digest).await.expect("mismatch log")
        else {
            panic!("expected mismatch")
        };
        assert_eq!(failure.class, ImageBuildFailureClass::Deterministic);
        assert_eq!(failure.reason, "image context differs from frozen input hashes");
        assert_eq!(processes.builds.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    #[test]
    fn bounded_reasons_preserve_unicode_and_hashes_stream_binary_inputs() {
        let reason = "λ".repeat(MAX_REASON_CHARS * 4);
        let summary = failure_summary(&reason, Some(1));
        assert_eq!(summary.chars().count(), MAX_REASON_CHARS);
        let temp = tempfile::tempdir().expect("context");
        let bytes = (0..200_000).map(|index| (index % 256) as u8).collect::<Vec<_>>();
        std::fs::write(temp.path().join("binary"), &bytes).expect("binary context");
        let first = hash_context(temp.path()).expect("stream hashes");
        std::fs::write(temp.path().join("binary"), &bytes).expect("same bytes, new timestamp");
        assert_eq!(first, hash_context(temp.path()).expect("same contents"));
        std::fs::write(temp.path().join("binary"), b"changed").expect("changed contents");
        assert_ne!(first, hash_context(temp.path()).expect("changed hash"));
    }
}
