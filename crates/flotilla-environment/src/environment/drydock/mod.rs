//! Provider-neutral Drydock adapter. The composition root owns drydock's
//! daemon; this adapter accepts the same ProviderApi for embedded and
//! socket-backed use.
//!
//! Drydock owns each vessel's connections (ADR 0070, drydock#132): the adapter
//! asks it for standing exposures and connects to the local sockets they give.
//! It never interprets a reach, holds a vessel key, opens SSH or names a provider.
pub mod api;
mod connector;
mod exposures;
#[cfg(test)]
mod memory;
mod observations;
mod vessel_agent;
use super::{
    agent::{client::AgentLink, AgentRunner},
    vessel_ops::UnavailableAgent,
    *,
};
pub use api::ProviderApi;
use api::{self as wire, ApiError};
pub use connector::installed_agent_build;
use flotilla_protocol::provider::AgentConnectionReport;
use std::{collections::BTreeMap, sync::RwLock, time::Duration};

/// Where the guest's agent and lab service are.
struct GuestLayout {
    /// The agent's socket.
    agent: PathBuf,
    /// The port the lab service's name answers on.
    lab_port: u16,
}

impl Default for GuestLayout {
    fn default() -> Self {
        Self { agent: "/run/drydock/agent.sock".into(), lab_port: 443 }
    }
}
/// The lab service's name inside the guest.
const LAB_HOSTNAME: &str = "forgejo.lab.flotilla.work";
/// How long one agent connection may take, the vessel's boot included.
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct DrydockEnvironmentProvider {
    api: Arc<dyn ProviderApi>,
    link: Arc<dyn AgentLink>,
    instance: String,
    module: String,
    lab_reach: Option<(String, u16)>,
    guest: GuestLayout,
    agent_timeout: Duration,
    owner: Arc<()>,
    executable: Arc<tokio::sync::OnceCell<Vec<u8>>>,
    declaration: RwLock<Option<wire::ProviderDeclaration>>,
    versions: RwLock<(String, String)>,
    provision_locks: std::sync::Mutex<HashMap<EnvironmentId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    observations: std::sync::OnceLock<Arc<observations::Hub>>,
    active: tokio::sync::Mutex<HashMap<EnvironmentId, std::sync::Weak<DrydockEnvironment>>>,
}
impl DrydockEnvironmentProvider {
    /// An instance served by `api`, whose vessels' agents are reached through `link`.
    pub fn new(api: Arc<dyn ProviderApi>, link: Arc<dyn AgentLink>, instance: String, module: String) -> Self {
        Self {
            api,
            link,
            instance,
            module,
            lab_reach: None,
            guest: GuestLayout::default(),
            agent_timeout: AGENT_CONNECT_TIMEOUT,
            owner: Arc::new(()),
            executable: Default::default(),
            declaration: RwLock::new(None),
            versions: Default::default(),
            provision_locks: Default::default(),
            observations: Default::default(),
            active: Default::default(),
        }
    }
    /// The agent build this instance's vessels run, instead of the installed
    /// generation's companion: for hosts and tests without one. Drydock must be
    /// given the same build.
    pub fn with_agent_build(mut self, build: Vec<u8>) -> Self {
        self.executable = Arc::new(tokio::sync::OnceCell::new_with(Some(build)));
        self
    }
    /// Where the guest's agent listens, for a stand-in vessel that is not a
    /// drydock guest.
    pub fn with_guest_agent_socket(mut self, socket: PathBuf) -> Self {
        self.guest.agent = socket;
        self
    }
    /// The port the lab service's name answers on in the guest, for a
    /// stand-in vessel that is not a drydock guest.
    pub fn with_guest_lab_port(mut self, port: u16) -> Self {
        self.guest.lab_port = port;
        self
    }
    /// How long one agent connection may take, the vessel's boot included.
    pub fn with_agent_timeout(mut self, timeout: Duration) -> Self {
        self.agent_timeout = timeout;
        self
    }
    pub fn with_lab_reach(mut self, reach: Option<(String, u16)>) -> Self {
        self.lab_reach = reach;
        self
    }
    /// The instance as drydock declares it. A daemon reports an instance its
    /// host cannot serve with the reason, and conservative capabilities.
    async fn declaration(&self) -> Result<wire::ProviderDeclaration, String> {
        let hello = self.api.hello().await.map_err(api_error)?;
        if !hello.compatible {
            return Err("drydock protocol version mismatch".into());
        }
        let declaration = match self.api.provider(&self.instance).await {
            Ok(declaration) => declaration,
            Err(error) if error.is_not_found() => return Err("instance absent from drydock".into()),
            Err(error) => return Err(api_error(error)),
        };
        let module_version = hello
            .loadable_modules
            .iter()
            .find(|m| m.name == declaration.module)
            .map(|m| m.version.clone())
            .unwrap_or_else(|| "unknown".into());
        *self.versions.write().expect("version lock") = (hello.host_version, module_version);
        *self.declaration.write().expect("declaration lock") = Some(declaration.clone());
        Ok(declaration)
    }
    /// A handle for `environment`. A provisioned handle gets an agent; an
    /// inventory handle has none until provision adopts it.
    fn handle(
        &self,
        environment: wire::Environment,
        image: String,
        env: BTreeMap<String, String>,
        provisioned: bool,
    ) -> Result<Arc<DrydockEnvironment>, String> {
        let label = environment.label.as_ref().ok_or("unlabelled backing cannot be adopted")?;
        let id = EnvironmentId::new(label.clone());
        let platform = environment.platform.ok_or("guest platform unknown in recorded drydock environment")?;
        let report = Arc::new(std::sync::Mutex::new(None));
        let exposures = exposures::Exposures { api: self.api.clone(), instance: self.instance.clone(), backing: environment.id.clone() };
        let agent = provisioned.then(|| {
            let connector = connector::DrydockAgentConnector {
                link: self.link.clone(),
                exposures: exposures.clone(),
                module: self.module.clone(),
                socket: self.guest.agent.clone(),
                timeout: self.agent_timeout,
                platform: platform.clone(),
                executable: self.executable.clone(),
                report: report.clone(),
            };
            // Attach sockets are drydock's exposures, never this runner's forwards.
            Arc::new(AgentRunner::without_attach_forwards(Arc::new(connector)))
        });
        Ok(Arc::new(DrydockEnvironment {
            api: self.api.clone(),
            instance: self.instance.clone(),
            id,
            descriptor: {
                let versions = self.versions.read().expect("version lock");
                flotilla_resources::EnvironmentProviderDescriptor {
                    instance: self.instance.clone(),
                    kind: self.kind(),
                    image_digest: image.clone(),
                    host_version: versions.0.clone(),
                    module_version: versions.1.clone(),
                    capabilities: self.capabilities(),
                    agent_connection: None,
                }
            },
            watch: self
                .observations
                .get_or_init(|| observations::Hub::start(self.api.clone(), self.instance.clone()))
                .watch(environment.id.clone()),
            backing: environment.id,
            image: ImageId::new(image),
            env: RwLock::new(env),
            capabilities: self.capabilities(),
            exposures,
            created_for_setup: std::sync::atomic::AtomicBool::new(false),
            agent,
            report,
        }))
    }

