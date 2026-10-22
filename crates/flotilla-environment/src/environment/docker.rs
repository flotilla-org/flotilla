//! Docker-backed environment provider.
//!
//! Shells out to the `docker` CLI via `CommandRunner`, consistent with how every
//! other provider interacts with external tools.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use super::agent::{AgentConnector, AgentRunner, UnixAgentConnector};
#[cfg(unix)]
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::time::Duration;

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, ImageId};

use super::{
    runner::DockerEnvironmentRunner, CreateOpts, EnvironmentBacking, EnvironmentHandle, EnvironmentProvider, EnvironmentToolAssetAccess,
    EnvironmentToolAssetKind, EnvironmentVariableUpdate, ImagePullPolicy, ProvisionedEnvironment, ProvisionedMount, ProvisionedMountMode,
};
use crate::environment::{PrepareOpts, ProvisionOpts};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;

/// Equal relative CPU scheduling weight for Docker crew environments.
/// Docker translates shares to the native cgroup weight when using cgroup v2.
pub const CREW_CPU_SHARES: usize = 1024;

/// The `PATH` Docker gives a container whose image does not set one.
const DOCKER_DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// ---------------------------------------------------------------------------
// DockerEnvironmentProvider
// ---------------------------------------------------------------------------

/// An `EnvironmentProvider` that manages Docker containers as sandbox environments.
pub struct DockerEnvironmentProvider {
    pub(super) inner: Arc<DockerEnvironmentProviderInner>,
    preparation_owner: Arc<()>,
}

impl DockerEnvironmentProvider {
    async fn pull_prepared_image(&self, image: &str, opts: &PrepareOpts) -> Result<(), String> {
        if let Some(auth) = &opts.prepared_auth {
            auth.validate(image, flotilla_resources::HostImageAction::ImagePull)?;
        }
        let config = super::RegistryAuth::config(opts.prepared_auth.as_ref())?;
        let directory = config.path().to_string_lossy();
        let mut args = vec!["--config", directory.as_ref()];
        args.extend(["pull", image]);
        self.inner
            .runner
            .run("docker", &args, Path::new("/"), &ChannelLabel::Default)
            .await
            .map(|_| ())
            .map_err(|error| format!("pinned image pull failed: {}", super::RegistryAuth::redact(opts.prepared_auth.as_ref(), error)))
    }

    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { inner: Arc::new(DockerEnvironmentProviderInner::new(runner)), preparation_owner: Arc::new(()) }
    }
}

/// These are host facts, supplied by discovery. Neither a vessel nor its image
/// chooses the current agent executable or the host runtime directory.
#[cfg(unix)]
#[derive(Clone)]
pub struct DockerAgentSettings {
    pub runtime_root: PathBuf,
    pub executable: PathBuf,
}
#[cfg(unix)]
type ConnectorFactory = dyn Fn(&Path) -> Arc<dyn AgentConnector> + Send + Sync;
#[cfg(unix)]
struct AgentLease {
    runner: Arc<AgentRunner>,
    supervisor: tokio::task::JoinHandle<()>,
}
#[cfg(unix)]
impl Drop for AgentLease {
    fn drop(&mut self) {
        self.supervisor.abort();
    }
}

#[cfg(unix)]
impl DockerEnvironmentProvider {
    pub fn with_agent_settings(mut self, settings: DockerAgentSettings) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure before sharing provider").agent_settings = settings;
        self
    }
    /// Injectable process/transport seam. Docker command tests still run the
    /// production preparation and supervisor commands, then dial a real Agent.
    pub fn with_agent_connector(mut self, connector: Arc<ConnectorFactory>) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure before sharing provider").connector = Some(connector);
        self
    }
}
struct DockerPrepared {
    original: ImageId,
    image: ImageId,
}

