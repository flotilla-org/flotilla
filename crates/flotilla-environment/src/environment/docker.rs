//! Docker-backed environment provider.
//!
//! Shells out to the `docker` CLI via `CommandRunner`, consistent with how every
//! other provider interacts with external tools.

use std::{collections::HashMap, path::Path, sync::Arc};

#[cfg(unix)]
use super::agent::{AgentConnector, AgentRunner, UnixAgentConnector};
#[cfg(unix)]
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, ImageId};

use super::{
    runner::DockerEnvironmentRunner, EnvironmentBacking, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment, ProvisionedMount,
};
#[cfg(any(unix, test))]
use super::{
    CreateOpts, EnvironmentToolAssetAccess, EnvironmentToolAssetKind, EnvironmentVariableUpdate, ImagePullPolicy, ProvisionedMountMode,
};
use crate::environment::{PrepareOpts, ProvisionOpts};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;

/// Equal relative CPU scheduling weight for Docker crew environments.
/// Docker translates shares to the native cgroup weight when using cgroup v2.
pub const CREW_CPU_SHARES: usize = 1024;

// Absolute system utility selection, shared with the process-boundary test.
#[cfg(unix)]
const ROOT_UTILITY_SELECTION: &str = concat!(
    "PATH=/usr/sbin:/usr/bin:/sbin:/bin;\n",
    "export PATH;\n",
    "mkdir=/bin/mkdir;\n",
    "[ -x \"$mkdir\" ] || mkdir=/usr/bin/mkdir;\n",
    "chown=/bin/chown;\n",
    "[ -x \"$chown\" ] || chown=/usr/bin/chown;\n",
    "chmod=/bin/chmod;\n",
    "[ -x \"$chmod\" ] || chmod=/usr/bin/chmod;\n",
);

/// Image trees root startup gives the crew user recursively: the agent replaces
/// its binary under `/opt/drydock` by rename and pushes tools into `/opt/flotilla`.
/// A recursive chown would cross into anything mounted inside them, failing on a
/// read-only mount and re-owning host files through a read-write one, so
/// provisioning refuses mounts at, above or below these trees. Mounted material
/// (skills under `/run/flotilla`, sockets, homes) has its ownership set host-side.
#[cfg(unix)]
const CREW_OWNED_TREES: [&str; 2] = ["/opt/drydock", "/opt/flotilla"];

/// The `PATH` Docker gives a container whose image does not set one.
#[cfg(any(unix, test))]
const DOCKER_DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// ---------------------------------------------------------------------------
// DockerEnvironmentProvider
// ---------------------------------------------------------------------------

