//! Provider-neutral Drydock adapter. The composition root owns host supervision;
//! this adapter accepts the same ProviderApi for embedded and socket-backed use.
//!
//! Drydock owns the connection to each vessel's agent (ADR 0070): the adapter
//! asks [`AgentReach`] for a connected agent and uses only the agent's API. It
//! never interprets a reach, opens SSH or names a provider.
mod connector;
mod observations;
use super::{agent::AgentRunner, vessel_ops::UnavailableAgent, *};
use crate::environment::runner::{contained_credential_exclusions, CONTAINED_CODEX_HOME, CONTAINED_WRITABLE_CONFIG_BASE};
use ::drydock::{
    agent::{AgentLink, AgentReach, AgentReport},
    api::ProviderApi,
};
use drydock_agent::{Endpoint, GuestForward};
use drydock_protocol as wire;
use flotilla_protocol::provider::AgentConnectionReport;
const CHECKPOINT_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
use sha2::Digest;
use std::{collections::BTreeMap, sync::RwLock, time::Duration};

/// Where the guest serves the lab service.
struct LabGuest {
    /// The loopback address its name resolves to in the guest.
    tcp: std::net::SocketAddr,
    /// The forward's guest socket, beside its TCP listener.
    socket: PathBuf,
    /// The hosts file that names it.
    hosts: PathBuf,
}

impl Default for LabGuest {
    fn default() -> Self {
        Self {
            tcp: std::net::SocketAddr::from(([127, 0, 0, 1], 443)),
            socket: "/run/drydock/flotilla-forgejo.sock".into(),
            hosts: "/etc/hosts".into(),
        }
    }
}
/// The lab service's name inside the guest; it resolves to the guest's loopback.
const LAB_HOSTNAME: &str = "forgejo.lab.flotilla.work";
/// Where per-vessel attach sockets live on the controller: short, for SUN_LEN.
const ATTACH_ROOT: &str = "/tmp/flotilla-cloud-agents";

pub struct DrydockEnvironmentProvider {
    api: Arc<dyn ProviderApi>,
    reach: AgentReach,
    instance: String,
    module: String,
    lab_reach: Option<(String, u16)>,
    lab_guest: LabGuest,
    owner: Arc<()>,
    directory: PathBuf,
    attach_root: PathBuf,
    executable: Arc<tokio::sync::OnceCell<Vec<u8>>>,
    declaration: RwLock<Option<wire::ProviderDeclaration>>,
    host: Option<Arc<::drydock::HostHandle>>,
    versions: RwLock<(String, String)>,
    provision_locks: std::sync::Mutex<HashMap<EnvironmentId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    observations: std::sync::OnceLock<Arc<observations::Hub>>,
    active: tokio::sync::Mutex<HashMap<EnvironmentId, std::sync::Weak<DrydockEnvironment>>>,
}
impl DrydockEnvironmentProvider {
    pub fn new(api: Arc<dyn ProviderApi>, instance: String, module: String, directory: PathBuf) -> Self {
        Self {
            api,
            reach: AgentReach::new(AgentLink::default()),
            instance,
            module,
            lab_reach: None,
            lab_guest: LabGuest::default(),
            owner: Arc::new(()),
            directory,
            attach_root: ATTACH_ROOT.into(),
            executable: Default::default(),
            declaration: RwLock::new(None),
            host: None,
            versions: Default::default(),
            provision_locks: Default::default(),
            observations: Default::default(),
            active: Default::default(),
        }
    }
    /// How this instance reaches its vessels' agents: drydock's, under the
    /// provider's configured link preference.
    pub fn with_agent_reach(mut self, reach: AgentReach) -> Self {
        self.reach = reach;
        self
    }
    /// The agent build this instance's vessels run, instead of the installed
    /// generation's companion: for hosts and tests without one.
    pub fn with_agent_build(mut self, build: Vec<u8>) -> Self {
        self.executable = Arc::new(tokio::sync::OnceCell::new_with(Some(build)));
        self
    }
    /// Where attach sockets for this instance's vessels live on the controller.
    pub fn with_attach_root(mut self, root: PathBuf) -> Self {
        self.attach_root = root;
        self
    }
    pub fn with_lab_reach(mut self, reach: Option<(String, u16)>) -> Self {
        self.lab_reach = reach;
        self
    }
    pub fn with_host(mut self, host: Arc<::drydock::HostHandle>) -> Self {
        self.host = Some(host);
        self
    }
    async fn declaration(&self) -> Result<wire::ProviderDeclaration, String> {
        if let Some(host) = &self.host {
            let status = host.status.borrow();
            let instance_readiness = host.instances().get(&self.instance).cloned().unwrap_or_else(|| status.readiness.clone());
            if !matches!(instance_readiness, wire::Readiness::Ready) {
                return Ok(wire::ProviderDeclaration {
                    provider: self.instance.clone(),
                    module: self.module.clone(),
                    capabilities: empty_capabilities(),
                    readiness: instance_readiness,
                    rate_limits: vec![],
                });
            }
        }
        let hello = self.api.hello().await.map_err(api_error)?;
        if !hello.is_compatible() {
            return Err("drydock protocol version mismatch".into());
        }
        let declaration = hello.providers.into_iter().find(|p| p.provider == self.instance).ok_or("instance absent from drydock hello")?;
        let module_version =
            hello.modules.iter().find(|m| m.name == declaration.module).map(|m| m.version.clone()).unwrap_or_else(|| "unknown".into());
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
        let agent = provisioned.then(|| {
            let connector = connector::DrydockAgentConnector {
                api: self.api.clone(),
                reach: self.reach.clone(),
                instance: self.instance.clone(),
                backing: environment.id.clone(),
                identity: key_dir(&self.directory, &id).join("id_ed25519"),
                platform: platform.clone(),
                executable: self.executable.clone(),
                report: report.clone(),
            };
            let attach = self.attach_root.join(&format!("{:x}", sha2::Sha256::digest(id.as_str().as_bytes()))[..24]);
            Arc::new(AgentRunner::new(Arc::new(connector), attach))
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
            directory: self.directory.clone(),
            created_for_setup: std::sync::atomic::AtomicBool::new(false),
            checkpoint: std::sync::Mutex::new(None),
            agent,
            report,
            checkpoint_error: Default::default(),
        }))
    }

    /// Bring the vessel to what this provision asks: its environment and the lab
    /// service reachable, through the agent. Every step is idempotent, so
    /// re-adoption runs it again. Tools and sockets are the vessel layer's.
    async fn set_up(&self, handle: &DrydockEnvironment) -> Result<(), String> {
        let agent = handle.agent.as_ref().ok_or("inventory handle has no vessel agent")?;
        let environment = handle.env.read().expect("environment").clone();
        agent.configure_environment(environment).await?;
        if let Some((hostname, port)) = &self.lab_reach {
            forward_lab(agent, hostname, *port, &self.lab_guest).await?;
        }
        ensure_directory(agent, &handle.capabilities.workspace_root).await
    }
}