#[cfg(unix)]
fn host_user() -> String {
    // SAFETY: getuid and getgid are side-effect-free process identity queries.
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

#[async_trait]
impl EnvironmentProvider for DockerEnvironmentProvider {
    async fn detect_readiness(&self) -> super::ProviderReadiness {
        // Probe the provider endpoint's host, which can be reached over SSH.
        if let Err(reason) = self.inner.runner.run("docker", &["--version"], Path::new("/"), &ChannelLabel::Default).await {
            return super::ProviderReadiness::MissingTool { reason };
        }
        match self.inner.runner.run("uname", &["-s"], Path::new("/"), &ChannelLabel::Default).await {
            Ok(os) if os.trim() == "Linux" => {
                match self.inner.runner.run("docker", &["info", "--format", "{{json .}}"], Path::new("/"), &ChannelLabel::Default).await {
                    Ok(_) => super::ProviderReadiness::Ready,
                    Err(reason) => super::ProviderReadiness::Unknown { reason },
                }
            }
            Ok(_) => super::ProviderReadiness::MissingTool { reason: "Linux host required for daemon-adjacent injection".into() },
            Err(reason) => super::ProviderReadiness::Unknown { reason },
        }
    }
    fn kind(&self) -> super::ProviderKind {
        super::ProviderKind::new(super::EnvironmentClass::Container, "docker")
    }
    fn capabilities(&self) -> super::ProviderCapabilities {
        Self::capability_declaration()
    }

    fn image_build_service(&self, builder: super::OwnedBuilderSettings) -> Option<Arc<dyn super::ImageBuildService>> {
        Some(Arc::new(super::docker_image::OwnedBuildKitService::new(self.inner.runner.clone(), builder)))
    }
    fn local_image_cache(&self) -> Option<&dyn super::LocalImageCache> {
        Some(self)
    }

    fn legacy_input(&self) -> Option<super::EnvironmentInput> {
        Some(super::EnvironmentInput::Docker)
    }

    fn detection(&self) -> Option<&dyn super::EnvironmentDetection> {
        Some(self)
    }

    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, opts: &PrepareOpts) -> Result<super::PreparedEnvironment, String> {
        if Some(super::EnvironmentInput::of(spec)?) != self.legacy_input() {
            return Err("Docker provider requires a Docker environment spec".into());
        }
        let spec = spec.docker.as_ref().expect("kind checked");
        let registry_digest = spec.image.rsplit_once('@').is_some_and(|(_, digest)| flotilla_resources::is_image_digest(digest));
        if !flotilla_resources::is_image_digest(&spec.image) && !registry_digest {
            // #2731 has not cut fleet baselines over to ImageBuild. Admission is
            // verified by the controller, never inferred from a tag's spelling.
            if !opts.legacy_baseline || spec.image_build_ref.is_some() || spec.image.contains('@') || spec.image.is_empty() {
                return Err("Docker environments require a pinned digest; mutable tags are unsupported".into());
            }
            let held = self.inner.image_exists(&spec.image, Path::new("/")).await?;
            match spec.pull_policy {
                flotilla_resources::DockerImagePullPolicy::Always => self.pull_prepared_image(&spec.image, opts).await?,
                flotilla_resources::DockerImagePullPolicy::IfNotPresent if !held => self.pull_prepared_image(&spec.image, opts).await?,
                flotilla_resources::DockerImagePullPolicy::Never if !held => {
                    return Err("baseline image is absent and pull policy is never".into())
                }
                _ => {}
            }
            let digest = self
                .inner
                .runner
                .run("docker", &["image", "inspect", "--format", "{{.Id}}", &spec.image], Path::new("/"), &ChannelLabel::Default)
                .await?;
            let digest = digest.trim();
            if !flotilla_resources::is_image_digest(digest) {
                return Err("baseline image did not resolve to a local digest".into());
            }
            return self.prepare_agent_image(ImageId::new(digest)).await;
        }
        if !self.inner.image_exists(&spec.image, Path::new("/")).await? {
            if !registry_digest {
                return Err("waiting on build: requested digest is absent from this provider's cache".into());
            }
            self.pull_prepared_image(&spec.image, opts).await?;
            if !self.inner.image_exists(&spec.image, Path::new("/")).await? {
                return Err("pinned image pull succeeded but digest is not present".into());
            }
        }
        self.prepare_agent_image(ImageId::new(spec.image.clone())).await
    }

    async fn provision(
        &self,
        id: EnvironmentId,
        prepared: &super::PreparedEnvironment,
        opts: ProvisionOpts,
    ) -> Result<EnvironmentHandle, String> {
        let prepared = prepared.get::<DockerPrepared>(&self.preparation_owner)?;
        #[cfg(not(unix))]
        {
            let _ = (id, opts, prepared);
            Err("Docker vessel agents require a Unix host".into())
        }
        #[cfg(unix)]
        {
            let directory = self.inner.runtime_directory(&id)?;
            let mut tools = opts.tools.clone();
            // Files are pushed only after hello. Shared writable material and
            // vessel-specific daemon sockets keep their existing mount contract.
            for tool in &mut tools {
                tool.assets.retain(|asset| asset.requires_mount());
            }
            if opts.tools.iter().flat_map(|tool| &tool.assets).any(|asset| {
                asset.environment_path.as_path().starts_with("/run/drydock")
                    || Path::new("/run/drydock").starts_with(asset.environment_path.as_path())
            }) {
                return Err("/run/drydock is reserved for this vessel's agent".into());
            }
            let mut mounts = opts.provisioned_mounts;
            if mounts.iter().any(|mount| {
                mount.environment_path.as_path().starts_with("/run/drydock")
                    || Path::new("/run/drydock").starts_with(mount.environment_path.as_path())
            }) {
                return Err("/run/drydock is reserved for this vessel's agent".into());
            }
            mounts.push(ProvisionedMount::new(directory.join("guest"), "/run/drydock", ProvisionedMountMode::Rw));
            let handle = self
                .create_with_image(
                    id.clone(),
                    &prepared.original,
                    &prepared.image,
                    CreateOpts {
                        tokens: opts.tokens,
                        working_directory: opts.working_directory,
                        provisioned_mounts: mounts,
                        tools,
                        cpu_limit: opts.cpu_limit,
                        memory_policy: opts.memory_policy,
                        image_pull_policy: ImagePullPolicy::Never,
                        prepared_auth: Default::default(),
                    },
                )
                .await?;
            let result = async {
                let lease = self.inner.agent_for(handle.backing_id(), &directory).await?;
                lease.runner.configure_environment(self.inner.env_vars(handle.backing_id()).await?.into_iter().collect()).await?;
                lease.runner.push_tools(&opts.tools).await?;
                let mut mounts = handle.provisioned_mounts();
                // Preserve the base identity across prepared-image rebuilds/adoption.
                let base = prepared.original.clone();
                Ok(self.inner.environment_with_agent(
                    id,
                    base,
                    handle.local_image_id().map(str::to_owned),
                    handle.backing_id().into(),
                    handle.container_name().unwrap_or(handle.backing_id()).into(),
                    std::mem::take(&mut mounts),
                    lease,
                ))
            }
            .await;
            if result.is_err() {
                let _ = self.inner.destroy(handle.backing_id()).await;
            }
            result
        }
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        let format = r#"{{.ID}} {{.Names}}\t{{.Label "flotilla.environment"}}\t{{if .Label "flotilla.source_image"}}{{.Label "flotilla.source_image"}}{{else}}{{.Image}}{{end}}\t{{.Label "flotilla.provisioned_mounts"}}"#;
        let output = self
            .inner
            .runner
            .run(
                "docker",
                &["ps", "-a", "--no-trunc", "--filter", "label=flotilla.environment", "--format", format],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await?;

        let mut handles = Vec::new();
        for line in output.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() < 4 {
                tracing::warn!(raw = %line, "docker list output missing provisioned mount metadata");
                return Err("docker list output missing provisioned mount metadata".to_string());
            }
            let (backing_id, container_name) = parts[0].split_once(' ').unwrap_or((parts[0], parts[0]));
            let backing_id = backing_id.to_string();
            let container_name = container_name.to_string();
            let env_id = parts[1].to_string();
            let image = parts[2].to_string();
            if env_id.is_empty() {
                tracing::warn!(container = %container_name, raw = %line, "docker list output missing environment id");
                return Err("docker list output missing environment id".to_string());
            }
            let mount_metadata = parts[3].trim();
            if mount_metadata.is_empty() {
                tracing::warn!(container = %container_name, "docker list output missing provisioned mount metadata");
                return Err(format!("docker list output missing provisioned mount metadata for container {container_name}"));
            }
            let provisioned_mounts: Vec<ProvisionedMount> = match serde_json::from_str(mount_metadata) {
                Ok(mounts) => mounts,
                Err(err) => {
                    tracing::warn!(container = %container_name, err = %err, raw = %mount_metadata, "failed to parse provisioned mount metadata");
                    return Err(format!("failed to parse provisioned mount metadata for container {container_name}: {err}"));
                }
            };
            #[cfg(unix)]
            if let Some(mount) =
                provisioned_mounts.iter().find(|mount: &&ProvisionedMount| mount.environment_path.as_path() == Path::new("/run/drydock"))
            {
                let directory = mount.host_path.as_path().parent().ok_or("invalid agent mount")?;
                if matches!(provider_observation(self.inner.runner.as_ref(), &backing_id).await?.state, super::ObservationState::Running) {
                    let lease = self.inner.agent_for(&backing_id, directory).await?;
                    // Reconnect/upgrade before adopting a running vessel. The
                    // daemon pushes its current CLI layer before probing tools.
                    lease.runner.configure_environment(self.inner.env_vars(&backing_id).await?.into_iter().collect()).await?;
                    handles.push(self.inner.environment_with_agent(
                        EnvironmentId::new(env_id),
                        ImageId::new(image),
                        None,
                        backing_id,
                        container_name,
                        provisioned_mounts,
                        lease,
                    ));
                    continue;
                }
            }
            handles.push(self.inner.provisioned_environment(
                EnvironmentId::new(env_id),
                ImageId::new(image),
                None,
                backing_id,
                container_name,
                provisioned_mounts,
            ));
        }

        Ok(handles)
    }

    async fn list_backings(&self) -> Result<Vec<EnvironmentBacking>, String> {
        // Full immutable IDs prevent a delayed sweep removing a replacement
        // container that reused an old name. Ignore mutable mount metadata.
        let output = self
            .inner
            .runner
            .run(
                "docker",
                &[
                    "ps",
                    "-a",
                    "--no-trunc",
                    "--filter",
                    "label=flotilla.environment",
                    "--format",
                    r#"{{.ID}}\t{{.Label "flotilla.environment"}}"#,
                ],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await?;
        output
            .lines()
            .map(|line| {
                let (container_id, environment_id) = line.split_once('\t').ok_or("Docker backing identity is incomplete")?;
                if container_id.len() != 64
                    || !container_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || environment_id.is_empty()
                    || environment_id.contains('\t')
                {
                    return Err("Docker backing identity is invalid".to_string());
                }
                Ok(EnvironmentBacking { environment_id: EnvironmentId::new(environment_id), backing_id: container_id.into() })
            })
            .collect()
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        self.inner.destroy(container_id).await
    }
}

fn protected_git_mount(mount: &ProvisionedMount) -> bool {
    mount.mode == ProvisionedMountMode::Ro
        && mount.host_path.as_path() == mount.environment_path.as_path()
        && mount.host_path.as_path().file_name().is_some_and(|name| name == "config" || name == "hooks" || name == "worktrees")
}

pub(super) struct DockerEnvironmentProviderInner {
    pub(super) runner: Arc<dyn CommandRunner>,
    #[cfg(unix)]
    agent_settings: DockerAgentSettings,
    #[cfg(unix)]
    connector: Option<Arc<ConnectorFactory>>,
    #[cfg(unix)]
    agents: tokio::sync::Mutex<HashMap<String, Arc<AgentLease>>>,
}

impl DockerEnvironmentProviderInner {
    fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            #[cfg(unix)]
            agent_settings: DockerAgentSettings {
                runtime_root: PathBuf::from("/tmp/flotilla-drydock"),
                executable: PathBuf::from("/usr/local/bin/drydock-agent"),
            },
            #[cfg(unix)]
            connector: None,
            #[cfg(unix)]
            agents: Default::default(),
        }
    }

    async fn image_exists(&self, tag: &str, cwd: &Path) -> Result<bool, String> {
        match self.runner.run("docker", &["image", "inspect", tag], cwd, &ChannelLabel::Default).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn provisioned_environment(
        self: &Arc<Self>,
        id: EnvironmentId,
        image: ImageId,
        local_image_id: Option<String>,
        backing_id: String,
        container_name: String,
        provisioned_mounts: Vec<ProvisionedMount>,
    ) -> EnvironmentHandle {
        let runner = Arc::new(DockerEnvironmentRunner::new(container_name.clone(), Arc::clone(&self.runner))) as Arc<dyn CommandRunner>;
        Arc::new(DockerProvisionedEnvironment {
            id,
            backing_id,
            container_name,
            image,
            local_image_id,
            inner: Arc::clone(self),
            runner,
            agent: None,
            provisioned_mounts,
        })
    }

    /// Resolves `image` locally under `pull_policy` and returns the environment
    /// its config declares.
    async fn image_environment(
        &self,
        image: &str,
        pull_policy: ImagePullPolicy,
        docker_config: Option<&str>,
    ) -> Result<HashMap<String, String>, String> {
        let output = match pull_policy {
            ImagePullPolicy::Always => {
                self.pull(image, docker_config)
                    .await
                    .map_err(|error| format!("pulling image {image} to read its environment failed: {error}"))?;
                self.inspect_environment(image).await?
            }
            ImagePullPolicy::IfNotPresent => match self.inspect_environment(image).await {
                Ok(output) => output,
                Err(inspect_error) => {
                    self.pull(image, docker_config).await.map_err(|error| {
                        format!(
                            "image {image} could not be inspected ({inspect_error}) and pulling it to read its environment failed: {error}"
                        )
                    })?;
                    self.inspect_environment(image).await?
                }
            },
            ImagePullPolicy::Never => self
                .inspect_environment(image)
                .await
                .map_err(|error| format!("image {image} is not available locally and the pull policy is never: {error}"))?,
        };
        let entries: Option<Vec<String>> = serde_json::from_str(output.trim())
            .map_err(|error| format!("docker returned invalid environment for image {image}: {error}"))?;
        Ok(entries
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| entry.split_once('=').map(|(name, value)| (name.to_string(), value.to_string())))
            .collect())
    }

    async fn inspect_environment(&self, image: &str) -> Result<String, String> {
        self.runner
            .run("docker", &["image", "inspect", "--format", "{{json .Config.Env}}", image], Path::new("/"), &ChannelLabel::Default)
            .await
    }

    async fn pull(&self, image: &str, docker_config: Option<&str>) -> Result<(), String> {
        let mut args = Vec::new();
        if let Some(config) = docker_config {
            args.extend(["--config", config]);
        }
        args.extend(["pull", image]);
        self.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
    }

    async fn local_image_id(&self, container_name: &str) -> Result<String, String> {
        let output = self
            .runner
            .run("docker", &["inspect", "--format", "{{.Image}}", container_name], Path::new("/"), &ChannelLabel::Default)
            .await?;
        let digest = output.trim();
        if !digest.starts_with("sha256:") || digest.len() == "sha256:".len() {
            return Err(format!("docker returned invalid image digest for container {container_name}: {digest:?}"));
        }
        Ok(digest.to_string())
    }

    async fn env_vars(&self, container_name: &str) -> Result<HashMap<String, String>, String> {
        // Config.Env includes image defaults and docker-run declarations, without
        // importing host ambient values or running a login shell inside the vessel.
        let output = self
            .runner
            .run("docker", &["inspect", "--format", "{{json .Config.Env}}", container_name], Path::new("/"), &ChannelLabel::Default)
            .await?;
        let entries: Option<Vec<String>> = serde_json::from_str(output.trim())
            .map_err(|error| format!("docker returned invalid environment for container {container_name}: {error}"))?;
        let entries = entries.unwrap_or_default();
        if entries.is_empty() {
            tracing::debug!(container = %container_name, "container has no configured environment; resolving runtime defaults");
        }
        // Docker entries are NAME=VALUE. Ignore malformed entries without '='.
        let mut environment: HashMap<String, String> = entries
            .into_iter()
            .filter_map(|entry| entry.split_once('=').map(|(key, value)| (key.to_string(), value.to_string())))
            .collect();
        if !environment.contains_key("HOME") {
            // Docker supplies HOME for the container user at exec time even when
            // Config.Env omits it. Read only that vessel-local default, without
            // login-shell mutations; an explicitly configured HOME always wins.
            // Probe errors fail discovery rather than silently launching crew tools
            // with an unknown HOME. Images without a shell can declare HOME
            // explicitly and avoid this probe.
            let home = self
                .runner
                .run("docker", &["exec", container_name, "sh", "-c", "printf %s \"${HOME-}\""], Path::new("/"), &ChannelLabel::Default)
                .await?;
            if !home.is_empty() {
                environment.insert("HOME".to_string(), home);
            }
        }
        Ok(environment)
    }

    async fn destroy(&self, container_name: &str) -> Result<(), String> {
        #[cfg(unix)]
        self.agents.lock().await.remove(container_name);
        self.runner.run("docker", &["rm", "-f", container_name], Path::new("/"), &ChannelLabel::Default).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DockerProvisionedEnvironment
// ---------------------------------------------------------------------------

/// A live handle to a Docker container environment.
pub struct DockerProvisionedEnvironment {
    id: EnvironmentId,
    backing_id: String,
    container_name: String,
    image: ImageId,
    local_image_id: Option<String>,
    pub(super) inner: Arc<DockerEnvironmentProviderInner>,
    pub(super) runner: Arc<dyn CommandRunner>,
    provisioned_mounts: Vec<ProvisionedMount>,
    agent: Option<Arc<dyn super::VesselAgent>>,
}

#[async_trait]
impl ProvisionedEnvironment for DockerProvisionedEnvironment {
    fn capabilities(&self) -> super::ProviderCapabilities {
        DockerEnvironmentProvider::capability_declaration()
    }
    fn backing_id(&self) -> &str {
        &self.backing_id
    }

    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
    }

    fn local_image_id(&self) -> Option<&str> {
        self.local_image_id.as_deref()
    }

    fn registry_digest(&self) -> Option<&str> {
        Some(self.image.as_str()).filter(|image| image.contains("@sha256:"))
    }

    fn container_name(&self) -> Option<&str> {
        Some(&self.container_name)
    }

    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        self.provisioned_mounts.clone()
    }

    async fn observe(&self) -> super::ProviderObservation {
        match provider_observation(self.inner.runner.as_ref(), &self.backing_id).await {
            Ok(observation) => observation,
            Err(message) => super::ProviderObservation::new(super::ObservationState::Unknown {
                reason: super::UnknownReason::ProviderApiError { message },
            }),
        }
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        self.inner.env_vars(&self.container_name).await
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }
    fn agent(&self) -> Option<Arc<dyn super::VesselAgent>> {
        self.agent.clone()
    }
    fn fallback_runner(&self) -> Option<Arc<dyn CommandRunner>> {
        Some(Arc::new(DockerEnvironmentRunner::new(self.container_name.clone(), self.inner.runner.clone())))
    }

    fn fallback_shell_plan(&self, cwd: &Path) -> Option<flotilla_protocol::ResolvedAttachPlan> {
        use flotilla_protocol::arg::Arg;
        Some(flotilla_protocol::ResolvedAttachPlan::command(vec![
            Arg::Literal("sh".into()),
            Arg::Literal("-c".into()),
            Arg::Quoted("printf '%s\\n' 'Vessel agent unavailable; using Docker fallback runner.' >&2; exec \"$@\"".into()),
            Arg::Literal("flotilla-fallback-shell".into()),
            Arg::Literal("docker".into()),
            Arg::Literal("exec".into()),
            Arg::Literal("-it".into()),
            Arg::Literal("-w".into()),
            Arg::Quoted(cwd.to_string_lossy().into_owned()),
            Arg::Quoted(self.container_name.clone()),
            Arg::Literal("sh".into()),
            Arg::Literal("-l".into()),
        ]))
    }

    async fn destroy(&self) -> Result<(), String> {
        self.inner.destroy(&self.backing_id).await
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerMemoryInfo {
    mem_total: u64,
    memory_limit: bool,
    swap_limit: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspection {
    id: String,
    state: DockerState,
    host_config: DockerHostConfig,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerState {
    status: String,
    #[serde(rename = "OOMKilled")]
    oom_killed: bool,
    exit_code: i32,
    started_at: String,
    finished_at: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerHostConfig {
    #[serde(default)]
    cpu_shares: usize,
    #[serde(default)]
    nano_cpus: u64,
    #[serde(default)]
    cpu_quota: i64,
    #[serde(default)]
    cpu_period: u64,
    memory: u64,
    memory_swap: i64,
}

/// Full inspect is necessary: the OOM flag alone misses userspace oomd kills.
async fn provider_observation(runner: &dyn CommandRunner, container: &str) -> Result<super::ProviderObservation, String> {
    let raw = runner.run("docker", &["inspect", container], Path::new("/"), &ChannelLabel::Default).await?;
    let inspect = parse_inspection(&raw)?;
    let mut oomd = String::new();
    let mut kernel = String::new();
    if (inspect.state.status == "exited" || inspect.state.status == "dead") && inspect.state.exit_code == 137 && !inspect.state.oom_killed {
        // Bound evidence to this container's lifetime. Journal access is best
        // effort: permission errors must never turn unknown SIGKILL into OOM.
        let start = chrono::DateTime::parse_from_rfc3339(&inspect.state.started_at)
            .map_err(|error| format!("invalid Docker StartedAt: {error}"))?;
        let finish = chrono::DateTime::parse_from_rfc3339(&inspect.state.finished_at)
            .map_err(|error| format!("invalid Docker FinishedAt: {error}"))?;
        let start = start.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S UTC").to_string();
        // Include the fractional finishing second in journalctl's window.
        let finish = (finish.with_timezone(&chrono::Utc) + chrono::Duration::seconds(1)).format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let time = ["--since", start.as_str(), "--until", finish.as_str()];
        let mut args = vec!["--no-pager", "--utc", "--output=short-iso", "--unit=systemd-oomd"];
        args.extend(time);
        oomd = runner.run("journalctl", &args, Path::new("/"), &ChannelLabel::Default).await.unwrap_or_default();
        let mut args = vec!["--no-pager", "--utc", "--output=short-iso", "--kernel"];
        args.extend(time);
        kernel = runner.run("journalctl", &args, Path::new("/"), &ChannelLabel::Default).await.unwrap_or_default();
    }
    let mut observation = classify_inspection(&inspect, &oomd, &kernel);
    if observation.termination.is_none() {
        // The systemd cgroup path is optional; unavailable/non-systemd hosts
        // retain their previous sample rather than inventing a zero-byte usage.
        if inspect.id.len() == 64 && inspect.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let path = format!("/sys/fs/cgroup/system.slice/docker-{}.scope/memory.current", inspect.id);
            if let Ok(raw) = runner.run("cat", &[&path], Path::new("/"), &ChannelLabel::Default).await {
                observation.memory_usage_bytes = raw.trim().parse().ok();
                if observation.memory_usage_bytes.is_some() {
                    observation.memory_observed_at = Some(chrono::Utc::now().to_rfc3339());
                }
            }
        }
    }
    let state = classify_provider_state(&inspect.state.status, &observation);
    Ok(super::ProviderObservation { state, runtime: Some(observation) })
}

fn classify_provider_state(status: &str, runtime: &flotilla_protocol::EnvironmentRuntimeObservation) -> super::ObservationState {
    use super::{GoneCause, ObservationState, StoppedCause, UnknownReason};
    use flotilla_protocol::EnvironmentExitCause;
    match status {
        "running" => ObservationState::Running,
        "created" | "restarting" => ObservationState::Unknown { reason: UnknownReason::GuestUnresponsive },
        "paused" => ObservationState::Stopped { cause: StoppedCause::PausedByProvider },
        "exited" | "dead" => {
            let cause = match runtime.termination.as_ref() {
                Some(exit) => match exit.cause {
                    EnvironmentExitCause::HostOomd | EnvironmentExitCause::KernelOom => GoneCause::HostOutOfMemory,
                    EnvironmentExitCause::CgroupOom => GoneCause::MemoryLimitReached,
                    EnvironmentExitCause::NormalStop => GoneCause::GuestPoweredOff,
                    EnvironmentExitCause::Signal | EnvironmentExitCause::NonzeroExit => GoneCause::BackingExited { code: exit.exit_code },
                },
                None => {
                    return ObservationState::Unknown {
                        reason: UnknownReason::ProviderApiError { message: "exit status without termination evidence".into() },
                    }
                }
            };
            ObservationState::Gone { cause }
        }
        other => ObservationState::Unknown { reason: UnknownReason::ProviderApiError { message: format!("Docker status {other}") } },
    }
}

fn parse_inspection(raw: &str) -> Result<DockerInspection, String> {
    let mut inspections: Vec<DockerInspection> = serde_json::from_str(raw).map_err(|error| format!("decode Docker inspect: {error}"))?;
    if inspections.len() != 1 {
        return Err("expected exactly one Docker inspect record".into());
    }
    let inspect = inspections.remove(0);
    if inspect.id.len() != 64 || !inspect.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Docker inspect returned an invalid container ID".into());
    }
    Ok(inspect)
}

fn classify_inspection(inspect: &DockerInspection, oomd: &str, kernel: &str) -> flotilla_protocol::EnvironmentRuntimeObservation {
    use flotilla_protocol::{EnvironmentExitCause, EnvironmentMemoryLimits, EnvironmentRuntimeObservation, EnvironmentTermination};
    let memory_limits = u64::try_from(inspect.host_config.memory_swap).ok().and_then(|combined| {
        (inspect.host_config.memory > 0).then_some(())?;
        Some(EnvironmentMemoryLimits {
            memory_bytes: inspect.host_config.memory,
            swap_bytes: combined.checked_sub(inspect.host_config.memory)?,
        })
    });
    let mut observation = EnvironmentRuntimeObservation {
        container_id: Some(inspect.id.clone()),
        started_at: Some(inspect.state.started_at.clone()),
        memory_limits,
        configured_limits: Some(flotilla_protocol::ConfiguredResourceLimits {
            cpus: if inspect.host_config.nano_cpus > 0 {
                Some(inspect.host_config.nano_cpus.div_ceil(1_000_000_000) as usize)
            } else if inspect.host_config.cpu_quota > 0 && inspect.host_config.cpu_period > 0 {
                Some((inspect.host_config.cpu_quota as u64).div_ceil(inspect.host_config.cpu_period) as usize)
            } else {
                None
            },
            cpu_shares: (inspect.host_config.cpu_shares > 0).then_some(inspect.host_config.cpu_shares),
            ..Default::default()
        }),
        ..Default::default()
    };
    if inspect.state.status != "exited" && inspect.state.status != "dead" {
        return observation;
    }
    let code = inspect.state.exit_code;
    let signal = (129..=192).contains(&code).then_some(code - 128);
    let scope = format!("docker-{}.scope", inspect.id);
    // Match the systemd-oomd format recorded on feta (Docker 27, systemd/cgroup v2).
    // Other journal formats remain unknown unless they provide this kill evidence.
    let oomd_kill = oomd.lines().find(|line| line.contains(&scope) && line.contains("Marked ") && line.contains(" for killing"));
    let kernel_kill =
        kernel.lines().find(|line| line.contains(&inspect.id) && (line.contains("oom-kill:") || line.contains("Killed process")));
    let (cause, evidence) = if inspect.state.oom_killed {
        (EnvironmentExitCause::CgroupOom, None)
    } else if code == 137 && oomd_kill.is_some() {
        (EnvironmentExitCause::HostOomd, oomd_kill.map(str::to_string))
    } else if code == 137 && kernel_kill.is_some() {
        (EnvironmentExitCause::KernelOom, kernel_kill.map(str::to_string))
    // 143 conventionally denotes SIGTERM: this includes external SIGTERM, since
    // inspect alone cannot establish whether Docker stop delivered the signal.
    } else if code == 0 || code == 143 {
        (EnvironmentExitCause::NormalStop, None)
    } else if signal.is_some() {
        (EnvironmentExitCause::Signal, None)
    } else {
        (EnvironmentExitCause::NonzeroExit, None)
    };
    observation.termination = Some(
        EnvironmentTermination::builder()
            .exit_code(code)
            .maybe_signal(signal)
            .oom_killed(inspect.state.oom_killed)
            .cause(cause)
            .finished_at(inspect.state.finished_at.clone())
            .maybe_evidence(evidence)
            .build(),
    );
    observation
}

impl DockerEnvironmentProvider {
    #[cfg(test)]
    pub(super) async fn create(&self, id: EnvironmentId, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String> {
        self.create_with_image(id, image, image, opts).await
    }
    async fn create_with_image(
        &self,
        id: EnvironmentId,
        image: &ImageId,
        runtime_image: &ImageId,
        opts: CreateOpts,
    ) -> Result<EnvironmentHandle, String> {
        let info = self
            .inner
            .runner
            .run("docker", &["info", "--format", "{{json .}}"], Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("Docker host memory preflight failed: {error}"))?;
        let info: DockerMemoryInfo = serde_json::from_str(&info).map_err(|error| format!("read Docker host memory: {error}"))?;
        if !info.memory_limit || !info.swap_limit {
            return Err("Docker host cannot enforce memory and swap limits".into());
        }
        let limits = opts.memory_policy.resolve(info.mem_total)?;
        let memory = limits.memory_bytes.to_string();
        // Docker wants RAM + swap; equal values explicitly disable swap.
        let memory_swap = (limits.memory_bytes + limits.swap_bytes).to_string();
        let container_name = format!("flotilla-env-{}", id);

        let requested_mounts = opts.provisioned_mounts;
        let mut provisioned_mounts = Vec::new();
        let mut tokens = opts.tokens;
        let config = super::RegistryAuth::config(opts.prepared_auth.as_ref())?;
        let docker_config = (opts.image_pull_policy != ImagePullPolicy::Never).then(|| config.path().to_string_lossy().into_owned());
        let mut pull_policy = opts.image_pull_policy.docker_value();
        // Docker replaces an image's variable outright when `-e` names it, so a
        // prepend with no caller-supplied value must start from the image's own.
        let mut image_environment: Option<HashMap<String, String>> = None;
        for tool in &opts.tools {
            for asset in &tool.assets {
                let mode = match asset.access {
                    EnvironmentToolAssetAccess::ReadOnly => ProvisionedMountMode::Ro,
                    EnvironmentToolAssetAccess::SharedWritable => ProvisionedMountMode::Rw,
                };
                let (host_path, environment_path) = match asset.kind {
                    EnvironmentToolAssetKind::UnixSocket | EnvironmentToolAssetKind::File | EnvironmentToolAssetKind::Directory => {
                        (asset.host_path.as_path().to_path_buf(), asset.environment_path.as_path().to_path_buf())
                    }
                };
                if let Some(mount) = requested_mounts.iter().chain(&provisioned_mounts).find(|mount| {
                    environment_path.starts_with(mount.environment_path.as_path())
                        || mount.environment_path.as_path().starts_with(&environment_path)
                }) {
                    return Err(format!("mount target {} is reserved for {}", mount.environment_path, asset.purpose));
                }
                provisioned_mounts.push(ProvisionedMount::new(host_path, environment_path, mode));
            }
            for update in &tool.environment {
                match update {
                    EnvironmentVariableUpdate::Set { name, value, purpose } => {
                        if tokens.iter().any(|(existing, _)| existing == name) {
                            return Err(format!("environment variable {name} is reserved for {purpose}"));
                        }
                        tokens.push((name.clone(), value.clone()));
                    }
                    EnvironmentVariableUpdate::PrependPath { name, value } => {
                        match tokens.iter_mut().find(|(existing, _)| existing == name) {
                            Some((_, existing)) => *existing = format!("{value}:{existing}"),
                            None => {
                                if image_environment.is_none() {
                                    image_environment = Some(
                                        self.inner
                                            .image_environment(image.as_str(), opts.image_pull_policy, docker_config.as_deref())
                                            .await?,
                                    );
                                    // The inspected image is now local; run exactly that one.
                                    pull_policy = ImagePullPolicy::Never.docker_value();
                                }
                                let base = image_environment
                                    .as_ref()
                                    .and_then(|environment| environment.get(name))
                                    .map(String::as_str)
                                    .or_else(|| (name == "PATH").then_some(DOCKER_DEFAULT_PATH))
                                    // An image that sets the variable to empty gets the bare
                                    // value, not a trailing `:` (which would mean the cwd).
                                    .filter(|base| !base.is_empty());
                                let combined = match base {
                                    Some(base) => format!("{value}:{base}"),
                                    None => value.clone(),
                                };
                                tokens.push((name.clone(), combined));
                            }
                        }
                    }
                }
            }
        }
        provisioned_mounts.extend(requested_mounts);
        let env_id_str = id.to_string();
        let image_str = runtime_image.as_str().to_string();
        let source_label = format!("flotilla.source_image={}", image.as_str());
        let label_val = format!("flotilla.environment={}", id);
        let mounts_label_val =
            format!("flotilla.provisioned_mounts={}", serde_json::to_string(&provisioned_mounts).map_err(|err| err.to_string())?);
        let env_id_env = format!("FLOTILLA_ENVIRONMENT_ID={}", env_id_str);
        #[cfg(unix)]
        let user = host_user();

        let mut args = Vec::new();
        if let Some(config) = &docker_config {
            args.extend(["--config", config.as_str()]);
        }
        args.extend([
            "run",
            "-d",
            "--init",
            "--pull",
            pull_policy,
            "--name",
            &container_name,
            "--label",
            &label_val,
            "--label",
            &mounts_label_val,
            "--label",
            &source_label,
            "-e",
            &env_id_env,
        ]);
        let cpu_shares = CREW_CPU_SHARES.to_string();
        args.extend(["--memory", memory.as_str(), "--memory-swap", memory_swap.as_str(), "--cpu-shares", &cpu_shares]);
        let cpu_limit = opts.cpu_limit.map(|limit| limit.to_string());
        if let Some(limit) = cpu_limit.as_deref() {
            args.extend(["--cpus", limit]);
        }
        #[cfg(unix)]
        args.extend(["--user", user.as_str()]);

        let mount_specs: Vec<(String, String)> = provisioned_mounts
            .iter()
            .map(|mount| {
                if protected_git_mount(mount) {
                    // --mount rejects a missing bind source. -v silently creates
                    // a directory, which would leave Git metadata unprotected.
                    if mount.host_path.to_string().contains(',') {
                        return Err(format!("protected Git mount path contains a comma: {}", mount.host_path));
                    }
                    return Ok((
                        "--mount".to_string(),
                        format!("type=bind,source={},target={},readonly", mount.host_path, mount.environment_path),
                    ));
                }
                let mode = match mount.mode {
                    ProvisionedMountMode::Ro => "ro",
                    ProvisionedMountMode::Rw => "rw",
                };
                Ok(("-v".to_string(), format!("{}:{}:{mode}", mount.host_path, mount.environment_path)))
            })
            .collect::<Result<_, String>>()?;
        for (flag, mount_spec) in &mount_specs {
            args.push(flag);
            args.push(mount_spec);
        }

        // Token env vars
        let token_env_strs: Vec<String> = tokens.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        for token_env in &token_env_strs {
            args.push("-e");
            args.push(token_env);
        }

        args.push(&image_str);
        args.push("sleep");
        args.push("infinity");

        let backing_id = self.inner.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Default).await?.trim().to_string();
        let local_image_id = match self.inner.local_image_id(&container_name).await {
            Ok(digest) => digest,
            Err(error) => {
                let cleanup = self.inner.destroy(&backing_id).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => format!("{error}; additionally failed to remove container {container_name}: {cleanup_error}"),
                });
            }
        };

        Ok(self.inner.provisioned_environment(id, image.clone(), Some(local_image_id), backing_id, container_name, provisioned_mounts))
    }
}

