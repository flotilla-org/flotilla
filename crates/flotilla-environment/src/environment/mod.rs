#[cfg(unix)]
pub mod agent;
mod detection;
#[cfg(unix)]
pub mod drydock;
mod seam;
pub use seam::*;
pub mod docker;
pub mod docker_image;
pub mod docker_vessels;
pub mod dummy;
pub mod host_direct;
pub mod image;
pub mod image_dockerfile;
pub mod registry_auth;
pub mod registry_client;
pub mod runner;
pub mod vessel_ops;
pub use image::{ImageBuildService, LocalImageCache, OwnedBuilderSettings};
pub use registry_auth::RegistryAuth;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, ExecutionEnvironmentPath, ImageId};
use serde::{Deserialize, Serialize};

use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::PinUnpack;

/// Options for creating a new provisioned environment.
///
/// Runtime-only — not serializable.
#[derive(Debug, Clone, Default)]
pub struct CreateOpts {
    pub tokens: Vec<(String, String)>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub provisioned_mounts: Vec<ProvisionedMount>,
    /// Tools that must be made available inside the environment.
    ///
    /// Providers choose how to deliver these assets. Docker pushes read-only
    /// files after the agent hello; writable shared material and vessel-specific
    /// daemon endpoints remain mounts.
    pub tools: Vec<EnvironmentTool>,
    pub image_pull_policy: ImagePullPolicy,
    pub prepared_auth: PreparedEnvironmentAuth,
    /// CPU quota for this vessel. None leaves the provider's default.
    pub cpu_limit: Option<usize>,
}

/// Inputs shared by environment provisioning, without image concepts.
#[derive(Debug, Clone, Default)]
pub struct ProvisionOpts {
    pub tokens: Vec<(String, String)>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub provisioned_mounts: Vec<ProvisionedMount>,
    pub tools: Vec<EnvironmentTool>,
    pub cpu_limit: Option<usize>,
}

/// Admitted operation credentials; callers never select an image through these options.
#[derive(Debug, Clone, Default)]
pub struct PrepareOpts {
    /// Requested OCI guest target; omitted selects the provider's admitted default.
    #[cfg(unix)]
    pub platform: Option<drydock_protocol::Platform>,
    pub prepared_auth: PreparedEnvironmentAuth,
}

impl From<CreateOpts> for ProvisionOpts {
    fn from(opts: CreateOpts) -> Self {
        Self {
            tokens: opts.tokens,
            working_directory: opts.working_directory,
            provisioned_mounts: opts.provisioned_mounts,
            tools: opts.tools,
            cpu_limit: opts.cpu_limit,
        }
    }
}

/// Runtime-only opaque admitted pull credentials.
pub type PreparedEnvironmentAuth = Option<RegistryAuth>;

/// A host-side tool that an environment provider must make invokable inside a
/// provisioned environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentTool {
    pub name: String,
    pub executable: ExecutionEnvironmentPath,
    pub assets: Vec<EnvironmentToolAsset>,
    pub environment: Vec<EnvironmentVariableUpdate>,
}

impl EnvironmentTool {
    pub fn new(name: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self { name: name.into(), executable: ExecutionEnvironmentPath::new(executable), assets: Vec::new(), environment: Vec::new() }
    }

    pub fn with_asset(mut self, asset: EnvironmentToolAsset) -> Self {
        self.assets.push(asset);
        self
    }

    pub fn with_environment(mut self, update: EnvironmentVariableUpdate) -> Self {
        self.environment.push(update);
        self
    }
}