    /// Bring the vessel to what this provision asks: its environment, through
    /// the agent, and the lab service reachable. Each step is idempotent, so
    /// re-adoption runs it again; a lab exposure already standing is left as
    /// drydock keeps it. Tools and sockets are the vessel layer's.
    async fn set_up(&self, handle: &DrydockEnvironment) -> Result<(), String> {
        let agent = handle.agent.as_ref().ok_or("inventory handle has no vessel agent")?;
        let environment = handle.env.read().expect("environment").clone();
        agent.configure_environment(environment).await?;
        if let Some((hostname, port)) = &self.lab_reach {
            handle.exposures.outward("lab", lab_request(hostname, *port, self.guest.lab_port)?).await?;
        }
        ensure_directory(agent, &handle.capabilities.workspace_root).await
    }
}

/// The lab service, reached from drydock's machine, at its own name inside the
/// guest. TLS stays end to end; the guest receives no reach credential.
fn lab_request(hostname: &str, port: u16, guest_port: u16) -> Result<wire::OutwardRequest, String> {
    if guest_port == 0 {
        return Err("lab service guest port must be nonzero".into());
    }
    Ok(wire::OutwardRequest {
        guest: wire::Guest::Name { name: LAB_HOSTNAME.into(), port: guest_port },
        destination: wire::OutwardDestination::Tcp(format!("{hostname}:{port}")),
    })
}