#[async_trait]
impl super::EnvironmentDetection for DockerEnvironmentProvider {
    async fn detect(
        &self,
        image: Option<&str>,
        binary: &str,
        args: &[&str],
        scratch: &Path,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        let image = image.ok_or("detection requires an image reference")?;
        let mut command = vec!["run", "--rm", "--pull=never", "--workdir", "/probe", "--tmpfs", "/probe", image, binary];
        command.extend_from_slice(args);
        self.inner.runner.run_output("docker", &command, scratch, &ChannelLabel::Default).await
    }
}

impl DockerEnvironmentProvider {
    fn capability_declaration() -> super::ProviderCapabilities {
        super::ProviderCapabilities {
            material_modes: vec![super::MaterialMode::Frozen, super::MaterialMode::Pushed, super::MaterialMode::Shared],
            checkout_strategies: vec![
                super::CheckoutStrategy::HostWorktree,
                super::CheckoutStrategy::HostClone,
                super::CheckoutStrategy::CloneInVessel,
            ],
            workspace_root: "/workspace".into(),
            reference_root: Some("/ref/repo".into()),
            fallback_runner: true,
            receives_host_tools: true,
            per_vessel: true,
            egress_route: super::EgressRoute::Open,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod seam_tests {
    use super::*;
    use crate::environment::{GoneCause, ObservationState, StoppedCause, UnknownReason};

    // ADR 0058: observations are facts, with Docker's existing OOM and exit
    // evidence preserved. The generator spans every captured exit cause and
    // running, paused, terminal, transient and unrecognised runtime statuses.
    #[hegel::test]
    fn docker_observation_causes(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let fixtures = [
            (include_str!("fixtures/cgroup-oomkilled.json"), "", GoneCause::MemoryLimitReached),
            (include_str!("fixtures/oomd-exit137.json"), include_str!("fixtures/oomd-journal.txt"), GoneCause::HostOutOfMemory),
            (include_str!("fixtures/oomd-exit137.json"), "", GoneCause::BackingExited { code: 137 }),
            (include_str!("fixtures/nonzero-exit3.json"), "", GoneCause::BackingExited { code: 3 }),
            (include_str!("fixtures/normal-stop.json"), "", GoneCause::GuestPoweredOff),
        ];
        let (raw, oomd, expected) = &fixtures[tc.draw(gs::integers::<usize>().min_value(0).max_value(fixtures.len() - 1))];
        let inspect = parse_inspection(raw).unwrap();
        let facts = classify_inspection(&inspect, oomd, "");
        let statuses = ["running", "paused", "exited", "dead", "created", "restarting", "removing", "unexpected"];
        let status = statuses[tc.draw(gs::integers::<usize>().min_value(0).max_value(statuses.len() - 1))];
        let actual = classify_provider_state(status, &facts);
        match status {
            "running" => assert_eq!(actual, ObservationState::Running),
            "created" | "restarting" => assert_eq!(actual, ObservationState::Unknown { reason: UnknownReason::GuestUnresponsive }),
            "paused" => assert_eq!(actual, ObservationState::Stopped { cause: StoppedCause::PausedByProvider }),
            "exited" | "dead" => assert_eq!(actual, ObservationState::Gone { cause: expected.clone() }),
            _ => assert!(matches!(actual, ObservationState::Unknown { reason: UnknownReason::ProviderApiError { .. } })),
        }
    }
}

impl DockerEnvironmentProvider {
    async fn prepare_agent_image(&self, original: ImageId) -> Result<super::PreparedEnvironment, String> {
        #[cfg(not(unix))]
        {
            let _ = original;
            Err("Docker vessel agents require a Unix host".into())
        }
        #[cfg(unix)]
        {
            let tools = drydock::launch::build_tools_digest().map_err(|e| e.to_string())?;
            let dockerfile = format!("FROM {tools} AS tools\nFROM {}\nCOPY --from=tools /opt/drydock /opt/drydock\n", original.as_str());
            let output = self
                .inner
                .runner
                .run_with_input(
                    "docker",
                    &["build", "--quiet", "--network=none", "-"],
                    Path::new("/"),
                    &ChannelLabel::Default,
                    dockerfile.as_bytes(),
                )
                .await?;
            let digest = output.lines().last().unwrap_or("").trim();
            if !flotilla_resources::is_image_digest(digest) {
                return Err("preparing drydock tools did not return an image digest".into());
            }
            Ok(super::PreparedEnvironment::new(&self.preparation_owner, DockerPrepared { original, image: ImageId::new(digest) }))
        }
    }
}

#[cfg(unix)]
impl DockerEnvironmentProviderInner {
    fn runtime_directory(&self, id: &EnvironmentId) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.agent_settings.runtime_root).map_err(|e| e.to_string())?;
        drydock_agent::socket::private_directory(&self.agent_settings.runtime_root).map_err(|e| e.to_string())?;
        let directory = self.agent_settings.runtime_root.join(&format!("{:x}", Sha256::digest(id.as_str().as_bytes()))[..24]);
        drydock_agent::socket::private_directory(&directory).map_err(|e| e.to_string())?;
        for child in ["guest", "host"] {
            drydock_agent::socket::private_directory(&directory.join(child)).map_err(|e| e.to_string())?;
        }
        Ok(directory)
    }

    async fn start_agent(runner: &dyn CommandRunner, container: &str) -> Result<(), String> {
        // Upgrade replaces the image's binary by rename. Give the container's
        // configured user its own copy's directory; never write the tools image.
        let user = host_user();
        runner
            .run(
                "docker",
                &[
                    "exec",
                    "--user",
                    "0",
                    container,
                    "sh",
                    "-c",
                    "mkdir -p /opt/flotilla /usr/local/bin /run/flotilla; chown -R \"$1\" /opt/drydock /opt/flotilla /run/flotilla; chown \"$1\" /usr/local/bin",
                    "flotilla-agent-owner",
                    &user,
                ],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await?;
        Self::supervise_agent(runner, container).await
    }
    async fn supervise_agent(runner: &dyn CommandRunner, container: &str) -> Result<(), String> {
        runner
            .run(
                "docker",
                &["exec", container, "/opt/drydock/drydock-agent", "--supervise", "--socket", "/run/drydock/agent.sock"],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await
            .map(|_| ())
    }

    async fn agent_for(&self, container: &str, directory: &Path) -> Result<Arc<AgentLease>, String> {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get(container) {
            return Ok(agent.clone());
        }
        Self::start_agent(self.runner.as_ref(), container).await?;
        let socket = directory.join("guest/agent.sock");
        let connector = match &self.connector {
            Some(factory) => factory(&socket),
            None => Arc::new(UnixAgentConnector {
                socket,
                executable: self.agent_settings.executable.clone(),
                timeout: Duration::from_secs(30),
            }),
        };
        let agent = Arc::new(AgentRunner::new(connector, directory.join("host")));
        super::VesselAgent::ready(&agent).await?;
        let runner = self.runner.clone();
        let backing = container.to_string();
        let supervisor = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                // Exec fails while stopped, then starts a fresh supervisor as
                // soon as Docker starts the container again. The lock prevents
                // duplicate agents and survives an agent re-exec.
                if let Err(error) = Self::supervise_agent(runner.as_ref(), &backing).await {
                    tracing::debug!(container = %backing, %error, "agent supervisor waiting for container");
                }
            }
        });
        let lease = Arc::new(AgentLease { runner: agent, supervisor });
        agents.insert(container.into(), lease.clone());
        Ok(lease)
    }