/// An asset needed by a tool inside an environment.
///
/// The source is deliberately described as a host path rather than as a bind
/// mount. Bind mounting is one provider's delivery strategy, not the contract.
/// Generation tools deliver stable real directories for `tools`, `bin`, `lib`,
/// and `share/terminfo`, mounted/copied to the matching `/opt/flotilla` children.
/// Relative links from the latter three follow `tools/current`, which selects
/// `tools/<identity>`. Preserve these links and expose atomic replacement to
/// running vessels: Docker binds the real directories; k8s populates a shared
/// volume; sandboxes upload a generation before replacing the current link.
/// ReadOnly describes guest access; the provider owns and rewrites the source.
/// Do not mount resolved generation children (they pin the vessel), or the whole
/// `/opt/flotilla` parent (it would hide image-owned harnesses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentToolAsset {
    pub host_path: DaemonHostPath,
    pub environment_path: ExecutionEnvironmentPath,
    pub kind: EnvironmentToolAssetKind,
    pub access: EnvironmentToolAssetAccess,
    pub purpose: String,
}

impl EnvironmentToolAsset {
    /// Host endpoints are vessel-specific mounts; an agent forward serves only
    /// what a mount cannot, such as loopback TCP. Writable shared material is not
    /// a pushed file.
    pub fn requires_mount(&self) -> bool {
        self.kind == EnvironmentToolAssetKind::UnixSocket
            || self.access == EnvironmentToolAssetAccess::SharedWritable
            || self.environment_path.as_path() == Path::new(CONTAINED_DAEMON_SOCKET_DIRECTORY)
    }

    pub fn new(
        host_path: impl Into<PathBuf>,
        environment_path: impl Into<PathBuf>,
        kind: EnvironmentToolAssetKind,
        access: EnvironmentToolAssetAccess,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            host_path: DaemonHostPath::new(host_path),
            environment_path: ExecutionEnvironmentPath::new(environment_path),
            kind,
            access,
            purpose: purpose.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentToolAssetKind {
    File,
    Directory,
    UnixSocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentToolAssetAccess {
    ReadOnly,
    SharedWritable,
}

/// Stable directory used for the host daemon socket inside contained
/// environments. Provisioning supplies a dedicated directory containing only
/// that environment's socket, so daemon restart can replace the socket inode
/// without exposing the host's administrative run directory.
pub const CONTAINED_DAEMON_SOCKET_DIRECTORY: &str = "/run/flotilla-daemon";
pub const CONTAINED_DAEMON_REQUIRED_ENV: &str = "FLOTILLA_CONTAINED_HOST_DAEMON";

/// Shared name for the host-daemon socket delivered to contained tools.
pub const DAEMON_SOCKET_ENV: &str = "FLOTILLA_DAEMON_SOCKET";

/// The guest socket of a Docker vessel's agent forward to flotillad, beside its
/// loopback listener; the mounted socket stays the CLI's.
pub const CONTAINED_DAEMON_FORWARD_SOCKET: &str = "/run/drydock/flotillad.sock";
/// The loopback address a Docker vessel's agent serves flotillad on, for clients
/// that speak only TCP, such as Claude Code's HTTP hooks.
pub const CONTAINED_DAEMON_FORWARD_TCP: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 15080);

pub fn contained_daemon_socket_path(host_socket_path: &Path) -> PathBuf {
    let file_name = host_socket_path.file_name().expect("daemon socket path must name a socket file");
    Path::new(CONTAINED_DAEMON_SOCKET_DIRECTORY).join(file_name)
}

/// A tool's requested mutation to the environment it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentVariableUpdate {
    Set { name: String, value: String, purpose: String },
    PrependPath { name: String, value: String },
}

impl EnvironmentVariableUpdate {
    pub fn set(name: impl Into<String>, value: impl Into<String>, purpose: impl Into<String>) -> Self {
        Self::Set { name: name.into(), value: value.into(), purpose: purpose.into() }
    }