/// The lab service, reached from the controller, at its own name and port 443
/// inside the guest. TLS stays end to end; the guest receives no reach credential.
async fn forward_lab(agent: &Arc<AgentRunner>, hostname: &str, port: u16, guest: &LabGuest) -> Result<(), String> {
    // The agent's TCP endpoint names one address, so take the first in the
    // resolver's order (RFC 6724 destination selection, as a plain connect tries first).
    let address = tokio::net::lookup_host((hostname, port))
        .await
        .map_err(|error| format!("resolve lab service {hostname}:{port}: {error}"))?
        .next()
        .ok_or_else(|| format!("lab service {hostname}:{port} resolves to no address"))?;
    let forward = GuestForward { id: "lab".into(), unix: guest.socket.clone(), tcp: Some(guest.tcp) };
    agent.put_guest_forward(forward, Endpoint::Tcp(address)).await.map_err(|failure| failure.to_string())?;
    // Provision holds this label's lock through set-up, so no two runs append at once.
    let script = lab_hosts_script(&guest.hosts);
    agent
        .run("sh", &["-c", &script], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default)
        .await
        .map(drop)
        .map_err(|error| format!("lab hosts entry in {}: {error}", guest.hosts.display()))
}

fn lab_hosts_script(hosts: &Path) -> String {
    let hosts = flotilla_protocol::arg::shell_quote(&hosts.to_string_lossy());
    format!("grep -q '^127.0.0.1 {LAB_HOSTNAME}$' {hosts} || printf '\\n127.0.0.1 {LAB_HOSTNAME}\\n' >> {hosts}")
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
                    image_pull: None,
                    image_digest: spec.image.clone(),
                    tools_digest: ::drydock::launch::build_tools_digest().map_err(|e| e.to_string())?.into(),
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
        let keydir = key_dir(&self.directory, &id);
        ::drydock::local::private_dir(&keydir)?;
        let public = if keydir.join("id_ed25519.pub").is_file() && keydir.join("id_ed25519").is_file() {
            tokio::fs::read_to_string(keydir.join("id_ed25519.pub")).await.map_err(|cause| cause.to_string())?
        } else {
            // An existing backing still trusts the original key. Regenerating
            // here cannot rotate that key through label-idempotent provision.
            let backings = self.api.list(&self.instance).await.map_err(|cause| {
                EnvironmentProvisionError::RetryAdoption(format!("cannot verify backing before SSH key repair: {}", api_error(cause)))
            })?;
            if backings.iter().any(|backing| backing.label.as_deref() == Some(id.as_str())) {
                return Err(EnvironmentProvisionError::RetryAdoption("SSH keypair incomplete or missing for adopted backing; restore the controller key files; public-key rotation on adoption is unsupported".into()));
            }
            remove_keys(&keydir).await?;
            ::drydock::launch::keypair(&keydir).await.map_err(api_error)?.1
        };
        // Preserve HTTP's creation result: label-idempotent POST may adopt a
        // live backing after a restart. Only 201 grants setup rollback.
        let request = wire::ProvisionRequest {
            label: id.to_string(),
            prepared: prepared.prepared.clone(),
            size: size(&prepared.spec.size),
            egress: wire::EgressRequest::Open,
            ssh_public_key: Some(public),
        };
        let (created, environment) = match self.api.provision(&self.instance, &request).await.map_err(api_error)? {
            ::drydock::api::Provisioned::Created(environment) => (true, environment),
            ::drydock::api::Provisioned::Existing(environment) => (false, environment),
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
            // Checkpoints are advisory recovery material. Neither the first archive
            // nor a refresh may delay adoption or trigger setup rollback.
            let checkpoint = keydir.join("checkpoint.tar");
            let weak = Arc::downgrade(&handle);
            let root = handle.capabilities.workspace_root.clone();
            *handle.checkpoint.lock().expect("checkpoint task") = Some(tokio::spawn(async move {
                let mut refresh = tokio::time::interval(Duration::from_secs(30));
                refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    refresh.tick().await;
                    let Some(handle) = weak.upgrade() else {
                        break;
                    };
                    if matches!(handle.observe().await.state, ObservationState::Gone { .. }) {
                        break;
                    }
                    let result = write_checkpoint(handle.runner(), &checkpoint, &root).await;
                    *handle.checkpoint_error.write().expect("checkpoint health") = result.err();
                    // Do not hold the environment alive while waiting for the next refresh.
                    drop(handle);
                }
            }));
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
        let environment = self.api.inspect(&self.instance, backing_id).await.ok();
        self.api.destroy(&self.instance, backing_id).await.map_err(api_error)?;
        if let Some(label) = environment.and_then(|e| e.label) {
            remove_keys(&key_dir(&self.directory, &EnvironmentId::new(label))).await?;
        }
        Ok(())
    }
}
// Hash IDs instead of allowing resource names to select filesystem paths.
fn key_dir(directory: &Path, id: &EnvironmentId) -> PathBuf {
    directory.join(format!("{:x}", sha2::Sha256::digest(directory.join(id.as_str()).as_os_str().as_encoded_bytes())))
}
fn size(s: &flotilla_resources::CloudSandboxSize) -> wire::Size {
    wire::Size { cpus: f64::from(s.cpus), memory_bytes: s.memory_bytes, disk_bytes: s.disk_bytes }
}
fn empty_capabilities() -> wire::Capabilities {
    wire::Capabilities {
        agent: false,
        platforms: vec![],
        material_modes: vec![],
        checkout_strategies: vec![],
        workspace_root: String::new(),
        suspend_resume: false,
        suspend_preserves_memory: None,
        export: false,
        egress: wire::EgressCapability { enforcement: wire::Enforcement::Advisory, routes: vec![wire::EgressRoute::Open] },
        fallback_runner: false,
        bootstrap: wire::BootstrapRoute::Bake,
        image_routes: vec![],
        usage: false,
        quota: false,
    }
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
    checkpoint: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    directory: PathBuf,
    /// The vessel's agent, for a handle provision adopted.
    agent: Option<Arc<AgentRunner>>,
    /// What drydock reports it chose for the agent's latest connection:
    /// recorded and shown, never decided on.
    report: Arc<std::sync::Mutex<Option<AgentReport>>>,
    checkpoint_error: RwLock<Option<String>>,
    watch: observations::Watch,
}
impl Drop for DrydockEnvironment {
    fn drop(&mut self) {
        if let Some(task) = self.checkpoint.get_mut().expect("checkpoint task").take() {
            task.abort();
        }
    }
}
/// Capture recoverable guest state through its runner, publishing only a complete archive.
pub async fn write_checkpoint(runner: Arc<dyn CommandRunner>, target: &Path, root: &Path) -> Result<(), String> {
    let temporary = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    // Synchronous drop cleanup also runs when the async task is cancelled.
    struct PartialArchive(PathBuf);
    impl Drop for PartialArchive {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _partial = PartialArchive(temporary.clone());
    let paths = [
        root.to_string_lossy().into_owned(),
        "/root".into(),
        CONTAINED_WRITABLE_CONFIG_BASE.into(),
        CONTAINED_CODEX_HOME.into(),
        "/var/lib/flotilla/cleat".into(),
    ];
    let script = checkpoint_script(&paths);
    runner.run_to_file("sh", &["-c", &script], Path::new("/"), &temporary).await?;
    tokio::fs::rename(temporary, target).await.map_err(|cause| cause.to_string())
}
fn checkpoint_script(paths: &[String]) -> String {
    // Exclude reproducible build output and caches from both the preflight and
    // the archive. Stage remotely and check the exact archive size before any
    // bytes cross SSH; the runner's stream bound remains defence in depth.
    let excludes = ["target", "node_modules", ".cache", ".cargo/registry", ".cargo/git", ".rustup", ".npm", ".uv"];
    let credentials = contained_credential_exclusions();
    let excludes = excludes
        .iter()
        .copied()
        .chain(credentials.iter().map(String::as_str))
        .map(|p| format!("--exclude={}", flotilla_protocol::arg::shell_quote(p)))
        .collect::<Vec<_>>()
        .join(" ");
    let paths = paths.iter().map(|p| flotilla_protocol::arg::shell_quote(p)).collect::<Vec<_>>().join(" ");
    format!(
        "flotilla_checkpoint_size=$(du -sb {excludes} -- {paths} 2>/dev/null | awk '{{n += $1}} END {{printf \"%.0f\", n}}'); \
         test \"$flotilla_checkpoint_size\" -le {CHECKPOINT_LIMIT_BYTES} || exit 1; \
         flotilla_checkpoint_free=$(df -PB1 /tmp | awk 'NR == 2 {{print $4}}'); \
         test \"$flotilla_checkpoint_free\" -ge \"$((flotilla_checkpoint_size + 1048576))\" || exit 1; \
         flotilla_checkpoint_tmp=$(mktemp /tmp/flotilla-checkpoint.XXXXXXXX) || exit 1; \
         trap 'rm -f -- \"$flotilla_checkpoint_tmp\"' EXIT; \
         tar --ignore-failed-read {excludes} -cf \"$flotilla_checkpoint_tmp\" -- {paths}; \
         flotilla_tar_status=$?; test \"$flotilla_tar_status\" -le 1 || exit 1; \
         test \"$(stat -c%s \"$flotilla_checkpoint_tmp\")\" -le {CHECKPOINT_LIMIT_BYTES} || exit 1; \
         cat -- \"$flotilla_checkpoint_tmp\""
    )
}

#[async_trait]
impl ProvisionedEnvironment for DrydockEnvironment {
    async fn detect_readiness(&self) -> ProviderReadiness {
        ProviderReadiness::Ready
    }
    fn health_message(&self) -> Option<String> {
        self.checkpoint_error.read().expect("checkpoint health").clone().map(|reason| format!("checkpoint stale: {reason}"))
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
    fn survivors(&self) -> Option<flotilla_resources::EnvironmentSurvivors> {
        Some(flotilla_resources::EnvironmentSurvivors {
            host_checkpoint: Some(key_dir(&self.directory, &self.id).join("checkpoint.tar").to_string_lossy().into_owned()),
            ..Default::default()
        })
    }
    fn provider_descriptor(&self) -> Option<flotilla_resources::EnvironmentProviderDescriptor> {
        let agent_connection = self.report.lock().expect("agent report").clone().map(|report| AgentConnectionReport {
            provider: report.provider,
            module: report.module,
            vessel: report.environment,
            endpoint: report.endpoint,
            mode: report.link.to_string(),
        });
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
                Err(::drydock::client::Error::Protocol(wire::ProtocolError::NotFound { .. })) => {
                    ObservationState::Gone { cause: GoneCause::MissingOnList }
                }
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
            Some(agent) => Arc::new(agent.clone()),
            None => Arc::new(UnavailableAgent("inventory handle requires provision before guest access".into())),
        }
    }
    async fn destroy(&self) -> Result<(), String> {
        self.api.destroy(&self.instance, &self.backing).await.map_err(api_error)?;
        if let Some(agent) = &self.agent {
            agent.close().await;
        }
        if let Some(task) = self.checkpoint.lock().expect("checkpoint task").take() {
            task.abort();
        }
        // Keep the last complete checkpoint for recovery; it is a declared
        // survivor, not scratch. The operator removes it after continue/abandon.
        remove_keys(&key_dir(&self.directory, &self.id)).await
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

async fn remove_keys(directory: &Path) -> Result<(), String> {
    for file in ["id_ed25519", "id_ed25519.pub"] {
        match tokio::fs::remove_file(directory.join(file)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}
fn api_error(cause: ::drydock::client::Error) -> String {
    use ::drydock::client::Error;
    match cause {
        Error::Protocol(_) => "drydock provider protocol error".into(),
        Error::NotReady(_) => "drydock provider not ready".into(),
        Error::ReadinessTimeout(_) => "drydock provider readiness timed out".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod conformance_test;