    #[allow(clippy::too_many_arguments)]
    fn environment_with_agent(
        self: &Arc<Self>,
        id: EnvironmentId,
        image: ImageId,
        local_image_id: Option<String>,
        backing_id: String,
        container_name: String,
        provisioned_mounts: Vec<ProvisionedMount>,
        lease: Arc<AgentLease>,
    ) -> EnvironmentHandle {
        Arc::new(DockerProvisionedEnvironment {
            id,
            backing_id,
            container_name,
            image,
            local_image_id,
            inner: self.clone(),
            runner: lease.runner.clone(),
            agent: Some(Arc::new(lease.runner.clone())),
            provisioned_mounts,
        })
    }
}

#[cfg(all(test, unix))]
mod agent_contract_tests {
    use super::super::agent::tests::MemoryAgent;
    use super::*;
    use flotilla_provider_runtime::CommandOutput;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };

    const BASE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PREPARED: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const BACKING: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    // Process-boundary fake for Docker. Refuse calls outside the provider's
    // lifecycle contract; no Docker executable or engine participates.
    #[derive(Default)]
    struct Engine {
        created: AtomicBool,
        mounts: Mutex<Vec<ProvisionedMount>>,
        calls: Mutex<Vec<Vec<String>>>,
    }
    #[async_trait]
    impl CommandRunner for Engine {
        async fn run(&self, program: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            if program == "cat" && args.first().is_some_and(|path| path.starts_with("/sys/fs/cgroup/")) {
                return Err("no cgroup fixture".into());
            }
            assert_eq!(program, "docker");
            self.calls.lock().unwrap().push(args.iter().map(|s| s.to_string()).collect());
            match args {
                ["image", "inspect", BASE] => Ok("[]".into()),
                ["info", "--format", "{{json .}}"] => Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into()),
                ["run", "-d", "--init", ..] => {
                    assert_eq!(&args[args.len() - 3..], [PREPARED, "sleep", "infinity"]);
                    assert!(!args.contains(&"--entrypoint"));
                    assert!(!args.iter().any(|arg| arg.contains("/opt/drydock/drydock-agent")));
                    let metadata = args.iter().find_map(|arg| arg.strip_prefix("flotilla.provisioned_mounts=")).unwrap();
                    let mounts: Vec<ProvisionedMount> = serde_json::from_str(metadata).unwrap();
                    assert_eq!(mounts.len(), 1, "CLI layer must not be a bind mount");
                    assert_eq!(mounts[0].environment_path.as_path(), Path::new("/run/drydock"));
                    assert_eq!(mounts[0].mode, ProvisionedMountMode::Rw);
                    assert_eq!(
                        std::fs::read_dir(mounts[0].host_path.as_path()).unwrap().count(),
                        0,
                        "the mounted dir contains no host control sockets"
                    );
                    *self.mounts.lock().unwrap() = mounts;
                    assert!(!self.created.swap(true, Ordering::SeqCst));
                    Ok(BACKING.into())
                }
                ["inspect", "--format", "{{.Image}}", _] => Ok(PREPARED.into()),
                ["inspect", "--format", "{{json .Config.Env}}", BACKING] => Ok(r#"["HOME=/tmp","PATH=/usr/bin:/bin"]"#.into()),
                ["exec", "--user", "0", BACKING, "sh", "-c", script, "flotilla-agent-owner", _] => {
                    assert!(script.contains("/opt/drydock"));
                    assert!(script.contains("/opt/flotilla"));
                    Ok(String::new())
                }
                ["exec", BACKING, "/opt/drydock/drydock-agent", "--supervise", "--socket", "/run/drydock/agent.sock"] => Ok(String::new()),
                ["inspect", BACKING] => {
                    let mut value: serde_json::Value = serde_json::from_str(include_str!("fixtures/normal-stop.json")).unwrap();
                    value[0]["Id"] = BACKING.into();
                    value[0]["State"]["Status"] = "running".into();
                    Ok(value.to_string())
                }
                ["ps", ..] => Ok(if self.created.load(Ordering::SeqCst) {
                    format!(
                        "{BACKING} flotilla-env-test\ttest\t{BASE}\t{}\n",
                        serde_json::to_string(&*self.mounts.lock().unwrap()).unwrap()
                    )
                } else {
                    String::new()
                }),
                ["rm", "-f", BACKING] => {
                    self.created.store(false, Ordering::SeqCst);
                    Ok(String::new())
                }
                _ => panic!("unexpected Docker command: {args:?}"),
            }
        }
        async fn run_with_input(&self, program: &str, args: &[&str], _: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
            assert_eq!(program, "docker");
            assert_eq!(args, ["build", "--quiet", "--network=none", "-"]);
            let file = std::str::from_utf8(input).unwrap();
            assert!(file.starts_with("FROM "));
            assert!(file.contains("@sha256:"));
            assert!(file.ends_with(&format!("FROM {BASE}\nCOPY --from=tools /opt/drydock /opt/drydock\n")));
            assert!(!file.contains("ENTRYPOINT"));
            assert!(!file.contains("CMD"));
            Ok(PREPARED.into())
        }
        async fn run_output(&self, p: &str, a: &[&str], c: &Path, l: &ChannelLabel) -> Result<CommandOutput, String> {
            self.run(p, a, c, l).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }

    // Issue 3486: the hello gates provisioning; the returned handle executes
    // through the real agent, and readoption keeps that runner after a roll.
    // The fake checks preparation, fixed alongside startup and per-vessel mounts.
    #[tokio::test]
    async fn prepares_starts_waits_for_hello_pushes_layer_and_readopts() {
        use super::super::{EnvironmentTool, EnvironmentToolAsset};
        let root = tempfile::tempdir().unwrap();
        let assets = tempfile::tempdir().unwrap();
        let source = assets.path().join("source");
        let destination = assets.path().join("guest/flotilla");
        std::fs::write(&source, b"cut-down CLI").unwrap();
        let engine = Arc::new(Engine::default());
        let memory = MemoryAgent::new(true);
        let shared = memory.clone();
        let provider = DockerEnvironmentProvider::new(engine.clone())
            .with_agent_settings(DockerAgentSettings { runtime_root: root.path().into(), executable: source.clone() })
            .with_agent_connector(Arc::new(move |_| shared.clone()));
        let spec = super::super::legacy_environment_spec(&flotilla_protocol::EnvironmentSpec {
            image: flotilla_protocol::ImageSource::Registry(BASE.into()),
            token_env_vars: Vec::new(),
        })
        .unwrap();
        let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
        let tools = vec![EnvironmentTool::new("flotilla", &destination).with_asset(EnvironmentToolAsset::new(
            &source,
            &destination,
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "cut-down CLI",
        ))];
        let handle =
            provider.provision(EnvironmentId::new("test"), &prepared, ProvisionOpts { tools, ..Default::default() }).await.unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"cut-down CLI");
        assert!(handle.agent().is_some());
        assert!(handle.fallback_runner().is_some());
        assert_eq!(handle.image().as_str(), BASE);
        assert_eq!(handle.runner().run("printf", &["agent runner"], Path::new("/"), &ChannelLabel::Default).await.unwrap(), "agent runner");
        let readopted = provider.list().await.unwrap().pop().unwrap();
        assert!(readopted.agent().is_some());
        assert_eq!(readopted.runner().run("printf", &["readopted"], Path::new("/"), &ChannelLabel::Default).await.unwrap(), "readopted");
        assert_eq!(memory.upgrades.load(Ordering::SeqCst), 1);
        handle.destroy().await.unwrap();
        assert!(!engine.created.load(Ordering::SeqCst));
    }
}

