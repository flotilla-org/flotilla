//! Provider-neutral Drydock adapter. The composition root owns host supervision;
//! this adapter accepts the same ProviderApi for embedded and socket-backed use.
mod observations;
mod reach;
mod runner;
use super::*;
use crate::environment::runner::{contained_credential_exclusions, CONTAINED_CODEX_HOME, CONTAINED_WRITABLE_CONFIG_BASE};
use ::drydock::api::ProviderApi;
use drydock_protocol as wire;
use flotilla_resources::CLOUD_ADOPTION_RETRY_PREFIX;
use runner::{error, AgentRunner};
const CHECKPOINT_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
use sha2::Digest;
use std::{collections::BTreeMap, sync::RwLock, time::Duration};

pub struct DrydockEnvironmentProvider {
    api: Arc<dyn ProviderApi>,
    connector: Arc<dyn reach::Connector>,
    instance: String,
    module: String,
    lab_reach: Option<(String, u16)>,
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
            lab_reach: None,
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
            env: RwLock::new(env),
            lab_reach: self.lab_reach.clone(),
            capabilities: self.capabilities(),
            directory: self.directory.clone(),
            created_for_setup: std::sync::atomic::AtomicBool::new(false),
            checkpoint: std::sync::Mutex::new(None),
            connection: Default::default(),
            connecting: Default::default(),
            assets: Default::default(),
            checkpoint_error: Default::default(),
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
        let public = if keydir.join("id_ed25519.pub").is_file() && keydir.join("id_ed25519").is_file() {
            tokio::fs::read_to_string(keydir.join("id_ed25519.pub")).await.map_err(error)?
        } else {
            // An existing backing still trusts the original key. Regenerating
            // here cannot rotate that key through label-idempotent provision.
            let backings = self.api.list(&self.instance).await.map_err(|cause| {
                format!("{CLOUD_ADOPTION_RETRY_PREFIX} cannot verify backing before SSH key repair: {}", api_error(cause))
            })?;
            if backings.iter().any(|backing| backing.label.as_deref() == Some(id.as_str())) {
                return Err(format!("{CLOUD_ADOPTION_RETRY_PREFIX} SSH keypair incomplete or missing for adopted backing; restore the controller key files; public-key rotation on adoption is unsupported"));
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
        let response = self
            .api
            .request(
                http::Method::POST,
                &["v1", "providers", &self.instance, "environments"],
                &[],
                Some(&serde_json::to_value(request).map_err(|e| e.to_string())?),
            )
            .await
            .map_err(api_error)?;
        if !(200..300).contains(&response.status) {
            return Err(api_error(::drydock::client::Error::Protocol(serde_json::from_value(response.body).map_err(|e| e.to_string())?)));
        }
        let created = response.status == 201;
        let environment: wire::Environment = serde_json::from_value(response.body).map_err(|e| e.to_string())?;
        let mut env = prepared.spec.env.clone();
        env.extend(opts.tokens);
        // Retired control entries may remain in preceding-generation records.
        // They are never guest variables, even while those records decode.
        env.retain(|name, _| {
            !matches!(
                name.as_str(),
                "FLOTILLA_REQUIRED_AGENT_ADAPTERS"
                    | "FLOTILLA_CLOUD_IMAGE_NEEDS"
                    | "FLOTILLA_DRYDOCK_LAB_FORWARD"
                    | "FLOTILLA_CREDENTIAL_REFS"
                    | "FLOTILLA_CREDENTIAL_SCOPES"
                    | "FLOTILLA_CREDENTIAL_PERMISSIONS"
                    | "FLOTILLA_CREW_SKILLS"
            )
        });
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
        let active = self.active.lock().await.get(&id).and_then(std::sync::Weak::upgrade);
        if let Some(handle) = active {
            if handle.backing == environment.id && handle.image.as_str() == prepared.spec.image {
                handle.created_for_setup.store(false, std::sync::atomic::Ordering::Release);
                let setup = async {
                    let connection = handle.connect().await?;
                    for tool in opts.tools {
                        for asset in tool.assets {
                            connection.deliver(asset.clone()).await?;
                            handle.remember_asset(asset);
                        }
                    }
                    // Re-adoption must apply rotated tokens and tool environments.
                    connection.configure(env.clone()).await?;
                    *handle.env.write().expect("environment") = env;
                    Ok::<_, String>(())
                }
                .await;
                setup.map_err(|cause| format!("{CLOUD_ADOPTION_RETRY_PREFIX} {cause}"))?;
                return Ok(handle);
            }
        }
        let handle = self.handle(environment, prepared.spec.image.clone(), env)?;
        handle.created_for_setup.store(created, std::sync::atomic::Ordering::Release);
        let setup = async {
            let connection = handle.connect().await?;
            for tool in opts.tools {
                for asset in tool.assets {
                    connection.deliver(asset.clone()).await?;
                    handle.remember_asset(asset);
                }
            }
            let environment = handle.env.read().expect("environment").clone();
            connection.configure(environment).await?;
            connection.configure_lab(self.lab_reach.clone()).await?;
            connection
                .runner()
                .run(
                    "mkdir",
                    &["-p", &handle.capabilities.workspace_root.to_string_lossy()],
                    Path::new("/"),
                    &flotilla_provider_runtime::ChannelLabel::Default,
                )
                .await?;
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
                    let result = async {
                        let connection = handle.connect().await?;
                        write_checkpoint(connection.runner(), &checkpoint, &root).await
                    }
                    .await;
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
                return Err(format!("{CLOUD_ADOPTION_RETRY_PREFIX} {cause}"));
            }
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
    env: RwLock<BTreeMap<String, String>>,
    lab_reach: Option<(String, u16)>,
    capabilities: ProviderCapabilities,
    descriptor: flotilla_resources::EnvironmentProviderDescriptor,
    created_for_setup: std::sync::atomic::AtomicBool,
    checkpoint: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    directory: PathBuf,
    connection: RwLock<Option<Arc<dyn reach::Guest>>>,
    connecting: tokio::sync::Mutex<()>,
    assets: RwLock<Vec<EnvironmentToolAsset>>,
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
async fn write_checkpoint(runner: Arc<dyn CommandRunner>, target: &Path, root: &Path) -> Result<(), String> {
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
    tokio::fs::rename(temporary, target).await.map_err(error)
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
        .map(|p| format!("--exclude={}", runner::quote(p)))
        .collect::<Vec<_>>()
        .join(" ");
    let paths = paths.iter().map(|p| runner::quote(p)).collect::<Vec<_>>().join(" ");
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

impl DrydockEnvironment {
    fn remember_asset(&self, asset: EnvironmentToolAsset) {
        let mut assets = self.assets.write().expect("assets");
        assets.retain(|previous| previous.environment_path != asset.environment_path);
        assets.push(asset);
    }
    async fn connect(&self) -> Result<Arc<dyn reach::Guest>, String> {
        let _guard = self.connecting.lock().await;
        let cached = self.connection.read().expect("connection").clone();
        if let Some(connection) = cached {
            if !connection.closed().await {
                return Ok(connection);
            }
        }
        self.connection.write().expect("connection").take();
        let keydir = key_dir(&self.directory, &self.id);
        let connection = self.connector.connect(&*self.api, &self.instance, &self.backing, keydir.join("id_ed25519")).await?;
        let assets = self.assets.read().expect("assets").clone();
        for asset in assets {
            connection.deliver(asset).await?;
        }
        let environment = self.env.read().expect("environment").clone();
        connection.configure(environment).await?;
        connection.configure_lab(self.lab_reach.clone()).await?;
        *self.connection.write().expect("connection") = Some(connection.clone());
        Ok(connection)
    }
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
        Some(self.descriptor.clone())
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
        // Running backing plus usable guest transport is the readiness contract.
        // A failed SSH reconnect is Unknown, never evidence that work is Gone.
        if matches!(state, ObservationState::Running) && !self.image.as_str().is_empty() {
            if let Err(message) = self.connect().await {
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
        match self.connection.read().expect("connection").as_ref() {
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

#[cfg(test)]
mod conformance_test;