/// `mkdir -p`: a missing directory is created 0755; an existing one keeps its mode.
async fn ensure_directory(agent: &Arc<AgentRunner>, path: &Path) -> Result<(), String> {
    match VesselAgent::stat(agent, path).await? {
        Some(EntryKind::Directory) => Ok(()),
        None => VesselAgent::create_dir(agent, path, 0o755).await,
        Some(_) => Err(format!("{} is not a directory", path.display())),
    }
}
struct Prepared {
    prepared: wire::Prepared,
    spec: flotilla_resources::CloudSandboxEnvironmentSpec,
}
#[async_trait]
impl EnvironmentProvider for DrydockEnvironmentProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::new(EnvironmentClass::CloudSandbox, &self.module)
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.declaration.read().expect("declaration lock").as_ref().map(|p| capabilities(&p.capabilities)).unwrap_or_default()
    }
    async fn detect_readiness(&self) -> ProviderReadiness {
        match self.declaration().await {
            Ok(p) => readiness(p.readiness),
            Err(reason) => ProviderReadiness::Unknown { reason },
        }
    }
    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        if spec.docker.is_some() || spec.host_direct.is_some() {
            return Err("cloud sandbox must be the only environment input".into());
        }
        let platform = opts.platform.clone().unwrap_or_else(crate::guest_tools::linux_amd64);
        if platform != crate::guest_tools::linux_amd64() {
            return Err(format!("unsupported guest platform {platform}"));
        }
        let spec = spec.cloud_sandbox.as_ref().ok_or("missing cloud sandbox input")?;
        if spec.size.cpus == 0 || spec.size.memory_bytes == 0 || spec.size.disk_bytes == 0 {
            return Err("cloud sandbox size must be positive".into());
        }
        if spec.lifetime_cap_seconds.is_some() {
            return Err("per-environment lifetime cap is unsupported by drydock; configure the module cap".into());
        }
        if !spec.image.rsplit_once("@sha256:").is_some_and(|(_, d)| d.len() == 64 && d.bytes().all(|c| c.is_ascii_hexdigit())) {
            return Err("cloud sandbox image must be pinned by sha256 digest".into());
        }
        if !matches!(self.detect_readiness().await, ProviderReadiness::Ready) {
            return Err("drydock provider is not ready".into());
        }
        let prepared = self
            .api
            .prepare(
                &self.instance,
                &wire::PrepareRequest {
                    platform: Some(platform.clone()),
                    // The provider's builder pulls the pinned image and tools itself, as
                    // before drydock protocol 8 named routes; no pull access is sent.
                    route: Some(wire::ImageRouteKind::RegistryPull),
                    image_digest: spec.image.clone(),
                    tools_digest: self.link.tools_image()?,
                    size: size(&spec.size),
                },
            )
            .await
            .map_err(api_error)?;
        if prepared.platform != platform {
            return Err(format!("unsupported guest platform {}", prepared.platform));
        }
        // Fail now for a missing agent companion, rather than obscuring an
        // installation error as guest boot latency.
        connector::agent_build(&self.executable, &prepared.platform).await?;
        Ok(PreparedEnvironment::new(&self.owner, Prepared { prepared, spec: spec.clone() }))
    }
    async fn provision(
        &self,
        id: EnvironmentId,
        prepared: &PreparedEnvironment,
        opts: ProvisionOpts,
    ) -> Result<EnvironmentHandle, EnvironmentProvisionError> {
        let prepared = prepared.get::<Prepared>(&self.owner)?;
        // Only retries for this label serialize through complete guest setup.
        // Different sandboxes must not wait for another guest's SSH readiness.
        let lock = {
            let mut locks = self.provision_locks.lock().expect("provision locks");
            locks.retain(|_, lock| lock.strong_count() > 0);
            let lock = locks.get(&id).and_then(std::sync::Weak::upgrade).unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
            locks.insert(id.clone(), Arc::downgrade(&lock));
            lock
        };
        let _provision_guard = lock.lock().await;
        if !opts.provisioned_mounts.is_empty() {
            return Err("cloud sandbox does not support Shared material".into());
        }
        if !opts.tools.is_empty() {
            return Err("cloud sandbox tools are delivered by the vessel layer, through the agent".into());
        }
        // Preserve HTTP's creation result: label-idempotent POST may adopt a
        // live backing after a restart. Only 201 grants setup rollback.
        let request = wire::ProvisionRequest {
            label: id.to_string(),
            prepared: prepared.prepared.clone(),
            size: size(&prepared.spec.size),
            egress: wire::EgressRequest::Open,
            // Drydock makes and holds the vessel's key under this label.
            ssh_public_key: None,
        };
        let provisioned = match self.api.provision(&self.instance, &request).await {
            Ok(provisioned) => provisioned,
            // A backing this label names, whose key drydock does not hold (one an
            // earlier generation provisioned with its own key), keeps its work.
            Err(ApiError::Protocol { kind: wire::ProtocolErrorKind::Refused, reason, .. })
                if self.list_backings().await.is_ok_and(|backings| backings.iter().any(|b| b.environment_id == id)) =>
            {
                return Err(EnvironmentProvisionError::RetryAdoption(format!("drydock refused to adopt the existing backing: {reason}")));
            }
            Err(error) => return Err(api_error(error).into()),
        };
        let (created, environment) = match provisioned {
            wire::Provisioned::Created(environment) => (true, environment),
            wire::Provisioned::Existing(environment) => (false, environment),
        };
        let mut env = prepared.spec.env.clone();
        env.extend(opts.tokens);
        let active = self.active.lock().await.get(&id).and_then(std::sync::Weak::upgrade);
        if let Some(handle) = active {
            if handle.backing == environment.id && handle.image.as_str() == prepared.spec.image {
                handle.created_for_setup.store(false, std::sync::atomic::Ordering::Release);
                // Re-adoption must apply rotated tokens.
                *handle.env.write().expect("environment") = env;
                self.set_up(&handle).await.map_err(EnvironmentProvisionError::RetryAdoption)?;
                return Ok(handle);
            }
        }
        let handle = self.handle(environment, prepared.spec.image.clone(), env, true)?;
        handle.created_for_setup.store(created, std::sync::atomic::Ordering::Release);
        let setup = async {
            self.set_up(&handle).await?;
            Ok::<_, String>(())
        }
        .await;
        if let Err(cause) = setup {
            if !created {
                return Err(EnvironmentProvisionError::RetryAdoption(cause));
            }
            return match handle.destroy().await {
                Ok(()) => Err(cause.into()),
                Err(cleanup) => Err(format!("{cause}; destroy failed: {cleanup}").into()),
            };
        }
        let mut active = self.active.lock().await;
        active.retain(|_, handle| handle.strong_count() > 0);
        active.insert(id, Arc::downgrade(&handle));
        Ok(handle)
    }
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        let items = self.api.list(&self.instance).await.map_err(api_error)?;
        items
            .into_iter()
            .filter(|e| e.label.is_some())
            .map(|e| self.handle(e, String::new(), BTreeMap::new(), false).map(|h| h as EnvironmentHandle))
            .collect()
    }
    async fn list_backings(&self) -> Result<Vec<EnvironmentBacking>, String> {
        Ok(self
            .api
            .list(&self.instance)
            .await
            .map_err(api_error)?
            .into_iter()
            .filter_map(|e| e.label.map(|label| EnvironmentBacking { environment_id: EnvironmentId::new(label), backing_id: e.id }))
            .collect())
    }
    async fn destroy(&self, backing_id: &str) -> Result<(), String> {
        for handle in self.active.lock().await.values().filter_map(std::sync::Weak::upgrade) {
            if handle.backing == backing_id {
                return handle.destroy().await;
            }
        }
        self.api.destroy(&self.instance, backing_id).await.map_err(api_error)
    }
}
fn size(s: &flotilla_resources::CloudSandboxSize) -> wire::Size {
    wire::Size { cpus: f64::from(s.cpus), memory_bytes: s.memory_bytes, disk_bytes: s.disk_bytes }
}
fn readiness(r: wire::Readiness) -> ProviderReadiness {
    match r {
        wire::Readiness::Ready => ProviderReadiness::Ready,
        wire::Readiness::MissingTool { reason } => ProviderReadiness::MissingTool { reason },
        wire::Readiness::MissingCredential { reason } => ProviderReadiness::MissingCredential { reason },
        wire::Readiness::Unknown { reason } | wire::Readiness::VersionMismatch { reason } => ProviderReadiness::Unknown { reason },
    }
}
fn capabilities(c: &wire::Capabilities) -> ProviderCapabilities {
    // Report only operations implemented by this adapter, even if a module
    // offers more. Tool delivery uses the native agent on every provision.
    ProviderCapabilities {
        material_modes: c
            .material_modes
            .iter()
            .filter(|m| !matches!(m, wire::MaterialMode::Shared))
            .map(|m| match m {
                wire::MaterialMode::Frozen => MaterialMode::Frozen,
                wire::MaterialMode::Pushed => MaterialMode::Pushed,
                wire::MaterialMode::Shared => MaterialMode::Shared,
            })
            .collect(),
        checkout_strategies: c
            .checkout_strategies
            .iter()
            .filter(|s| matches!(s, wire::CheckoutStrategy::CloneInVessel))
            .map(|s| match s {
                wire::CheckoutStrategy::HostWorktree => CheckoutStrategy::HostWorktree,
                wire::CheckoutStrategy::HostClone => CheckoutStrategy::HostClone,
                wire::CheckoutStrategy::CloneInVessel => CheckoutStrategy::CloneInVessel,
            })
            .collect(),
        workspace_root: c.workspace_root.clone().into(),
        reference_root: None,
        suspend_resume: false,
        export: false,
        egress_route: EgressRoute::Open,
        egress_enforcement: EgressEnforcement::Advisory,
        fallback_runner: c.fallback_runner,
        receives_host_tools: true,
        per_vessel: true,
    }
}
struct DrydockEnvironment {
    api: Arc<dyn ProviderApi>,
    instance: String,
    id: EnvironmentId,
    backing: String,
    image: ImageId,
    env: RwLock<BTreeMap<String, String>>,
    capabilities: ProviderCapabilities,
    descriptor: flotilla_resources::EnvironmentProviderDescriptor,
    created_for_setup: std::sync::atomic::AtomicBool,
    /// The vessel's connections drydock keeps.
    exposures: exposures::Exposures,
    /// The vessel's agent, for a handle provision adopted.
    agent: Option<Arc<AgentRunner>>,
    /// Where the agent's latest connection went: recorded and shown, never decided on.
    report: Arc<std::sync::Mutex<Option<AgentConnectionReport>>>,
    watch: observations::Watch,
}
#[async_trait]
impl ProvisionedEnvironment for DrydockEnvironment {
    async fn detect_readiness(&self) -> ProviderReadiness {
        ProviderReadiness::Ready
    }
    fn created_for_setup(&self) -> bool {
        self.created_for_setup.load(std::sync::atomic::Ordering::Acquire)
    }