    pub fn prepend_path(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::PrependPath { name: name.into(), value: value.into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImagePullPolicy {
    Always,
    #[default]
    IfNotPresent,
    Never,
}

impl ImagePullPolicy {
    #[cfg(any(unix, test))]
    fn docker_value(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::IfNotPresent => "missing",
            Self::Never => "never",
        }
    }
}

impl From<flotilla_resources::DockerImagePullPolicy> for ImagePullPolicy {
    fn from(value: flotilla_resources::DockerImagePullPolicy) -> Self {
        match value {
            flotilla_resources::DockerImagePullPolicy::Always => Self::Always,
            flotilla_resources::DockerImagePullPolicy::IfNotPresent => Self::IfNotPresent,
            flotilla_resources::DockerImagePullPolicy::Never => Self::Never,
        }
    }
}

/// Structured metadata for a flotilla-managed bind mount inside a provisioned environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionedMount {
    pub host_path: DaemonHostPath,
    pub environment_path: ExecutionEnvironmentPath,
    #[serde(default)]
    pub mode: ProvisionedMountMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvisionedMountMode {
    #[default]
    Ro,
    Rw,
}

impl ProvisionedMount {
    pub fn new(host_path: impl Into<PathBuf>, environment_path: impl Into<PathBuf>, mode: ProvisionedMountMode) -> Self {
        Self { host_path: DaemonHostPath::new(host_path), environment_path: ExecutionEnvironmentPath::new(environment_path), mode }
    }
}

/// What is at a path, never following a final symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Host-facing capabilities of the one resident vessel agent. Vessel delivery
/// (#3956) runs every step through these general facilities; it never uses a
/// fallback runner. File operations are the agent's own, not scripts a caller
/// composes; running a command is one facility among them, for steps that are
/// genuinely commands.
#[async_trait]
pub trait VesselAgent: Send + Sync {
    async fn ready(&self) -> Result<(), String>;
    async fn forward_socket(&self, guest: &Path) -> Result<PathBuf, String>;
    async fn push_tools(&self, tools: &[EnvironmentTool]) -> Result<(), String>;
    /// Serve the socket `guest` in the vessel, and loopback `tcp` there when given,
    /// carrying each connection to the controller's Unix socket `controller`: the
    /// reverse of [`Self::forward_socket`].
    async fn forward_controller_socket(&self, guest: &Path, tcp: Option<SocketAddr>, controller: &Path) -> Result<(), String> {
        let _ = (tcp, controller);
        Err(format!("this vessel agent cannot serve {} from the controller", guest.display()))
    }
    /// Add `environment` to what every command in the vessel runs with, over
    /// what its provider configured there.
    async fn extend_environment(&self, environment: std::collections::BTreeMap<String, String>) -> Result<(), String> {
        let _ = environment;
        Err("this vessel agent cannot configure its environment".into())
    }
    /// Run commands in the vessel, through the agent.
    fn commands(&self) -> Arc<dyn CommandRunner>;
    /// The base under which this vessel's writable state lives: the contained
    /// base in a container, the host runner's base on a shared host. On a host it
    /// may be a tmpfs, so it holds only what the next launch delivers again.
    async fn writable_base(&self) -> Result<PathBuf, String>;
    /// The durable base harness homes live under (#4012), never a tmpfs: see
    /// [`runner::home_base`].
    async fn home_base(&self) -> Result<PathBuf, String> {
        runner::home_base(&*self.commands()).await
    }
    /// What is at `path`, or `None` when nothing is.
    async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String>;
    /// Create the directory `path`, and any missing parents with the agent's default
    /// directory mode, leaving `path` with exactly `mode`. Existing contents are kept;
    /// anything but a directory at `path`, a symbolic link included, is refused.
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String>;
    /// The regular file at `path`, following links, refused beyond `limit` bytes,
    /// or `None` when there is none.
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String>;
    /// Replace the file at `path` with `bytes` and `mode`, atomically.
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String>;
    /// Make `path` a symbolic link to `target`, atomically replacing anything there
    /// but a directory.
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String>;
    /// Move the entry at `from` to `to` in one `rename(2)`, refusing when anything is
    /// already at `to`.
    async fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
        Err(format!("this vessel agent cannot rename {} to {}", from.display(), to.display()))
    }
    /// The guest's architecture (`amd64`, `arm64`), as the vessel's agent reports the
    /// platform it was built for.
    async fn architecture(&self) -> Result<String, String> {
        Err("this vessel agent does not report its platform".into())
    }
    /// Make the artifact with `sha256` an entry of the agent's verified store
    /// (drydock#86), fetched by the vessel itself from `urls` in order; a repeat is
    /// answered from the store's index. Only bytes hashing to `sha256` are kept.
    async fn fetch_artifact(&self, sha256: &str, urls: &[String], unpack: PinUnpack) -> Result<(), String> {
        let _ = (urls, unpack);
        Err(format!("this vessel agent has no artifact store to fetch {sha256} into"))
    }
    /// As [`VesselAgent::fetch_artifact`], with the bytes pushed from the
    /// controller's verified local copy at `file`.
    async fn push_artifact(&self, sha256: &str, file: &Path, unpack: PinUnpack) -> Result<(), String> {
        let _ = (file, unpack);
        Err(format!("this vessel agent has no artifact store to push {sha256} into"))
    }
    /// Point a symbolic link at `path` to the store entry for `sha256`, or to `member`
    /// inside it, atomically. The link is recorded as a use, so the entry is kept.
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
        let _ = member;
        Err(format!("this vessel agent has no artifact store to link {} to {sha256}", path.display()))
    }
}

/// A live handle to a provisioned sandbox environment.
pub type EnvironmentHandle = Arc<dyn ProvisionedEnvironment>;

/// Minimal backing identity for recovery. No mutable mount or image metadata is
/// needed to reclaim a container left behind by an earlier daemon generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvironmentBacking {
    pub environment_id: EnvironmentId,
    pub backing_id: String,
}