#[cfg(all(test, unix))]
mod runtime_path_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    // Hashing environment identities prevents traversal and gives disjoint,
    // private guest directories. Only one guest dir is ever mounted, never its
    // host sibling, another guest's directory or the host control directory.
    #[test]
    fn runtime_paths_are_private_disjoint_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let provider = DockerEnvironmentProvider::new(Arc::new(flotilla_provider_runtime::ProcessCommandRunner))
            .with_agent_settings(DockerAgentSettings { runtime_root: root.path().into(), executable: root.path().join("agent") });
        let a = provider.inner.runtime_directory(&EnvironmentId::new("vessel-a")).unwrap();
        for id in ["vessel-b", "../../vessel-a", "", "a/../vessel-a"] {
            let b = provider.inner.runtime_directory(&EnvironmentId::new(id)).unwrap();
            assert_ne!(a, b);
            assert_eq!(b.parent(), Some(root.path()));
            for child in ["guest", "host"] {
                assert_eq!(std::fs::metadata(b.join(child)).unwrap().permissions().mode() & 0o777, 0o700);
            }
            assert!(b.join("guest/agent.sock").as_os_str().len() < 108);
        }
        assert!(!a.join("guest/host").exists());
    }

    // Glue: the dead-agent diagnosis path retains an interactive tty and cwd,
    // and tells the viewer explicitly that it is using Docker's fallback.
    #[test]
    fn fallback_shell_is_interactive_and_reports_the_fallback() {
        use flotilla_protocol::{arg::Arg, ResolvedAttachAction};
        let provider = DockerEnvironmentProvider::new(Arc::new(flotilla_provider_runtime::ProcessCommandRunner));
        let handle = provider.inner.provisioned_environment(
            EnvironmentId::new("shell"),
            ImageId::new("image"),
            None,
            "backing".into(),
            "container".into(),
            Vec::new(),
        );
        let plan = handle.fallback_shell_plan(Path::new("/workspace with spaces")).unwrap();
        let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else {
            panic!("one hop");
        };
        assert!(args.contains(&Arg::Literal("-it".into())));
        assert!(args.contains(&Arg::Quoted("/workspace with spaces".into())));
        assert!(args.contains(&Arg::Quoted("container".into())));
        assert!(args
            .iter()
            .any(|arg| matches!(arg, Arg::Quoted(text) if text.contains("agent unavailable") && text.contains("fallback runner"))));
    }
}