    fn backing_id(&self) -> &str {
        &self.backing
    }
    fn id(&self) -> &EnvironmentId {
        &self.id
    }
    fn image(&self) -> &ImageId {
        &self.image
    }
    fn local_image_id(&self) -> Option<&str> {
        self.image.as_str().rsplit_once('@').map(|(_, digest)| digest)
    }
    fn registry_digest(&self) -> Option<&str> {
        self.local_image_id().map(|_| self.image.as_str())
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }
    fn provider_descriptor(&self) -> Option<flotilla_resources::EnvironmentProviderDescriptor> {
        let agent_connection = self.report.lock().expect("agent report").clone();
        Some(flotilla_resources::EnvironmentProviderDescriptor { agent_connection, ..self.descriptor.clone() })
    }

    async fn observe(&self) -> ProviderObservation {
        let cached = self.watch.state.read().expect("observation lock").clone();
        // A snapshot may have been collected before this handle was created.
        // Confirm snapshot-derived absence directly before declaring work lost.
        let cached = cached.filter(|state| !matches!(state, ObservationState::Gone { cause: GoneCause::MissingOnList }));
        let state = if let Some(state) = cached {
            state
        } else {
            match self.api.inspect(&self.instance, &self.backing).await {
                Ok(e) => observation(e.state),
                Err(error) if error.is_not_found() => ObservationState::Gone { cause: GoneCause::MissingOnList },
                Err(e) => ObservationState::Unknown { reason: UnknownReason::ProviderApiError { message: api_error(e) } },
            }
        };
        // Running backing plus a ready agent is the readiness contract. An agent
        // that does not answer is Unknown, never evidence that work is Gone.
        if let (ObservationState::Running, Some(agent)) = (&state, &self.agent) {
            if let Err(message) = VesselAgent::ready(agent).await {
                return ProviderObservation::new(ObservationState::Unknown { reason: UnknownReason::ProviderApiError { message } });
            }
        }
        ProviderObservation::new(state)
    }
    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        let output = self.runner().run("env", &["-0"], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default).await?;
        Ok(output.split('\0').filter_map(|entry| entry.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        match &self.agent {
            Some(agent) => agent.clone(),
            None => Arc::new(InventoryRunner),
        }
    }
    fn agent(&self) -> Arc<dyn VesselAgent> {
        match &self.agent {
            Some(agent) => Arc::new(vessel_agent::DrydockVesselAgent { runner: agent.clone(), exposures: self.exposures.clone() }),
            None => Arc::new(UnavailableAgent("inventory handle requires provision before guest access".into())),
        }
    }
    async fn destroy(&self) -> Result<(), String> {
        self.api.destroy(&self.instance, &self.backing).await.map_err(api_error)?;
        if let Some(agent) = &self.agent {
            agent.close().await;
        }
        Ok(())
    }
}

fn observation(o: wire::Observation) -> ObservationState {
    match o {
        wire::Observation::Running => ObservationState::Running,
        wire::Observation::Stopped { cause } => ObservationState::Stopped {
            cause: match cause {
                wire::StopCause::SuspendedByRequest => StoppedCause::SuspendedByRequest,
                wire::StopCause::PausedByProvider => StoppedCause::PausedByProvider,
                wire::StopCause::GuestHalted => StoppedCause::GuestHalted,
            },
        },
        wire::Observation::Gone { cause } => ObservationState::Gone {
            cause: match cause {
                wire::GoneCause::LifetimeCapReached => GoneCause::LifetimeCapReached,
                wire::GoneCause::EvictedByProvider | wire::GoneCause::DestroyedAtProvider => GoneCause::EvictedByProvider,
                wire::GoneCause::HostOutOfMemory => GoneCause::HostOutOfMemory,
                wire::GoneCause::VmmExited { code } => GoneCause::BackingExited { code },
                wire::GoneCause::GuestPoweredOff => GoneCause::GuestPoweredOff,
                wire::GoneCause::MissingOnList => GoneCause::MissingOnList,
                wire::GoneCause::DestroyedByRequest => GoneCause::DestroyedByRequest,
            },
        },
        wire::Observation::Transitioning { .. } => {
            ObservationState::Unknown { reason: UnknownReason::ProviderApiError { message: "provider transitioning".into() } }
        }
        wire::Observation::Unknown { reason } => ObservationState::Unknown {
            reason: match reason {
                wire::UnknownReason::HostUnreachable => UnknownReason::HostUnreachable,
                wire::UnknownReason::GuestUnresponsive => UnknownReason::GuestUnresponsive,
                _ => UnknownReason::ProviderApiError { message: "provider state unknown".into() },
            },
        },
    }
}

#[cfg(test)]
mod tests;

// Inventory handles report observations without opening guest transports. Restart
// adoption re-enters idempotent provision before registering a runnable handle.
struct InventoryRunner;
#[async_trait]
impl CommandRunner for InventoryRunner {
    async fn run(&self, _: &str, _: &[&str], _: &Path, _: &flotilla_provider_runtime::ChannelLabel) -> Result<String, String> {
        Err("inventory handle requires provision before guest execution".into())
    }
    async fn run_output(
        &self,
        _: &str,
        _: &[&str],
        _: &Path,
        _: &flotilla_provider_runtime::ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        Err("inventory handle requires provision before guest execution".into())
    }
    async fn same_filesystem(&self, _: &Path, _: &Path) -> Result<bool, String> {
        Err("cloud sandbox has no Shared material".into())
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        false
    }
}

/// Drydock masks credentials in the provider's explanation before it leaves
/// the host; without it an operator cannot tell a quota from a fault.
fn api_error(cause: ApiError) -> String {
    cause.to_string()
}