/// Transitional discriminator for the unchanged stored EnvironmentSpec inputs.
/// This is not the provider's runtime kind: dummy accepts the legacy image input
/// too. Retire this bridge when Environment moves to generated tables (#3108).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentInput {
    HostDirect,
    Docker,
}

impl EnvironmentInput {
    pub fn of(spec: &flotilla_resources::EnvironmentSpec) -> Result<Self, String> {
        match (spec.host_direct.is_some(), spec.docker.is_some()) {
            (true, false) => Ok(Self::HostDirect),
            (false, true) => Ok(Self::Docker),
            _ => Err("environment spec must select exactly one kind".into()),
        }
    }
}

/// A provider's private preparation result. Only the creating instance may
/// consume it; callers never need to know the backing runtime's inputs.
pub struct PreparedEnvironment {
    owner: Arc<()>,
    value: Box<dyn std::any::Any + Send + Sync>,
}

impl PreparedEnvironment {
    pub fn new<T: Send + Sync + 'static>(owner: &Arc<()>, value: T) -> Self {
        Self { owner: Arc::clone(owner), value: Box::new(value) }
    }

    pub(crate) fn get<T: 'static>(&self, owner: &Arc<()>) -> Result<&T, String> {
        if !Arc::ptr_eq(&self.owner, owner) {
            return Err("preparation belongs to a different provider instance".into());
        }
        self.value.downcast_ref().ok_or_else(|| "preparation has a different provider kind".into())
    }
}

/// Provisioning classification is independent of its human-readable diagnostic.
/// This runtime-only outcome is never serialized into a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentProvisionError {
    Failed(String),
    /// A retained backing needs another setup attempt, regardless of spec kind.
    RetryAdoption(String),
}
impl std::fmt::Display for EnvironmentProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(message) | Self::RetryAdoption(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for EnvironmentProvisionError {}
impl From<String> for EnvironmentProvisionError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}
impl From<&str> for EnvironmentProvisionError {
    fn from(message: &str) -> Self {
        Self::Failed(message.into())
    }
}

