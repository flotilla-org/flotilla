//! Provider-neutral Drydock adapter. The composition root owns host supervision;
//! this adapter accepts the same ProviderApi for embedded and socket-backed use.
mod observations;
mod reach;
mod runner;
use super::*;
use ::drydock::api::ProviderApi;
use drydock_protocol as wire;
use runner::{error, AgentRunner};
use sha2::Digest;
use std::{collections::BTreeMap, sync::RwLock, time::Duration};
use tokio::sync::OnceCell;

pub struct DrydockEnvironmentProvider {
    api: Arc<dyn ProviderApi>,
    connector: Arc<dyn reach::Connector>,
    instance: String,
    module: String,
    owner: Arc<()>,
    directory: PathBuf,
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
            connector: Arc::new(reach::SshConnector),
            instance,
            module,
            owner: Arc::new(()),
            directory,
            declaration: RwLock::new(None),
            host: None,
            versions: Default::default(),
            provision_locks: Default::default(),
            observations: Default::default(),
            active: Default::default(),
        }
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
        let response = self.api.request(http::Method::GET, &["v1", "hello"], &[], None).await.map_err(api_error)?;
        let hello: wire::Hello = serde_json::from_value(response.body).map_err(|_| "invalid drydock hello")?;
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
    fn handle(
        &self,
        environment: wire::Environment,
        image: String,
        env: BTreeMap<String, String>,
    ) -> Result<Arc<DrydockEnvironment>, String> {
        let label = environment.label.as_ref().ok_or("unlabelled backing cannot be adopted")?;
        Ok(Arc::new(DrydockEnvironment {
            api: self.api.clone(),
            connector: self.connector.clone(),
            instance: self.instance.clone(),
            id: EnvironmentId::new(label.clone()),
            descriptor: {
                let versions = self.versions.read().expect("version lock");
                flotilla_resources::EnvironmentProviderDescriptor {
                    instance: self.instance.clone(),
                    kind: self.kind(),
                    image_digest: image.clone(),
                    host_version: versions.0.clone(),
                    module_version: versions.1.clone(),
                    capabilities: self.capabilities(),
                }
            },
            watch: self
                .observations
                .get_or_init(|| observations::Hub::start(self.api.clone(), self.instance.clone()))
                .watch(environment.id.clone()),
            backing: environment.id,
            image: ImageId::new(image),
            env,
            capabilities: self.capabilities(),
            directory: self.directory.clone(),
            checkpoint: std::sync::Mutex::new(None),
            connection: OnceCell::new(),
        }))
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
    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        if spec.docker.is_some() || spec.host_direct.is_some() {
            return Err("cloud sandbox must be the only environment input".into());
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
                    platform: None,
                    image_digest: spec.image.clone(),
                    tools_digest: ::drydock::launch::build_tools_digest().map_err(|e| e.to_string())?.into(),
                    size: size(&spec.size),
                },
            )
            .await
            .map_err(api_error)?;
        Ok(PreparedEnvironment::new(&self.owner, Prepared { prepared, spec: spec.clone() }))
    }
    async fn provision(&self, id: EnvironmentId, prepared: &PreparedEnvironment, opts: ProvisionOpts) -> Result<EnvironmentHandle, String> {
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
        let keydir = key_dir(&self.directory, &id);
        ::drydock::local::private_dir(&keydir)?;
        let public = if keydir.join("id_ed25519.pub").is_file() {
            tokio::fs::read_to_string(keydir.join("id_ed25519.pub")).await.map_err(error)?
        } else {
            ::drydock::launch::keypair(&keydir).await.map_err(api_error)?.1
        };
        let environment = self
            .api
            .provision(
                &self.instance,
                &wire::ProvisionRequest {
                    label: id.to_string(),
                    prepared: prepared.prepared.clone(),
                    size: size(&prepared.spec.size),
                    egress: wire::EgressRequest::Open,
                    ssh_public_key: Some(public),
                },
            )
            .await
            .map_err(api_error)?;
        let mut env = prepared.spec.env.clone();
        env.extend(opts.tokens);
        for tool in &opts.tools {
            for update in &tool.environment {
                match update {
                    EnvironmentVariableUpdate::Set { name, value, .. } => {
                        env.insert(name.clone(), value.clone());
                    }
                    EnvironmentVariableUpdate::PrependPath { name, value } => {
                        let existing =
                            env.get(name).map(String::as_str).unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
                        env.insert(name.clone(), format!("{value}:{existing}"));
                    }
                }
            }
        }
        if let Some(handle) = self.active.lock().await.get(&id).and_then(std::sync::Weak::upgrade) {
            if handle.backing == environment.id && handle.image.as_str() == prepared.spec.image {
                let connection = handle.connect().await?;
                for tool in opts.tools {
                    for asset in tool.assets {
                        connection.deliver(asset).await?;
                    }
                }
                // Re-adoption must apply rotated tokens and tool environments.
                connection.configure(env).await?;
                return Ok(handle);
            }
        }
        let handle = self.handle(environment, prepared.spec.image.clone(), env)?;
        let setup = async {
            let connection = handle.connect().await?;
            for tool in opts.tools {
                for asset in tool.assets {
                    connection.deliver(asset).await?;
                }
            }
            connection.configure(handle.env.clone()).await?;
            connection
                .runner()
                .run(
                    "mkdir",
                    &["-p", &handle.capabilities.workspace_root.to_string_lossy()],
                    Path::new("/"),
                    &flotilla_provider_runtime::ChannelLabel::Default,
                )
                .await?;
            let checkpoint = keydir.join("checkpoint.tar");
            write_checkpoint(connection.runner(), &checkpoint, &handle.capabilities.workspace_root).await?;
            let runner = connection.runner();
            let root = handle.capabilities.workspace_root.clone();
            let watch = handle.watch.clone();
            *handle.checkpoint.lock().expect("checkpoint task") = Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if matches!(*watch.state.read().expect("observation lock"), Some(ObservationState::Gone { .. })) {
                        break;
                    }
                    if write_checkpoint(runner.clone(), &checkpoint, &root).await.is_err() {
                        tracing::warn!("cloud sandbox checkpoint refresh failed");
                    }
                }
            }));
            Ok::<_, String>(())
        }
        .await;
        if let Err(cause) = setup {
            return match handle.destroy().await {
                Ok(()) => Err(cause),
                Err(cleanup) => Err(format!("{cause}; destroy failed: {cleanup}")),
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
            .map(|e| self.handle(e, String::new(), BTreeMap::new()).map(|h| h as EnvironmentHandle))
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
    connector: Arc<dyn reach::Connector>,
    instance: String,
    id: EnvironmentId,
    backing: String,
    image: ImageId,
    env: BTreeMap<String, String>,
    capabilities: ProviderCapabilities,
    descriptor: flotilla_resources::EnvironmentProviderDescriptor,
    checkpoint: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    directory: PathBuf,
    connection: OnceCell<Arc<dyn reach::Guest>>,
    watch: observations::Watch,
}
impl Drop for DrydockEnvironment {
    fn drop(&mut self) {
        if let Some(task) = self.checkpoint.get_mut().expect("checkpoint task").take() {
            task.abort();
        }
    }
}
async fn write_checkpoint(runner: Arc<dyn CommandRunner>, target: &Path, root: &Path) -> Result<(), String> {
    let temporary = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let paths = [
        root.to_string_lossy().into_owned(),
        "/root".into(),
        "/tmp/flotilla-config".into(),
        "/tmp/flotilla-codex".into(),
        "/var/lib/flotilla/cleat".into(),
    ];
    // GNU tar uses status 1 for files changing during capture. Keep that
    // usable archive; status >= 2 is a failed capture and never replaces it.
    let script = format!(
        "tar --ignore-failed-read -cf - -- {}; flotilla_tar_status=$?; test \"$flotilla_tar_status\" -le 1",
        paths.iter().map(|p| runner::quote(p)).collect::<Vec<_>>().join(" ")
    );
    let result = runner.run_to_file("sh", &["-c", &script], Path::new("/"), &temporary).await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(temporary).await;
        return Err(error);
    }
    tokio::fs::rename(temporary, target).await.map_err(error)
}
impl DrydockEnvironment {
    async fn connect(&self) -> Result<&Arc<dyn reach::Guest>, String> {
        self.connection
            .get_or_try_init(|| async {
                let keydir = key_dir(&self.directory, &self.id);
                self.connector.connect(&*self.api, &self.instance, &self.backing, keydir.join("id_ed25519")).await
            })
            .await
    }
}
#[async_trait]
impl ProvisionedEnvironment for DrydockEnvironment {
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
        Some(self.descriptor.clone())
    }

    async fn observe(&self) -> ProviderObservation {
        if let Some(state) = self.watch.state.read().expect("observation lock").clone() {
            return ProviderObservation::new(state);
        }
        let state = match self.api.inspect(&self.instance, &self.backing).await {
            Ok(e) => observation(e.state),
            Err(::drydock::client::Error::Protocol(wire::ProtocolError::NotFound { .. })) => {
                ObservationState::Gone { cause: GoneCause::MissingOnList }
            }
            Err(e) => ObservationState::Unknown { reason: UnknownReason::ProviderApiError { message: api_error(e) } },
        };
        ProviderObservation::new(state)
    }
    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        let output = self.runner().run("env", &["-0"], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default).await?;
        Ok(output.split('\0').filter_map(|entry| entry.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        match self.connection.get() {
            Some(connection) => connection.runner(),
            None => Arc::new(InventoryRunner),
        }
    }
    async fn destroy(&self) -> Result<(), String> {
        self.api.destroy(&self.instance, &self.backing).await.map_err(api_error)?;
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
            Err(e) => return Err(error(e)),
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