/// An `EnvironmentProvider` that manages Docker containers as sandbox environments.
pub struct DockerEnvironmentProvider {
    pub(super) inner: Arc<DockerEnvironmentProviderInner>,
    #[cfg(unix)]
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
        args.extend(["pull", "--platform", "linux/amd64", image]);
        self.inner
            .runner
            .run("docker", &args, Path::new("/"), &ChannelLabel::Default)
            .await
            .map(|_| ())
            .map_err(|error| format!("pinned image pull failed: {}", super::RegistryAuth::redact(opts.prepared_auth.as_ref(), error)))
    }

    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            inner: Arc::new(DockerEnvironmentProviderInner::new(runner)),
            #[cfg(unix)]
            preparation_owner: Arc::new(()),
        }
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
/// Repository of the tags naming local crew images for the drydock layer build.
#[cfg(unix)]
const AGENT_IMAGE_BASE_REPOSITORY: &str = "flotilla-agent-base";
#[cfg(unix)]
type ConnectorFactory = dyn Fn(&Path) -> Arc<dyn AgentConnector> + Send + Sync;
#[cfg(unix)]
struct AgentLease {
    directory: PathBuf,
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
    /// Configure before sharing this provider. Panics if another Arc already owns its inner state.
    pub fn with_agent_settings(mut self, settings: DockerAgentSettings) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure before sharing provider").agent_settings = settings;
        self
    }
    /// Injectable process/transport seam. Docker command tests still run the
    /// production preparation and supervisor commands, then dial a real Agent.
    /// Configure before sharing; panics if another Arc owns the inner state.
    pub fn with_agent_connector(mut self, connector: Arc<ConnectorFactory>) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure before sharing provider").connector = Some(connector);
        self
    }
}
#[cfg(unix)]
struct DockerPrepared {
    platform: drydock_protocol::Platform,
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
        #[cfg(all(unix, not(target_os = "linux")))]
        if self.inner.connector.is_none() {
            return Err("guest platform linux/amd64: Docker requires a Linux daemon-adjacent socket transport".into());
        }
        #[cfg(unix)]
        if opts.platform.as_ref().is_some_and(|platform| platform != &crate::guest_tools::linux_amd64()) {
            return Err(format!(
                "unsupported guest platform {}; Docker admits linux/amd64",
                opts.platform.as_ref().expect("requested platform")
            ));
        }
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
    ) -> Result<EnvironmentHandle, super::EnvironmentProvisionError> {
        #[cfg(not(unix))]
        {
            let _ = (id, opts, prepared);
            Err("Docker vessel agents require a Unix host".into())
        }
        #[cfg(unix)]
        {
            let prepared = prepared.get::<DockerPrepared>(&self.preparation_owner)?;
            if prepared.platform != crate::guest_tools::linux_amd64() {
                return Err(format!("unsupported guest platform {}", prepared.platform).into());
            }
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
            let mounted_assets = tools.iter().flat_map(|tool| &tool.assets).map(|asset| asset.environment_path.as_path());
            for target in mounts.iter().map(|mount| mount.environment_path.as_path()).chain(mounted_assets) {
                if let Some(tree) = CREW_OWNED_TREES.iter().find(|tree| target.starts_with(tree) || Path::new(tree).starts_with(target)) {
                    return Err(
                        format!("mount target {} overlaps {tree}, which vessel startup gives to the crew user", target.display()).into()
                    );
                }
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
            result.map_err(super::EnvironmentProvisionError::Failed)
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
        #[cfg(unix)]
        let mut adoption = tokio::task::JoinSet::new();
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
                let directory = mount.host_path.as_path().parent().map(Path::to_path_buf);
                let inner = self.inner.clone();
                let (backing, name, id, base, mounts, index) =
                    (backing_id.clone(), container_name.clone(), env_id.clone(), image.clone(), provisioned_mounts.clone(), handles.len());
                adoption.spawn(async move {
                    let result = async {
                        let directory = directory.ok_or("invalid agent mount")?;
                        if !matches!(provider_observation(inner.runner.as_ref(), &backing).await?.state, super::ObservationState::Running) {
                            return Ok(None);
                        }
                        let lease = inner.agent_for(&backing, &directory).await?;
                        lease.runner.configure_environment(inner.env_vars(&backing).await?.into_iter().collect()).await?;
                        Ok::<_, String>(Some(inner.environment_with_agent(
                            EnvironmentId::new(id),
                            ImageId::new(base),
                            None,
                            backing,
                            name,
                            mounts,
                            lease,
                        )))
                    }
                    .await;
                    (index, result)
                });
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

        #[cfg(unix)]
        while let Some(result) = adoption.join_next().await {
            match result {
                Ok((index, Ok(Some(handle)))) => handles[index] = handle,
                Ok((_, Ok(None))) => {}
                Ok((_, Err(error))) if error.starts_with("guest platform ") => return Err(error),
                Ok((index, Err(error))) => {
                    tracing::warn!(container = %handles[index].backing_id(), %error, "agent adoption failed; retaining Docker fallback")
                }
                Err(error) => tracing::warn!(%error, "agent adoption task failed; retaining Docker fallback"),
            }
        }
        #[cfg(unix)]
        self.inner.agents.lock().await.retain(|backing, cell| {
            let present = handles.iter().any(|handle| handle.backing_id() == backing);
            if !present {
                if let Some(lease) = cell.get() {
                    lease.supervisor.abort();
                }
            }
            present
        });
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

#[cfg(any(unix, test))]
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
    agent_bytes: Arc<tokio::sync::OnceCell<Vec<u8>>>,
    #[cfg(unix)]
    agents: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::OnceCell<Arc<AgentLease>>>>>,
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
            #[cfg(unix)]
            agent_bytes: Default::default(),
        }
    }

    #[cfg(unix)]
    async fn verify_image_platform(&self, image: &str) -> Result<(), String> {
        let platform = self
            .runner
            .run("docker", &["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", image], Path::new("/"), &ChannelLabel::Default)
            .await?;
        if platform.trim() != "linux/amd64" {
            return Err(format!("guest platform linux/amd64 required; image {image} is {}", platform.trim()));
        }
        Ok(())
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
    #[cfg(any(unix, test))]
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

    #[cfg(any(unix, test))]
    async fn inspect_environment(&self, image: &str) -> Result<String, String> {
        self.runner
            .run("docker", &["image", "inspect", "--format", "{{json .Config.Env}}", image], Path::new("/"), &ChannelLabel::Default)
            .await
    }

    #[cfg(any(unix, test))]
    async fn pull(&self, image: &str, docker_config: Option<&str>) -> Result<(), String> {
        let mut args = Vec::new();
        if let Some(config) = docker_config {
            args.extend(["--config", config]);
        }
        args.extend(["pull", "--platform", "linux/amd64", image]);
        self.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
    }

    #[cfg(any(unix, test))]
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
        self.runner.run("docker", &["rm", "-f", container_name], Path::new("/"), &ChannelLabel::Default).await?;
        #[cfg(unix)]
        let removed = self.agents.lock().await.remove(container_name);
        #[cfg(unix)]
        if let Some(cell) = removed {
            if let Some(lease) = cell.get() {
                lease.supervisor.abort();
                self.remove_agent_directory(&lease.directory).await;
            }
        }
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
    fn agent(&self) -> Arc<dyn super::VesselAgent> {
        self.agent.clone().unwrap_or_else(|| {
            Arc::new(super::vessel_ops::UnavailableAgent(format!("container {} has no live vessel agent", self.container_name)))
        })
    }
    fn terminal_route(&self) -> Option<&dyn super::ProviderTerminalRoute> {
        Some(self)
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

impl super::ProviderTerminalRoute for DockerProvisionedEnvironment {
    fn terminal_runner(&self) -> Arc<dyn CommandRunner> {
        // Immutable runtime identity, never a container name that can be reused.
        Arc::new(DockerEnvironmentRunner::new(self.backing_id.clone(), self.inner.runner.clone()))
    }

    fn attach_plan(
        &self,
        endpoint: &flotilla_protocol::result_set::CleatEndpoint,
        client: &Path,
        seat: flotilla_protocol::commands::AttachMode,
    ) -> Result<flotilla_protocol::ResolvedAttachPlan, String> {
        use flotilla_protocol::{arg::Arg, commands::AttachMode, ResolvedAttachPlan};
        let socket = Path::new(&endpoint.runtime_root).join(&endpoint.daemon).join("socket");
        let mut args = vec![
            Arg::Literal("docker".into()),
            Arg::Literal("exec".into()),
            Arg::Literal("-it".into()),
            Arg::Quoted(self.backing_id.clone()),
            Arg::Quoted(client.to_string_lossy().into_owned()),
            Arg::Literal("attach".into()),
            Arg::Literal("--socket".into()),
            Arg::Quoted(socket.to_string_lossy().into_owned()),
            Arg::Literal("--no-create".into()),
        ];
        match seat {
            AttachMode::Default => {}
            AttachMode::Strict => args.push(Arg::Literal("--strict".into())),
            AttachMode::PreferTake | AttachMode::Take => args.push(Arg::Literal("--take".into())),
        }
        args.push(Arg::Quoted(endpoint.session.clone()));
        Ok(ResolvedAttachPlan::command(args))
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
#[cfg(any(unix, test))]
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
    #[cfg(any(unix, test))]
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
            "--platform",
            "linux/amd64",
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
        context: &flotilla_provider_runtime::execution_context::ObservationContext,
        binary: &str,
        args: &[&str],
        scratch: &Path,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        let image = image.ok_or("detection requires an image reference")?;
        // An image declares its own base layer; the probe renders it under the
        // same isolation as any other target.
        let context = context.with_base(self.image_base(image, context.accounts()).await?);
        let name = format!("flotilla-observation-{}", uuid::Uuid::new_v4());
        let mut command = [
            "run",
            "--rm",
            "--name",
            &name,
            "--pull=never",
            "--network=none",
            "--workdir",
            "/",
            "--tmpfs",
            "/tmp",
            "--entrypoint",
            "env",
            image,
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        command.extend(super::detection::observation_args("/tmp/flotilla-observation", &context, binary, args));
        super::detection::run_with_cleanup(
            self.inner.runner.clone(),
            "docker",
            command,
            scratch.into(),
            "docker",
            vec!["rm".into(), "--force".into(), name],
            false,
        )
        .await
    }
}

impl DockerEnvironmentProvider {
    /// The environment the local image's configuration declares. Detection
    /// never pulls.
    async fn image_base(
        &self,
        image: &str,
        accounts: &flotilla_provider_runtime::execution_context::AccountVariables,
    ) -> Result<flotilla_provider_runtime::execution_context::BaseLayer, String> {
        let output = self
            .inner
            .runner
            .run_output("docker", &["image", "inspect", "--format", "{{json .Config.Env}}", image], Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("inspect image {image} environment: {error}"))?;
        if !output.success() {
            return Err(format!("inspect image {image} environment exited {:?}", output.exit_code));
        }
        let entries: Option<Vec<String>> = serde_json::from_str(output.stdout.trim())
            .map_err(|error| format!("docker returned invalid environment for image {image}: {error}"))?;
        let environment = entries
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| entry.split_once('=').map(|(name, value)| (name.to_string(), value.to_string())));
        flotilla_provider_runtime::execution_context::BaseLayer::declared(environment, accounts)
            .map_err(|error| format!("image {image}: {error}"))
    }

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
            let platform = crate::guest_tools::linux_amd64();
            if self.inner.connector.is_none() {
                crate::guest_tools::GuestTools::installed(&platform)?;
            }
            self.inner.verify_image_platform(original.as_str()).await?;
            let tools = drydock::launch::build_tools_digest().map_err(|e| e.to_string())?;
            let base = self.agent_image_base(&original).await?;
            let dockerfile = format!("FROM --platform=linux/amd64 {tools} AS tools\nFROM --platform=linux/amd64 {base}\nCOPY --from=tools /opt/drydock /opt/drydock\n");
            let output = self
                .inner
                .runner
                .run_with_input(
                    "docker",
                    &["build", "--platform", "linux/amd64", "--quiet", "--network=none", "-"],
                    Path::new("/"),
                    &ChannelLabel::Default,
                    dockerfile.as_bytes(),
                )
                .await?;
            let digest = output.lines().last().unwrap_or("").trim();
            if !flotilla_resources::is_image_digest(digest) {
                return Err("preparing drydock tools did not return an image digest".into());
            }
            Ok(super::PreparedEnvironment::new(&self.preparation_owner, DockerPrepared { original, image: ImageId::new(digest), platform }))
        }
    }

    /// BuildKit reads `FROM sha256:<id>` as Docker Hub's `sha256` repository, so
    /// a bare local image ID is first named by a tag derived from that ID.
    /// Registry references are built from unchanged.
    #[cfg(unix)]
    async fn agent_image_base(&self, original: &ImageId) -> Result<String, String> {
        let Some(hex) = original.as_str().strip_prefix("sha256:").filter(|_| flotilla_resources::is_image_digest(original.as_str())) else {
            return Ok(original.as_str().to_string());
        };
        // The tag stays after the build: removing it could drop the image's
        // last name, and concurrent preparations of one image share the tag.
        let tag = format!("{AGENT_IMAGE_BASE_REPOSITORY}:{hex}");
        self.inner.runner.run("docker", &["tag", original.as_str(), &tag], Path::new("/"), &ChannelLabel::Default).await?;
        Ok(tag)
    }
}

#[cfg(unix)]
impl DockerEnvironmentProviderInner {
    async fn remove_agent_directory(&self, directory: &Path) {
        // Mounted labels can describe legacy/external paths. Remove only the
        // hashed child owned by this provider, never an arbitrary adopted path.
        if directory.parent() != Some(self.agent_settings.runtime_root.as_path())
            || !directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.len() == 24 && name.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            tracing::warn!(path = %directory.display(), "retaining external agent runtime directory");
            return;
        }
        if let Err(error) = tokio::fs::remove_dir_all(directory).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %directory.display(), %error, "failed to remove agent runtime directory");
            }
        }
    }

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
        // Root exec overrides the crew PATH and repairs legacy PATH directory
        // ownership before publishing tools into the separate crew-owned prefix.
        // The repair list must cover DOCKER_DEFAULT_PATH. The script's narrower
        // PATH excludes legacy crew-owned /usr/local directories during repair.
        // Repair directories only: recursively chowning legacy /usr/local files
        // cannot make crew-written contents trustworthy. Root execs must keep
        // using absolute system programs; replace the vessel from a clean image
        // to discard any previously planted binaries.
        // Select utilities only from absolute system paths, supporting split-/usr
        // images as well as /bin utilities and merged-/usr images.
        // Only CREW_OWNED_TREES are chowned, and provisioning keeps mounts out
        // of them. Nothing under /run/flotilla is crew-written in a Docker
        // vessel: it holds only host-owned mounts such as read-only skills.
        let root_script = format!(
            "{ROOT_UTILITY_SELECTION}{}\"$chown\" -R \"$1\" {}",
            concat!(
                "\"$mkdir\" -p /usr/local/sbin /usr/local/bin /usr/sbin /usr/bin /sbin /bin /opt/flotilla/crew-bin && ",
                "\"$chown\" 0:0 /usr/local/sbin /usr/local/bin /usr/sbin /usr/bin /sbin /bin && ",
                "\"$chmod\" go-w /usr/local/sbin /usr/local/bin /usr/sbin /usr/bin /sbin /bin && ",
            ),
            CREW_OWNED_TREES.join(" "),
        );
        let root_path = format!("PATH={DOCKER_DEFAULT_PATH}");
        let user = host_user();
        runner
            .run(
                "docker",
                &[
                    "exec",
                    "--user",
                    "0",
                    "--env",
                    &root_path,
                    "--env",
                    "LD_LIBRARY_PATH=",
                    "--env",
                    "LD_PRELOAD=",
                    container,
                    "/bin/sh",
                    "-c",
                    &root_script,
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

    async fn agent_for(self: &Arc<Self>, container: &str, directory: &Path) -> Result<Arc<AgentLease>, String> {
        let cell = self.agents.lock().await.entry(container.into()).or_default().clone();
        let lease = cell
            .get_or_try_init(|| async {
                let executable = if self.connector.is_none() {
                    let tools = crate::guest_tools::GuestTools::installed(&crate::guest_tools::linux_amd64())?;
                    let image = self
                        .runner
                        .run("docker", &["inspect", "--format", "{{.Image}}", container], Path::new("/"), &ChannelLabel::Default)
                        .await?;
                    self.verify_image_platform(image.trim()).await?;
                    tools.agent()
                } else {
                    self.agent_settings.executable.clone()
                };
                Self::start_agent(self.runner.as_ref(), container).await?;
                let socket = directory.join("guest/agent.sock");
                let connector = match &self.connector {
                    Some(factory) => factory(&socket),
                    None => Arc::new(
                        UnixAgentConnector::new(socket, executable, crate::guest_tools::linux_amd64(), Duration::from_secs(30))
                            .with_executable_cache(self.agent_bytes.clone()),
                    ),
                };
                let agent = Arc::new(AgentRunner::new(connector, directory.join("host")));
                super::VesselAgent::ready(&agent).await?;
                let (inner, backing, watched) = (Arc::downgrade(self), container.to_string(), agent.clone());
                let supervisor = tokio::spawn(async move {
                    let mut delay = Duration::from_secs(2);
                    loop {
                        tokio::time::sleep(delay).await;
                        if watched.healthy().await {
                            delay = Duration::from_secs(2);
                            continue;
                        }
                        let Some(inner) = inner.upgrade() else { break };
                        // A successful empty inventory is authoritative removal;
                        // provider API failures instead back off and remain retryable.
                        match inner
                            .runner
                            .run(
                                "docker",
                                &["ps", "-a", "--no-trunc", "--filter", &format!("id={backing}"), "--format", "{{.ID}}"],
                                Path::new("/"),
                                &ChannelLabel::Default,
                            )
                            .await
                        {
                            Ok(ids) if ids.trim().is_empty() => {
                                inner.agents.lock().await.remove(&backing);
                                break;
                            }
                            Ok(_) => {
                                if Self::supervise_agent(inner.runner.as_ref(), &backing).await.is_ok() {
                                    let _ = super::VesselAgent::ready(&watched).await;
                                }
                            }
                            Err(error) => tracing::debug!(%backing, %error, "agent supervision waiting for provider"),
                        }
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                });
                Ok::<_, String>(Arc::new(AgentLease { directory: directory.into(), runner: agent, supervisor }))
            })
            .await?;
        Ok(lease.clone())
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
    use super::super::{agent::tests::MemoryAgent, contract_tests::LocalImageTags};
    use super::super::{EnvironmentTool, EnvironmentToolAsset};
    use super::*;
    use flotilla_provider_runtime::CommandOutput;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };

    const BASE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PREPARED: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TAGGED_BASE: &str = "flotilla-agent-base:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BACKING: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    // Process-boundary fake for Docker. Refuse calls outside the provider's
    // lifecycle contract; no Docker executable or engine participates.
    #[derive(Default)]
    struct Engine {
        created: AtomicBool,
        mounts: Mutex<Vec<ProvisionedMount>>,
        calls: Mutex<Vec<Vec<String>>>,
        tags: LocalImageTags,
        // Mounts the vessel requests ahead of the agent's own /run/drydock.
        requested: Vec<ProvisionedMount>,
        root_script: Mutex<Option<String>>,
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
                ["tag", id, tag] => self.tags.tag(id, tag),
                ["info", "--format", "{{json .}}"] => Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into()),
                ["run", "-d", "--init", ..] => {
                    assert_eq!(&args[args.len() - 3..], [PREPARED, "sleep", "infinity"]);
                    assert!(!args.contains(&"--entrypoint"));
                    assert!(!args.iter().any(|arg| arg.contains("/opt/drydock/drydock-agent")));
                    let metadata = args.iter().find_map(|arg| arg.strip_prefix("flotilla.provisioned_mounts=")).unwrap();
                    let mounts: Vec<ProvisionedMount> = serde_json::from_str(metadata).unwrap();
                    let (agent, requested) = mounts.split_last().unwrap();
                    assert_eq!(requested, self.requested.as_slice(), "CLI layer must not be a bind mount");
                    assert_eq!(agent.environment_path.as_path(), Path::new("/run/drydock"));
                    assert_eq!(agent.mode, ProvisionedMountMode::Rw);
                    assert_eq!(
                        std::fs::read_dir(agent.host_path.as_path()).unwrap().count(),
                        0,
                        "the mounted dir contains no host control sockets"
                    );
                    *self.mounts.lock().unwrap() = mounts;
                    assert!(!self.created.swap(true, Ordering::SeqCst));
                    Ok(BACKING.into())
                }
                ["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", _] => Ok("linux/amd64".into()),
                ["inspect", "--format", "{{.Image}}", _] => Ok(PREPARED.into()),
                ["inspect", "--format", "{{json .Config.Env}}", BACKING] => Ok(r#"["HOME=/tmp","PATH=/usr/bin:/bin"]"#.into()),
                ["exec", "--user", "0", "--env", path, "--env", "LD_LIBRARY_PATH=", "--env", "LD_PRELOAD=", BACKING, "/bin/sh", "-c", script, "flotilla-agent-owner", _] =>
                {
                    assert_eq!(*path, format!("PATH={DOCKER_DEFAULT_PATH}"));
                    assert!(script.contains("/opt/drydock"));
                    assert!(script.contains("/opt/flotilla"));
                    *self.root_script.lock().unwrap() = Some(script.to_string());
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
            assert_eq!(args, ["build", "--platform", "linux/amd64", "--quiet", "--network=none", "-"]);
            let file = std::str::from_utf8(input).unwrap();
            assert!(file.starts_with("FROM "));
            assert!(file.contains("@sha256:"));
            assert!(file.ends_with(&format!("FROM --platform=linux/amd64 {TAGGED_BASE}\nCOPY --from=tools /opt/drydock /opt/drydock\n")));
            assert!(!file.contains("ENTRYPOINT"));
            assert!(!file.contains("CMD"));
            assert_eq!(self.tags.resolve_build(file)?.as_deref(), Some(BASE));
            Ok(PREPARED.into())
        }
        async fn run_output(&self, p: &str, a: &[&str], c: &Path, l: &ChannelLabel) -> Result<CommandOutput, String> {
            self.run(p, a, c, l).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }

    // Root startup must use absolute programs and must leave every root PATH
    // directory owned by root. This process-boundary fake inspects real argv.
    #[tokio::test]
    async fn root_startup_keeps_path_directories_root_owned() {
        struct RootContract(AtomicBool);
        #[async_trait]
        impl CommandRunner for RootContract {
            async fn run(&self, _: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
                if args.get(1) == Some(&"--user") {
                    self.0.store(true, Ordering::SeqCst);
                    for value in [format!("PATH={DOCKER_DEFAULT_PATH}"), "LD_LIBRARY_PATH=".into(), "LD_PRELOAD=".into()] {
                        assert!(args.windows(2).any(|pair| pair == ["--env", value.as_str()]));
                    }
                    let shell = args.iter().position(|arg| *arg == "/bin/sh").expect("absolute root shell");
                    assert_eq!(args[shell + 1], "-c");
                    let script = args[shell + 2];
                    assert!(script.starts_with(ROOT_UTILITY_SELECTION));
                    assert!(!script.contains("chown \"$1\" /usr/local/bin"));
                    assert!(script.contains("/opt/flotilla/crew-bin"));
                    // Execute only the unprivileged selection prologue against fake
                    // system directories. Exhaust /bin-only, /usr/bin-only, both,
                    // and neither: no utility is ever resolved through PATH.
                    use std::os::unix::fs::PermissionsExt;
                    for (primary, fallback) in [(true, false), (false, true), (true, true), (false, false)] {
                        let root = tempfile::tempdir().unwrap();
                        for (name, present) in [("primary", primary), ("fallback", fallback)] {
                            let directory = root.path().join(name);
                            std::fs::create_dir(&directory).unwrap();
                            if present {
                                for utility in ["mkdir", "chown", "chmod"] {
                                    let path = directory.join(utility);
                                    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
                                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
                                }
                            }
                        }
                        // Replace the longer /usr/bin/ prefix first: replacing
                        // /bin/ first would also rewrite that prefix's suffix.
                        let prologue = ROOT_UTILITY_SELECTION
                            .replace("/usr/bin/", &format!("{}/fallback/", root.path().display()))
                            .replace("/bin/", &format!("{}/primary/", root.path().display()));
                        let output = std::process::Command::new("/bin/sh")
                            .args(["-c", &format!("{prologue}printf '%s\\n' \"$mkdir\" \"$chown\" \"$chmod\"; test -x \"$mkdir\"")])
                            .output()
                            .unwrap();
                        assert_eq!(output.status.success(), primary || fallback);
                        let chosen = root.path().join(if primary { "primary" } else { "fallback" });
                        assert_eq!(
                            String::from_utf8(output.stdout).unwrap().lines().collect::<Vec<_>>(),
                            ["mkdir", "chown", "chmod"].map(|utility| chosen.join(utility).display().to_string())
                        );
                    }
                    let root_directories = DOCKER_DEFAULT_PATH.split(':').collect::<Vec<_>>().join(" ");
                    assert!(script.contains(&format!("\"$chown\" 0:0 {root_directories}")));
                    assert!(script.contains(&format!("\"$chmod\" go-w {root_directories}")));
                    let crew_paths = script.rsplit("\"$chown\" -R ").next().unwrap().split_whitespace().skip(1);
                    for path in crew_paths {
                        assert!(!DOCKER_DEFAULT_PATH.split(':').any(|root| Path::new(root).starts_with(path)));
                    }
                }
                Ok(String::new())
            }
            async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
                unreachable!()
            }
            async fn exists(&self, _: &str, _: &[&str]) -> bool {
                false
            }
        }
        let contract = RootContract(AtomicBool::new(false));
        DockerEnvironmentProviderInner::start_agent(&contract, BACKING).await.unwrap();
        assert!(contract.0.load(Ordering::SeqCst), "startup must establish ownership before supervising");
    }

    // Issue 3486: the hello gates provisioning; the returned handle executes
    // through the real agent, and readoption keeps that runner after a roll.
    // The fake checks preparation, fixed alongside startup and per-vessel mounts.
    #[tokio::test]
    async fn prepares_starts_waits_for_hello_pushes_layer_and_readopts() {
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
        assert!(handle.agent().ready().await.is_ok());
        assert!(handle.fallback_runner().is_some());
        assert_eq!(handle.image().as_str(), BASE);
        assert_eq!(handle.runner().run("printf", &["agent runner"], Path::new("/"), &ChannelLabel::Default).await.unwrap(), "agent runner");
        let readopted = provider.list().await.unwrap().pop().unwrap();
        assert!(readopted.agent().ready().await.is_ok());
        assert_eq!(readopted.runner().run("printf", &["readopted"], Path::new("/"), &ChannelLabel::Default).await.unwrap(), "readopted");
        assert_eq!(memory.upgrades.load(Ordering::SeqCst), 1);
        handle.destroy().await.unwrap();
        assert!(!engine.created.load(Ordering::SeqCst));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0, "destroy must remove the per-vessel runtime directory");
    }

    async fn provision_with(engine: &Arc<Engine>, root: &Path, tools: Vec<EnvironmentTool>) -> Result<EnvironmentHandle, String> {
        let executable = root.join("agent");
        std::fs::write(&executable, b"agent").unwrap();
        let memory = MemoryAgent::new(true);
        let provider = DockerEnvironmentProvider::new(engine.clone())
            .with_agent_settings(DockerAgentSettings { runtime_root: root.join("runtime"), executable })
            .with_agent_connector(Arc::new(move |_| memory.clone()));
        let spec = super::super::legacy_environment_spec(&flotilla_protocol::EnvironmentSpec {
            image: flotilla_protocol::ImageSource::Registry(BASE.into()),
            token_env_vars: Vec::new(),
        })
        .unwrap();
        let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
        let opts = ProvisionOpts { tools, provisioned_mounts: engine.requested.clone(), ..Default::default() };
        provider.provision(EnvironmentId::new("test"), &prepared, opts).await.map_err(|error| error.to_string())
    }

    fn overlaps(a: &Path, b: &Path) -> bool {
        a.starts_with(b) || b.starts_with(a)
    }

    // Canary r842: a read-only skills mount under /run/flotilla failed
    // `chown -R`. Startup must never chown at, above or below any path the
    // container mounts, so read-only mounts succeed and host files stay owned.
    #[tokio::test]
    async fn root_startup_never_chowns_a_mounted_path() {
        let root = tempfile::tempdir().unwrap();
        let requested = vec![
            ProvisionedMount::new(root.path().join("skills"), "/run/flotilla/skills", ProvisionedMountMode::Ro),
            ProvisionedMount::new(root.path().join("data"), "/run/flotilla/data", ProvisionedMountMode::Rw),
            ProvisionedMount::new(root.path().join("codex"), "/tmp/flotilla-codex", ProvisionedMountMode::Rw),
            ProvisionedMount::new(root.path().join("cleat"), "/var/lib/flotilla/cleat", ProvisionedMountMode::Rw),
            ProvisionedMount::new(root.path().join("sibling"), "/opt/flotilla-cache", ProvisionedMountMode::Rw),
        ];
        let engine = Arc::new(Engine { requested, ..Default::default() });
        provision_with(&engine, root.path(), Vec::new()).await.unwrap();
        let script = engine.root_script.lock().unwrap().clone().expect("startup ran the root script");
        let recursive = script.rsplit("\"$chown\" -R \"$1\" ").next().unwrap().split_whitespace().map(Path::new).collect::<Vec<_>>();
        for mount in engine.mounts.lock().unwrap().iter() {
            for target in &recursive {
                assert!(
                    !overlaps(target, mount.environment_path.as_path()),
                    "chown -R {} crosses mount {}",
                    target.display(),
                    mount.environment_path
                );
            }
        }
        assert_eq!(recursive, CREW_OWNED_TREES.map(Path::new));
    }

    // Mounts and mounted tool assets inside, at or above a crew-owned tree are
    // refused before any container exists; sibling names are not overlaps.
    #[tokio::test]
    async fn provisioning_refuses_mounts_overlapping_crew_owned_trees() {
        for target in ["/opt/flotilla/cache", "/opt/flotilla", "/opt/drydock/bin", "/opt"] {
            let root = tempfile::tempdir().unwrap();
            let requested = vec![ProvisionedMount::new(root.path().join("m"), target, ProvisionedMountMode::Ro)];
            let engine = Arc::new(Engine { requested, ..Default::default() });
            let error = provision_with(&engine, root.path(), Vec::new()).await.err().expect("overlapping mount refused");
            assert!(error.contains("vessel startup gives to the crew user"), "{error}");
            assert!(!engine.created.load(Ordering::SeqCst));
        }
        for (kind, access) in [
            (EnvironmentToolAssetKind::Directory, EnvironmentToolAssetAccess::SharedWritable),
            (EnvironmentToolAssetKind::UnixSocket, EnvironmentToolAssetAccess::ReadOnly),
        ] {
            let root = tempfile::tempdir().unwrap();
            let state = "/opt/flotilla/state";
            let tools =
                vec![EnvironmentTool::new("state", state).with_asset(EnvironmentToolAsset::new(root.path(), state, kind, access, "state"))];
            let engine = Arc::new(Engine::default());
            let error = provision_with(&engine, root.path(), tools).await.err().expect("mounted asset refused");
            assert!(error.contains("vessel startup gives to the crew user"), "{error}");
            assert!(!engine.created.load(Ordering::SeqCst));
        }
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

    #[tokio::test]
    async fn teardown_removes_owned_runtime_directories_and_preserves_external_mounts() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let provider = DockerEnvironmentProvider::new(Arc::new(flotilla_provider_runtime::ProcessCommandRunner))
            .with_agent_settings(DockerAgentSettings { runtime_root: root.path().into(), executable: root.path().join("agent") });
        let owned = provider.inner.runtime_directory(&EnvironmentId::new("vessel")).unwrap();
        let unowned = root.path().join("legacy");
        std::fs::create_dir(&unowned).unwrap();
        for directory in [&unowned, external.path()] {
            std::fs::write(directory.join("sentinel"), b"preserve").unwrap();
            provider.inner.remove_agent_directory(directory).await;
            assert!(directory.join("sentinel").exists());
        }
        provider.inner.remove_agent_directory(&owned).await;
        assert!(!owned.exists());
        // Retried teardown is idempotent.
        provider.inner.remove_agent_directory(&owned).await;
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

#[cfg(all(test, unix))]
mod adoption_tests {
    use super::super::agent::tests::MemoryAgent;
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    // Fake only Docker's process boundary; agent adoption uses real protocol handlers.
    #[derive(Default)]
    struct Inventory {
        rows: Mutex<Vec<(String, PathBuf)>>,
        starts: AtomicUsize,
    }
    #[async_trait]
    impl CommandRunner for Inventory {
        async fn run(&self, program: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            if program == "cat" {
                return Err("no host cgroup sample".into());
            }
            match args {
                ["ps", "-a", "--no-trunc", "--filter", filter, "--format", "{{.ID}}"] => {
                    let backing = filter.strip_prefix("id=").unwrap();
                    Ok(if self.rows.lock().unwrap().iter().any(|(id, _)| id == backing) { backing.into() } else { String::new() })
                }
                ["ps", ..] => Ok(self
                    .rows
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, guest)| {
                        let mounts = if guest.as_os_str().is_empty() {
                            vec![]
                        } else {
                            vec![ProvisionedMount::new(guest, "/run/drydock", ProvisionedMountMode::Rw)]
                        };
                        format!("{id} {id}\t{id}\tbase\t{}", serde_json::to_string(&mounts).unwrap())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")),
                ["inspect", "--format", "{{json .Config.Env}}", _] => Ok(r#"["HOME=/tmp"]"#.into()),
                ["inspect", _] => {
                    let mut value: serde_json::Value = serde_json::from_str(include_str!("fixtures/normal-stop.json")).unwrap();
                    value[0]["State"]["Status"] = "running".into();
                    Ok(value.to_string())
                }
                ["exec", _, "/opt/drydock/drydock-agent", "--supervise", "--socket", "/run/drydock/agent.sock"] => {
                    self.starts.fetch_add(1, Ordering::SeqCst);
                    Ok(String::new())
                }
                ["exec", "--user", "0", ..] => Ok(String::new()),
                _ => panic!("unexpected Docker command {args:?}"),
            }
        }
        async fn run_output(
            &self,
            _: &str,
            _: &[&str],
            _: &Path,
            _: &ChannelLabel,
        ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
            unreachable!()
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }
    struct Unreachable;
    #[async_trait]
    impl AgentConnector for Unreachable {
        async fn connect(&self) -> Result<drydock_agent::Client, String> {
            Err("agent unreachable".into())
        }
    }

    // One failed agent cannot erase healthy inventory. Healthy monitoring uses
    // the existing client; out-of-band removal retires the watcher/cache even
    // while a caller retains the old handle.
    #[tokio::test]
    async fn unavailable_agent_keeps_inventory_and_removed_container_retires_supervision() {
        let root = tempfile::tempdir().unwrap();
        let engine = Arc::new(Inventory::default());
        let memory = MemoryAgent::new(false);
        let shared = memory.clone();
        let bad = root.path().join("bad/guest/agent.sock");
        for id in ["bad", "healthy"] {
            let directory = root.path().join(id);
            std::fs::create_dir_all(directory.join("guest")).unwrap();
            std::fs::create_dir_all(directory.join("host")).unwrap();
            engine.rows.lock().unwrap().push((id.into(), directory.join("guest")));
        }
        // Recorded and inspected mounts both omit the agent on this older backing.
        engine.rows.lock().unwrap().push(("pre-agent".into(), PathBuf::new()));
        let provider = DockerEnvironmentProvider::new(engine.clone()).with_agent_connector(Arc::new(move |socket| {
            if socket == bad {
                Arc::new(Unreachable)
            } else {
                shared.clone()
            }
        }));
        let handles = provider.list().await.unwrap();
        assert_eq!(handles.len(), 3);
        assert!(handles.iter().find(|h| h.id().as_str() == "bad").unwrap().agent().ready().await.is_err());
        let old = handles.iter().find(|h| h.id().as_str() == "pre-agent").unwrap();
        assert!(old.agent().ready().await.is_err());
        assert!(old.provisioned_mounts().is_empty());
        assert!(old.terminal_route().is_some());
        assert!(handles.iter().find(|h| h.id().as_str() == "bad").unwrap().terminal_route().is_some());
        let healthy = handles.iter().find(|h| h.id().as_str() == "healthy").unwrap();
        assert!(healthy.agent().ready().await.is_ok());
        let starts = engine.starts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(engine.starts.load(Ordering::SeqCst), starts, "healthy agents need no repeated Docker exec");
        engine.rows.lock().unwrap().retain(|(id, _)| id != "healthy");
        memory.stop();
        tokio::time::timeout(Duration::from_secs(8), async {
            while provider.inner.agents.lock().await.contains_key("healthy") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    struct GatedConnector {
        entered: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl AgentConnector for GatedConnector {
        async fn connect(&self) -> Result<drydock_agent::Client, String> {
            self.entered.notify_one();
            std::future::pending().await
        }
    }
    // Initialization is exclusive per backing, not across the provider map.
    #[tokio::test]
    async fn stalled_hello_does_not_serialize_other_vessels() {
        let root = tempfile::tempdir().unwrap();
        for id in ["slow", "healthy"] {
            std::fs::create_dir_all(root.path().join(id).join("host")).unwrap();
        }
        let entered = Arc::new(tokio::sync::Notify::new());
        let gate = entered.clone();
        let memory = MemoryAgent::new(false);
        let provider = DockerEnvironmentProvider::new(Arc::new(Inventory::default())).with_agent_connector(Arc::new(move |socket| {
            if socket.to_string_lossy().contains("slow") {
                Arc::new(GatedConnector { entered: gate.clone() })
            } else {
                memory.clone()
            }
        }));
        let inner = provider.inner.clone();
        let slow_directory = root.path().join("slow");
        let slow = tokio::spawn(async move { inner.agent_for("slow", &slow_directory).await });
        entered.notified().await;
        let healthy = tokio::time::timeout(Duration::from_secs(1), provider.inner.agent_for("healthy", &root.path().join("healthy")))
            .await
            .unwrap()
            .unwrap();
        assert!(healthy.runner.healthy().await);
        slow.abort();
    }
}