/// Makes an environment spec real, without exposing runtime-specific inputs.
#[async_trait]
pub trait EnvironmentProvider: Send + Sync {
    /// Descriptive identity only; callers must use capability data for behaviour.
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> ProviderCapabilities;
    /// Current local readiness. Cloud providers must verify their credential on
    /// this controlling host before returning Ready. Unknown fails closed.
    async fn detect_readiness(&self) -> ProviderReadiness {
        ProviderReadiness::default()
    }

    /// Compatibility with unlabelled preceding-generation stored inputs only.
    /// New providers need no legacy input discriminator.
    fn legacy_input(&self) -> Option<EnvironmentInput> {
        None
    }

    /// Optional image capability owned by this exact provider instance.
    fn local_image_cache(&self) -> Option<&dyn LocalImageCache> {
        None
    }
    /// Whether this instance's environments receive the host's tools (the
    /// flotilla CLI, cleat and build limits) and run crews in the delivered
    /// cleat pool. Per-vessel (Docker-kind) environments do unless their
    /// instance brings its own crew runtime; host adoption never does.
    fn receives_host_tools(&self) -> bool {
        self.capabilities().receives_host_tools
    }
    /// Optional owned build service using the same endpoint as this
    /// instance's local cache, bound to the host's owned builder settings.
    fn image_build_service(&self, _builder: OwnedBuilderSettings) -> Option<Arc<dyn ImageBuildService>> {
        None
    }
    fn detection(&self) -> Option<&dyn EnvironmentDetection> {
        None
    }
    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String>;
    async fn provision(
        &self,
        id: EnvironmentId,
        prepared: &PreparedEnvironment,
        opts: ProvisionOpts,
    ) -> Result<EnvironmentHandle, EnvironmentProvisionError>;
    async fn inspect(&self, id: &EnvironmentId) -> Result<Option<EnvironmentHandle>, String> {
        Ok(self.list().await?.into_iter().find(|handle| handle.id() == id))
    }
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
    async fn list_backings(&self) -> Result<Vec<EnvironmentBacking>, String> {
        self.list()
            .await?
            .into_iter()
            .map(|handle| {
                let backing_id = handle.backing_id().to_string();
                Ok(EnvironmentBacking { environment_id: handle.id().clone(), backing_id })
            })
            .collect()
    }
    /// Destroy an environment using only its stable provider identity.
    ///
    /// Teardown must not depend on parsing mutable actuated-object metadata:
    /// that metadata can outlive the daemon version which wrote it.
    async fn destroy(&self, backing_id: &str) -> Result<(), String>;
}

/// A handle to a single provisioned sandbox environment instance.
#[async_trait]
pub trait ProvisionedEnvironment: Send + Sync {
    /// Setup rollback is permitted only for a backing created by this call.
    /// Adoption failures preserve the backing for a later retry.
    fn created_for_setup(&self) -> bool {
        true
    }

    /// Opaque identity interpreted only by this provider, independent of transport.
    fn backing_id(&self) -> &str;
    async fn observe(&self) -> ProviderObservation;
    fn capabilities(&self) -> ProviderCapabilities;
    /// Current local readiness. Cloud providers must verify their credential on
    /// this controlling host before returning Ready. Unknown fails closed.
    async fn detect_readiness(&self) -> ProviderReadiness {
        ProviderReadiness::default()
    }

    /// Advisory health information that does not prevent new sessions.
    fn health_message(&self) -> Option<String> {
        None
    }

    /// The facts a cloud backing records when it is provisioned. Every cloud
    /// sandbox handle must return one, and keep returning one, or its provision
    /// fails; other kinds record none. Deliberately without a default (#4163).
    fn provider_descriptor(&self) -> Option<flotilla_resources::EnvironmentProviderDescriptor>;
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;
    /// Immutable content digest of the image actually backing this environment.
    fn local_image_id(&self) -> Option<&str> {
        None
    }

    /// Registry manifest identity, when the image was pulled by digest.
    fn registry_digest(&self) -> Option<&str> {
        None
    }

    /// Provider-specific transport identifier (e.g. Docker container name).
    /// Used by hop chain to construct exec/enter commands.
    fn container_name(&self) -> Option<&str> {
        None
    }
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        Vec::new()
    }

    /// The vessel's configured environment, used as the terminal baseline.
    /// This must not include the provisioning host's ambient environment.
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;
    fn runner(&self) -> Arc<dyn CommandRunner>;
    /// The vessel's one agent. Every kind has one (#3956 Q2): the drydock agent in
    /// Docker and cloud vessels, an in-process agent over the host runner for
    /// host-direct. A handle whose agent is not live returns one that is not
    /// ready, so callers ask its readiness, never its presence.
    fn agent(&self) -> Arc<dyn VesselAgent>;
    /// Explicit permission to reach an existing terminal independently of the
    /// resident vessel agent. A diagnostic runner alone grants no such access.
    fn terminal_route(&self) -> Option<&dyn ProviderTerminalRoute> {
        None
    }
    fn fallback_runner(&self) -> Option<Arc<dyn CommandRunner>> {
        None
    }
    fn fallback_shell_plan(&self, _cwd: &Path) -> Option<flotilla_protocol::ResolvedAttachPlan> {
        None
    }

    async fn destroy(&self) -> Result<(), String>;
}

/// Provider-owned transport to existing Cleat terminals on one live backing.
/// Consumers validate the realised backing, selected session and current endpoint
/// before asking for a plan. Implementations must never create or replace a session.
pub trait ProviderTerminalRoute: Send + Sync {
    /// Terminal inventory/preflight only; does not change the general agent runner.
    fn terminal_runner(&self) -> Arc<dyn CommandRunner>;
    fn attach_plan(
        &self,
        endpoint: &flotilla_protocol::result_set::CleatEndpoint,
        client: &Path,
        seat: flotilla_protocol::commands::AttachMode,
    ) -> Result<flotilla_protocol::ResolvedAttachPlan, String>;
}

/// Decode the preceding checkout configuration at its admission seam. Building
/// Dockerfiles is demand-driven through ImageBuild, never a provider operation.
pub fn legacy_environment_spec(spec: &flotilla_protocol::EnvironmentSpec) -> Result<flotilla_resources::EnvironmentSpec, String> {
    let flotilla_protocol::ImageSource::Registry(image) = &spec.image else {
        return Err("waiting on build: declare an ImageBuild and pin its resulting digest".into());
    };
    Ok(flotilla_resources::EnvironmentSpec {
        cloud_sandbox: None,
        provider_address: None,
        host_direct: None,
        docker: Some(flotilla_resources::DockerEnvironmentSpec {
            host_ref: String::new(),
            image: image.clone(),
            image_build_ref: None,
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: Default::default(),
            mounts: Vec::new(),
            env: Default::default(),
        }),
    })
}

/// Frozen provider instance identity carried from placement policy to its
/// environment. Metadata is extensible, so this adds no stored spec shape.
pub const ENVIRONMENT_PROVIDER_INSTANCE_LABEL: &str = "flotilla.work/environment-provider-instance";

#[cfg(test)]
mod registry_client_tests;

#[cfg(test)]
mod image_tests;

#[cfg(test)]
mod image_build_tests;

/// Compose the built-in provider for a known command endpoint. Callers with a
/// discovered registry must select its existing instance instead.
pub fn command_provider(kind: EnvironmentInput, runner: Arc<dyn CommandRunner>) -> Arc<dyn EnvironmentProvider> {
    match kind {
        EnvironmentInput::Docker => Arc::new(docker::DockerEnvironmentProvider::new(runner)),
        EnvironmentInput::HostDirect => Arc::new(host_direct::HostDirectEnvironmentProvider::new(runner, HashMap::new())),
    }
}
