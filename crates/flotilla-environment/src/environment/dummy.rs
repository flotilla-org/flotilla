//! Dummy environments for load testing (#3206).
//!
//! A dummy-class provider instance that realises per-vessel environments
//! without containers. Provisioning waits a configured delay, fails at a
//! configured rate (drawn deterministically from the seed and environment ID),
//! and otherwise returns a handle whose command runner is an in-memory world:
//! files written by provisioning are kept, `test` and `cat` read them back, and
//! every other program is absent; detection finds only the dummy crew harness.
//! Its environment marks itself with [`DUMMY_FLEET_ENV`], so discovery inside
//! it selects the other dummy providers (terminal pool, agent adapter, VCS) and
//! nothing real.
//!
//! The provider is selected like any other instance: placement freezes
//! [`DUMMY_ENVIRONMENT_INSTANCE`] for a Docker kind on a host where it is the
//! only per-vessel provider. A cloud-sandbox-class instance,
//! [`DUMMY_CLOUD_ENVIRONMENT_INSTANCE`], stands in for a sandbox vendor: it
//! realises cloud-sandbox kinds and reports a provider descriptor for each
//! backing, as every cloud backing must.
//!
//! Tests act as the vendor through [`DummyEnvironmentProvider::vanish`],
//! [`DummyEnvironmentProvider::stop`], [`DummyEnvironmentProvider::resume`]
//! and [`DummyEnvironmentProvider::refuse_provisioning`]: what the provider
//! then reports is what the daemon observes, with no stored record touched.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, ImageId};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_resources::PlacedImageIdentity;

use super::{
    EnvironmentHandle, EnvironmentInput, EnvironmentProvider, LocalImageCache, PrepareOpts, PreparedEnvironment, ProvisionOpts,
    ProvisionedEnvironment, ProvisionedMount, RegistryAuth,
};

/// Environment variable that marks a dummy environment. Dummy providers inside
/// an environment activate only when its discovery bag asserts this variable.
pub const DUMMY_FLEET_ENV: &str = "FLOTILLA_DUMMY_FLEET";

/// The dummy crew's harness, the one program a dummy environment's detection finds.
pub const DUMMY_CREW_BINARY: &str = "flotilla-dummy-crew";

/// `$0` of the agent exit-receipt batch script run by `flotilla_agent::process`.
const EXIT_RECEIPT_BATCH: &str = "flotilla-agent-exits";

/// Registry key of the dummy provider instance.
pub const DUMMY_ENVIRONMENT_INSTANCE: &str = "dummy";

/// Registry key of the dummy cloud-sandbox provider instance.
pub const DUMMY_CLOUD_ENVIRONMENT_INSTANCE: &str = "dummy-cloud";

/// The vendor class a dummy instance stands in for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum DummyEnvironmentClass {
    /// Per-vessel environments on the daemon's host, as Docker realises them.
    #[default]
    Container,
    /// Per-vessel sandboxes a cloud vendor realises, reached through an account.
    CloudSandbox,
}

/// What the vendor holds, by environment, since the provider was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DummyEnvironmentCounts {
    /// Backings the vendor realised.
    pub provisioned: u64,
    /// Backings destroyed on the daemon's request.
    pub destroyed: u64,
    /// Backings the vendor deleted of its own accord.
    pub vanished: u64,
    /// Backings the vendor holds now, running or stopped.
    pub live: u64,
}

// A backing's vendor-side state, beside its requested destruction.
const RUNNING: u8 = 0;
const STOPPED: u8 = 1;
const VANISHED: u8 = 2;

/// Realisation behaviour, taken from the daemon's `[dummy_fleet]` settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DummyEnvironmentBehaviour {
    pub seed: u64,
    pub realise_delay: Duration,
    pub failure_rate: f64,
}

/// In-process providers model resources that a real container destroys with it.
pub type DummyEnvironmentCleanup = Arc<dyn Fn(&str) + Send + Sync>;

pub struct DummyEnvironmentProvider {
    behaviour: DummyEnvironmentBehaviour,
    class: DummyEnvironmentClass,
    instance: String,
    cleanup: Option<DummyEnvironmentCleanup>,
    owner: Arc<()>,
    handles: Mutex<HashMap<EnvironmentId, Arc<DummyEnvironment>>>,
    refusing: AtomicBool,
    provisioned: AtomicU64,
    destroyed_counter: Arc<AtomicU64>,
    vanished: AtomicU64,
}

impl DummyEnvironmentProvider {
    pub fn new(behaviour: DummyEnvironmentBehaviour) -> Self {
        Self {
            behaviour,
            class: DummyEnvironmentClass::Container,
            instance: DUMMY_ENVIRONMENT_INSTANCE.into(),
            cleanup: None,
            owner: Arc::new(()),
            handles: Mutex::new(HashMap::new()),
            refusing: AtomicBool::new(false),
            provisioned: AtomicU64::new(0),
            destroyed_counter: Arc::default(),
            vanished: AtomicU64::new(0),
        }
    }

    /// A cloud-sandbox-class instance registered as `instance`.
    pub fn cloud(behaviour: DummyEnvironmentBehaviour, instance: &str) -> Self {
        Self { class: DummyEnvironmentClass::CloudSandbox, instance: instance.into(), ..Self::new(behaviour) }
    }

    pub fn with_cleanup(mut self, cleanup: DummyEnvironmentCleanup) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, HashMap<EnvironmentId, Arc<DummyEnvironment>>> {
        self.handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The backing for `environment`, by environment ID or backing name.
    fn backing(&self, environment: &str) -> Option<Arc<DummyEnvironment>> {
        self.handles()
            .iter()
            .find(|(id, handle)| (id.as_str() == environment || handle.container_name == environment) && handle.held())
            .map(|(_, handle)| Arc::clone(handle))
    }

    /// The vendor deletes `environment`'s backing, as a sandbox vendor evicts
    /// one: it leaves the inventory, its handles report it gone, and the
    /// processes inside it end. False when the vendor holds no such backing.
    pub fn vanish(&self, environment: &str) -> bool {
        let Some(handle) = self.backing(environment) else { return false };
        if handle.vendor.swap(VANISHED, Ordering::AcqRel) == VANISHED {
            return false;
        }
        self.vanished.fetch_add(1, Ordering::Relaxed);
        self.handles().retain(|_, current| !Arc::ptr_eq(current, &handle));
        if let Some(cleanup) = &self.cleanup {
            cleanup(handle.id.as_str());
        }
        true
    }

    /// The vendor stops `environment`'s backing; it stays in the inventory.
    pub fn stop(&self, environment: &str) -> bool {
        self.backing(environment)
            .is_some_and(|handle| handle.vendor.compare_exchange(RUNNING, STOPPED, Ordering::AcqRel, Ordering::Acquire).is_ok())
    }

    /// The vendor resumes a backing it stopped.
    pub fn resume(&self, environment: &str) -> bool {
        self.backing(environment)
            .is_some_and(|handle| handle.vendor.compare_exchange(STOPPED, RUNNING, Ordering::AcqRel, Ordering::Acquire).is_ok())
    }

    /// While set, the vendor refuses to realise any new backing.
    pub fn refuse_provisioning(&self, refuse: bool) {
        self.refusing.store(refuse, Ordering::Release);
    }

    pub fn counts(&self) -> DummyEnvironmentCounts {
        DummyEnvironmentCounts {
            provisioned: self.provisioned.load(Ordering::Relaxed),
            destroyed: self.destroyed_counter.load(Ordering::Relaxed),
            vanished: self.vanished.load(Ordering::Relaxed),
            live: self.handles().values().filter(|handle| handle.held()).count() as u64,
        }
    }

    fn descriptor(&self, image: &str) -> Option<flotilla_resources::EnvironmentProviderDescriptor> {
        (self.class == DummyEnvironmentClass::CloudSandbox).then(|| flotilla_resources::EnvironmentProviderDescriptor {
            instance: self.instance.clone(),
            kind: self.kind(),
            image_digest: dummy_image_id(image),
            host_version: "dummy".into(),
            module_version: "dummy".into(),
            capabilities: self.capabilities(),
            agent_connection: None,
        })
    }
}

/// A stable draw in `[0, 1)` for one subject. FNV-1a keeps it reproducible
/// across processes and toolchains, unlike the standard library hasher.
pub fn dummy_draw(seed: u64, domain: &str, subject: &str) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for byte in domain.bytes().chain([0]).chain(subject.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // Finalise so nearby subjects spread across the interval.
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

fn dummy_image_id(image: &str) -> String {
    format!("sha256:{:016x}", (dummy_draw(0, "image", image) * u64::MAX as f64) as u64)
}

#[async_trait]
impl EnvironmentProvider for DummyEnvironmentProvider {
    async fn detect_readiness(&self) -> super::ProviderReadiness {
        super::ProviderReadiness::Ready
    }
    fn kind(&self) -> super::ProviderKind {
        let class = match self.class {
            DummyEnvironmentClass::Container => super::EnvironmentClass::Dummy,
            DummyEnvironmentClass::CloudSandbox => super::EnvironmentClass::CloudSandbox,
        };
        super::ProviderKind::new(class, "dummy")
    }
    fn capabilities(&self) -> super::ProviderCapabilities {
        Self::capability_declaration(self.class)
    }

    fn legacy_input(&self) -> Option<EnvironmentInput> {
        (self.class == DummyEnvironmentClass::Container).then_some(EnvironmentInput::Docker)
    }

    fn local_image_cache(&self) -> Option<&dyn LocalImageCache> {
        (self.class == DummyEnvironmentClass::Container).then_some(self as &dyn LocalImageCache)
    }

    fn detection(&self) -> Option<&dyn super::EnvironmentDetection> {
        Some(self)
    }

    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        let image = match self.class {
            DummyEnvironmentClass::Container => {
                &spec.docker.as_ref().filter(|_| spec.host_direct.is_none()).ok_or("dummy provider requires legacy image inputs")?.image
            }
            DummyEnvironmentClass::CloudSandbox => {
                &spec.cloud_sandbox.as_ref().ok_or("dummy cloud provider requires cloud sandbox inputs")?.image
            }
        };
        Ok(PreparedEnvironment::new(&self.owner, image.clone()))
    }

    async fn provision(
        &self,
        id: EnvironmentId,
        prepared: &PreparedEnvironment,
        opts: ProvisionOpts,
    ) -> Result<EnvironmentHandle, super::EnvironmentProvisionError> {
        let image = prepared.get::<String>(&self.owner)?.clone();
        self.handles().retain(|_, handle| handle.held());
        if let Some(existing) = self.handles().get(&id) {
            return Ok(Arc::clone(existing) as EnvironmentHandle);
        }
        if !self.behaviour.realise_delay.is_zero() {
            tokio::time::sleep(self.behaviour.realise_delay).await;
        }
        if self.refusing.load(Ordering::Acquire) {
            return Err(format!("dummy vendor refuses to provision {id}").into());
        }
        if dummy_draw(self.behaviour.seed, "environment-failure", id.as_str()) < self.behaviour.failure_rate {
            return Err(format!("dummy environment {id} failed to realise (configured failure rate)").into());
        }
        let mut environment: HashMap<String, String> = opts.tokens.into_iter().collect();
        environment.insert(DUMMY_FLEET_ENV.into(), "1".into());
        environment.insert("FLOTILLA_ENVIRONMENT_ID".into(), id.to_string());
        let handle = Arc::new(DummyEnvironment {
            container_name: format!("flotilla-dummy-{id}"),
            local_image_id: dummy_image_id(&image),
            descriptor: self.descriptor(&image),
            capabilities: self.capabilities(),
            image: ImageId::new(image),
            mounts: opts.provisioned_mounts,
            environment,
            runner: Arc::new(DummyCommandRunner::default()),
            id: id.clone(),
            destroyed: AtomicBool::new(false),
            vendor: AtomicU8::new(RUNNING),
            cleanup: self.cleanup.clone(),
            destroyed_count: Arc::clone(&self.destroyed_counter),
        });
        let mut handles = self.handles();
        let held = handles.entry(id).or_insert_with(|| {
            self.provisioned.fetch_add(1, Ordering::Relaxed);
            handle
        });
        Ok(Arc::clone(held) as EnvironmentHandle)
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(self.handles().values().filter(|handle| handle.held()).map(|handle| Arc::clone(handle) as EnvironmentHandle).collect())
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        let destroyed = self
            .handles()
            .iter()
            .filter(|(id, handle)| handle.container_name == container_id || id.as_str() == container_id)
            .map(|(_, handle)| Arc::clone(handle))
            .collect::<Vec<_>>();
        for handle in destroyed {
            handle.destroy().await?;
            // Keep failed handles discoverable for retry. Do not remove a new
            // handle if provisioning has already replaced the destroyed one.
            let mut handles = self.handles();
            if handles.get(&handle.id).is_some_and(|current| Arc::ptr_eq(current, &handle)) {
                handles.remove(&handle.id);
            }
        }
        Ok(())
    }
}

/// Every image is present: a dummy environment never pulls or builds.
#[async_trait]
impl LocalImageCache for DummyEnvironmentProvider {
    async fn inspect(&self, digest: &str) -> Result<Option<PlacedImageIdentity>, String> {
        Ok(Some(PlacedImageIdentity { local_image_id: dummy_image_id(digest), registry_digest: None }))
    }

    async fn inspect_reference(&self, reference: &str) -> Result<Option<PlacedImageIdentity>, String> {
        LocalImageCache::inspect(self, reference).await
    }

    async fn registry_available(&self, _reference: &str) -> Result<(), String> {
        Ok(())
    }

    async fn pull(&self, _reference: &str, _auth: Option<&RegistryAuth>) -> Result<(), String> {
        Ok(())
    }

    async fn inventory(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    async fn remove(&self, _digest: &str) -> Result<(), String> {
        Ok(())
    }
}

struct DummyEnvironment {
    destroyed: AtomicBool,
    /// The vendor's view of the backing: running, stopped or vanished.
    vendor: AtomicU8,
    cleanup: Option<DummyEnvironmentCleanup>,
    destroyed_count: Arc<AtomicU64>,
    descriptor: Option<flotilla_resources::EnvironmentProviderDescriptor>,
    capabilities: super::ProviderCapabilities,
    id: EnvironmentId,
    image: ImageId,
    local_image_id: String,
    container_name: String,
    mounts: Vec<ProvisionedMount>,
    environment: HashMap<String, String>,
    runner: Arc<DummyCommandRunner>,
}

impl DummyEnvironment {
    /// Whether the vendor still holds this backing.
    fn held(&self) -> bool {
        !self.destroyed.load(Ordering::Acquire) && self.vendor.load(Ordering::Acquire) != VANISHED
    }
}

#[async_trait]
impl ProvisionedEnvironment for DummyEnvironment {
    fn provider_descriptor(&self) -> Option<flotilla_resources::EnvironmentProviderDescriptor> {
        self.descriptor.clone()
    }
    fn capabilities(&self) -> super::ProviderCapabilities {
        self.capabilities.clone()
    }
    fn backing_id(&self) -> &str {
        &self.container_name
    }

    fn id(&self) -> &EnvironmentId {
        &self.id
    }
    fn image(&self) -> &ImageId {
        &self.image
    }
    fn local_image_id(&self) -> Option<&str> {
        Some(&self.local_image_id)
    }
    fn container_name(&self) -> Option<&str> {
        Some(&self.container_name)
    }
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        self.mounts.clone()
    }
    async fn observe(&self) -> super::ProviderObservation {
        super::ProviderObservation::new(if self.destroyed.load(Ordering::Acquire) {
            super::ObservationState::Gone { cause: super::GoneCause::DestroyedByRequest }
        } else {
            match self.vendor.load(Ordering::Acquire) {
                VANISHED => super::ObservationState::Gone { cause: super::GoneCause::EvictedByProvider },
                STOPPED => super::ObservationState::Stopped { cause: super::StoppedCause::PausedByProvider },
                _ => super::ObservationState::Running,
            }
        })
    }

    async fn detect_readiness(&self) -> super::ProviderReadiness {
        super::ProviderReadiness::Ready
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(self.environment.clone())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner) as Arc<dyn CommandRunner>
    }
    /// The dummy's scripted runner stands for its vessel, agent included.
    fn agent(&self) -> Arc<dyn super::VesselAgent> {
        Arc::new(super::vessel_ops::RunnerVesselAgent(self.runner()))
    }
    async fn destroy(&self) -> Result<(), String> {
        if !self.destroyed.swap(true, Ordering::AcqRel) && self.vendor.load(Ordering::Acquire) != VANISHED {
            self.destroyed_count.fetch_add(1, Ordering::Relaxed);
            if let Some(cleanup) = &self.cleanup {
                cleanup(self.id.as_str());
            }
        }
        Ok(())
    }
}

fn absent(program: &str) -> CommandOutput {
    CommandOutput { stdout: String::new(), stderr: format!("{program}: not present in a dummy environment"), exit_code: Some(127) }
}

fn succeeded(stdout: impl Into<String>) -> CommandOutput {
    CommandOutput { stdout: stdout.into(), stderr: String::new(), exit_code: Some(0) }
}

fn failed(stderr: impl Into<String>) -> CommandOutput {
    CommandOutput { stdout: String::new(), stderr: stderr.into(), exit_code: Some(1) }
}

/// The command world inside a dummy environment: an in-memory file tree and
/// the few file utilities that provisioning and teardown invoke. Anything
/// else is absent, so discovery detects no real binaries.
#[derive(Default)]
pub struct DummyCommandRunner {
    files: Mutex<HashMap<PathBuf, String>>,
    directories: Mutex<BTreeSet<PathBuf>>,
}

impl DummyCommandRunner {
    fn files(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, String>> {
        self.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn directories(&self) -> std::sync::MutexGuard<'_, BTreeSet<PathBuf>> {
        self.directories.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn has_path(&self, path: &Path) -> bool {
        self.files().contains_key(path) || self.directories().contains(path)
    }

    fn remove(&self, path: &Path) {
        self.files().retain(|file, _| !file.starts_with(path));
        self.directories().retain(|directory| !directory.starts_with(path));
    }

    fn store(&self, path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            self.directories().extend(parent.ancestors().map(Path::to_path_buf));
        }
        self.files().insert(path.to_path_buf(), content.to_string());
    }

    fn operands<'a>(args: &'a [&'a str]) -> impl Iterator<Item = &'a str> {
        args.iter().copied().filter(|arg| !arg.starts_with('-'))
    }

    fn execute(&self, cmd: &str, args: &[&str]) -> CommandOutput {
        match cmd {
            "true" | "chmod" | "chown" | "touch" | "sync" => succeeded(""),
            "mkdir" => {
                self.directories().extend(Self::operands(args).map(PathBuf::from));
                succeeded("")
            }
            "rm" | "rmdir" => {
                for path in Self::operands(args) {
                    self.remove(Path::new(path));
                }
                succeeded("")
            }
            "mv" => {
                let operands = Self::operands(args).collect::<Vec<_>>();
                if let [source, destination] = operands.as_slice() {
                    let moved = self.files().remove(Path::new(source));
                    if let Some(content) = moved {
                        self.store(Path::new(destination), &content);
                    }
                }
                succeeded("")
            }
            "test" => match args {
                [flag, path] if matches!(*flag, "-e" | "-f" | "-d") && self.has_path(Path::new(path)) => succeeded(""),
                _ => failed(""),
            },
            "cat" => {
                let mut stdout = String::new();
                for path in Self::operands(args) {
                    match self.files().get(Path::new(path)) {
                        Some(content) => stdout.push_str(content),
                        None => return failed(format!("cat: {path}: No such file or directory")),
                    }
                }
                succeeded(stdout)
            }
            "sh" | "bash" => match args {
                // The agent exit-receipt batch (flotilla_agent::process): one
                // `index\tvalue` line per path, `-` for an absent receipt.
                ["-c", _, EXIT_RECEIPT_BATCH, paths @ ..] => {
                    let files = self.files();
                    let lines = paths.iter().enumerate().map(|(index, path)| {
                        let value = files.get(Path::new(path)).map_or("-", |content| {
                            let content = content.trim();
                            if !content.is_empty() && content.bytes().all(|byte| byte.is_ascii_digit()) {
                                content
                            } else {
                                "E"
                            }
                        });
                        format!("{index}\t{value}\n")
                    });
                    succeeded(lines.collect::<String>())
                }
                // Other scripts (staging probes) observe an empty world: they
                // succeed and report nothing.
                _ => succeeded(""),
            },
            program => absent(program),
        }
    }
}

#[async_trait]
impl CommandRunner for DummyCommandRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        let output = self.execute(cmd, args);
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        Ok(self.execute(cmd, args))
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, _input: &[u8]) -> Result<String, String> {
        self.run(cmd, args, cwd, label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.execute(cmd, args).success()
    }

    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        Ok(self.has_path(path))
    }

    async fn same_filesystem(&self, _source: &Path, _destination: &Path) -> Result<bool, String> {
        Ok(true)
    }

    async fn writable_scratch_base(&self, _preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        Ok(fallback.to_path_buf())
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        if let Some(existing) = self.files().get(path) {
            return Ok(existing.clone());
        }
        self.store(path, content);
        Ok(content.to_string())
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.store(path, content);
        Ok(())
    }

    async fn write_file_with_mode(&self, path: &Path, content: &str, _mode: u32) -> Result<(), String> {
        self.store(path, content);
        Ok(())
    }

    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let content =
            self.files().get(source).cloned().ok_or_else(|| format!("{} is absent in the dummy environment", source.display()))?;
        tokio::fs::write(destination, content).await.map_err(|error| format!("copy {} to the host: {error}", source.display()))
    }

    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let content = tokio::fs::read_to_string(source)
            .await
            .map_err(|error| format!("read {} for the dummy environment: {error}", source.display()))?;
        self.store(destination, &content);
        Ok(())
    }
}

#[async_trait]
impl super::EnvironmentDetection for DummyEnvironmentProvider {
    async fn detect(
        &self,
        _image: Option<&str>,
        _context: &flotilla_provider_runtime::execution_context::ObservationContext,
        binary: &str,
        _args: &[&str],
        _scratch: &Path,
    ) -> Result<CommandOutput, String> {
        Ok(match binary {
            DUMMY_CREW_BINARY => succeeded(format!("{DUMMY_CREW_BINARY} 1.0.0")),
            _ => absent(binary),
        })
    }
}

impl DummyEnvironmentProvider {
    fn capability_declaration(class: DummyEnvironmentClass) -> super::ProviderCapabilities {
        // A sandbox vendor's guest shares nothing with the daemon's host.
        let (material_modes, checkout_strategies) = match class {
            DummyEnvironmentClass::Container => (
                vec![super::MaterialMode::Frozen, super::MaterialMode::Pushed, super::MaterialMode::Shared],
                vec![super::CheckoutStrategy::HostWorktree, super::CheckoutStrategy::HostClone, super::CheckoutStrategy::CloneInVessel],
            ),
            DummyEnvironmentClass::CloudSandbox => {
                (vec![super::MaterialMode::Frozen, super::MaterialMode::Pushed], vec![super::CheckoutStrategy::CloneInVessel])
            }
        };
        super::ProviderCapabilities {
            material_modes,
            checkout_strategies,
            workspace_root: "/workspace".into(),
            reference_root: Some("/ref/repo".into()),
            fallback_runner: true,
            receives_host_tools: false,
            per_vessel: true,
            egress_route: super::EgressRoute::Open,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec};

    use super::*;

    fn docker_spec(image: &str) -> EnvironmentSpec {
        EnvironmentSpec {
            cloud_sandbox: None,
            provider_address: None,
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "host".into(),
                image: image.into(),
                image_build_ref: None,
                declared_agent_adapters: Default::default(),
                required_agent_adapters: Default::default(),
                pull_policy: Default::default(),
                mounts: Vec::new(),
                env: Default::default(),
            }),
        }
    }

    async fn provision(provider: &DummyEnvironmentProvider, id: &str) -> Result<EnvironmentHandle, String> {
        let prepared = provider.prepare(&docker_spec("dummy/crew:latest"), &PrepareOpts::default()).await?;
        provider
            .provision(
                EnvironmentId::new(id),
                &prepared,
                ProvisionOpts { tokens: vec![("CREW".into(), "coder".into())], ..Default::default() },
            )
            .await
            .map_err(|error| error.to_string())
    }

    // Destroying a dummy container destroys its in-process child resources
    // once, by environment identity, including when addressed by container ID.
    #[tokio::test]
    async fn destroying_an_environment_cleans_up_its_in_process_children() {
        let removed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&removed);
        let provider = DummyEnvironmentProvider::new(DummyEnvironmentBehaviour::default())
            .with_cleanup(Arc::new(move |id| captured.lock().expect("removed").push(id.to_string())));
        let one = provision(&provider, "one").await.expect("one");
        provision(&provider, "two").await.expect("two");
        one.destroy().await.expect("destroy active handle");
        assert_eq!(one.observe().await.state, super::super::ObservationState::Gone { cause: super::super::GoneCause::DestroyedByRequest });
        provider.destroy("flotilla-dummy-one").await.expect("destroy one");
        provider.destroy("one").await.expect("idempotent destroy");
        assert_eq!(*removed.lock().expect("removed"), vec!["one"]);
        assert_eq!(provider.list().await.expect("surviving environments").len(), 1);
    }

    // Behaviour: a test acting as the vendor changes only what the provider
    // reports. A vanished backing leaves the inventory, reports eviction and
    // ends its children; a stopped one stays listed until resumed; a refusing
    // vendor realises nothing new; the counts follow each.
    #[tokio::test]
    async fn the_vendor_vanishes_stops_resumes_and_refuses_backings() {
        let removed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&removed);
        let provider = DummyEnvironmentProvider::new(DummyEnvironmentBehaviour::default())
            .with_cleanup(Arc::new(move |id| captured.lock().expect("removed").push(id.to_string())));
        let gone = provision(&provider, "gone").await.expect("gone");
        let paused = provision(&provider, "paused").await.expect("paused");
        assert!(provider.vanish("flotilla-dummy-gone"), "addressed by backing name");
        assert!(!provider.vanish("gone"), "a vanished backing is no longer held");
        assert_eq!(gone.observe().await.state, super::super::ObservationState::Gone { cause: super::super::GoneCause::EvictedByProvider });
        assert!(EnvironmentProvider::inspect(&provider, &EnvironmentId::new("gone")).await.expect("inspect").is_none());
        assert_eq!(*removed.lock().expect("removed"), vec!["gone"]);

        assert!(provider.stop("paused"));
        assert_eq!(
            paused.observe().await.state,
            super::super::ObservationState::Stopped { cause: super::super::StoppedCause::PausedByProvider }
        );
        assert_eq!(provider.list().await.expect("list").len(), 1, "a stopped backing stays in the inventory");
        assert!(provider.resume("paused"));
        assert!(!provider.resume("paused"), "only a stopped backing resumes");
        assert_eq!(paused.observe().await.state, super::super::ObservationState::Running);

        provider.refuse_provisioning(true);
        assert!(provision(&provider, "refused").await.is_err());
        assert!(provision(&provider, "paused").await.is_ok(), "a held backing is still adopted");
        provider.refuse_provisioning(false);
        provision(&provider, "accepted").await.expect("accepted");
        paused.destroy().await.expect("destroy");
        assert!(!provider.vanish("paused"), "a destroyed backing cannot vanish");
        assert_eq!(provider.counts(), DummyEnvironmentCounts { provisioned: 3, destroyed: 1, vanished: 1, live: 1 });
    }

    // A cloud-class vendor takes cloud sandbox inputs, offers no local image
    // cache or legacy input, and describes each backing.
    #[tokio::test]
    async fn a_cloud_vendor_realises_described_sandboxes() {
        let provider = DummyEnvironmentProvider::cloud(DummyEnvironmentBehaviour::default(), DUMMY_CLOUD_ENVIRONMENT_INSTANCE);
        assert!(provider.prepare(&docker_spec("dummy/crew:latest"), &PrepareOpts::default()).await.is_err());
        let spec = EnvironmentSpec {
            cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
                image: "dummy/crew:latest".into(),
                image_build_ref: None,
                size: flotilla_resources::CloudSandboxSize { cpus: 1, memory_bytes: 1, disk_bytes: 1 },
                lifetime_cap_seconds: None,
                probe_needs: Default::default(),
                env: Default::default(),
            }),
            ..Default::default()
        };
        let prepared = provider.prepare(&spec, &PrepareOpts::default()).await.expect("prepare");
        let handle = provider.provision(EnvironmentId::new("env-cloud"), &prepared, Default::default()).await.expect("provision");
        let descriptor = handle.provider_descriptor().expect("a cloud backing is described");
        assert_eq!(descriptor.instance, DUMMY_CLOUD_ENVIRONMENT_INSTANCE);
        assert_eq!(descriptor.kind.class, super::super::EnvironmentClass::CloudSandbox);
        assert!(provider.legacy_input().is_none());
        assert!(provider.local_image_cache().is_none());
        assert_eq!(provider.capabilities().checkout_strategies, vec![super::super::CheckoutStrategy::CloneInVessel]);
    }

    #[test]
    fn draws_are_stable_and_spread_over_the_unit_interval() {
        assert_eq!(dummy_draw(7, "environment-failure", "env-1"), dummy_draw(7, "environment-failure", "env-1"));
        assert_ne!(dummy_draw(7, "environment-failure", "env-1"), dummy_draw(8, "environment-failure", "env-1"));
        let draws = (0..1000).map(|index| dummy_draw(3, "d", &format!("subject-{index}"))).collect::<Vec<_>>();
        assert!(draws.iter().all(|draw| (0.0..1.0).contains(draw)));
        let below_half = draws.iter().filter(|draw| **draw < 0.5).count();
        assert!((400..600).contains(&below_half), "draws should be roughly uniform, got {below_half} of 1000 below one half");
    }

    #[tokio::test]
    async fn provisioning_realises_a_marked_environment_without_a_container() {
        let provider = DummyEnvironmentProvider::new(DummyEnvironmentBehaviour::default());
        let handle = provision(&provider, "env-a").await.expect("dummy environment");
        let environment = handle.env_vars().await.expect("environment");
        assert_eq!(environment.get(DUMMY_FLEET_ENV).map(String::as_str), Some("1"));
        assert_eq!(environment.get("CREW").map(String::as_str), Some("coder"));
        assert_eq!(handle.observe().await.state, super::super::ObservationState::Running);
        assert!(!provider.receives_host_tools(), "dummy crews need no delivered cleat or CLI");
        assert!(handle.local_image_id().is_some());
        // Provisioning the same identity again returns the realised handle.
        let again = provision(&provider, "env-a").await.expect("idempotent provisioning");
        assert!(Arc::ptr_eq(&handle, &again));
        assert_eq!(provider.list().await.expect("list").len(), 1);
        provider.destroy(handle.container_name().expect("backing name")).await.expect("destroy");
        assert!(provider.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn the_failure_rate_refuses_exactly_the_drawn_environments() {
        let behaviour = DummyEnvironmentBehaviour { seed: 11, realise_delay: Duration::ZERO, failure_rate: 0.5 };
        let provider = DummyEnvironmentProvider::new(behaviour);
        let mut failures = 0;
        for index in 0..200 {
            let id = format!("env-{index}");
            let expected = dummy_draw(11, "environment-failure", &id) < 0.5;
            let result = provision(&provider, &id).await;
            assert_eq!(result.is_err(), expected, "{id}");
            failures += usize::from(expected);
        }
        assert!((60..140).contains(&failures), "half of the environments should fail, got {failures}");
        let none = DummyEnvironmentProvider::new(DummyEnvironmentBehaviour { failure_rate: 0.0, ..Default::default() });
        assert!(provision(&none, "env-0").await.is_ok());
        let all = DummyEnvironmentProvider::new(DummyEnvironmentBehaviour { failure_rate: 1.0, ..Default::default() });
        assert!(provision(&all, "env-0").await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn realisation_waits_for_the_configured_delay() {
        let behaviour = DummyEnvironmentBehaviour { realise_delay: Duration::from_secs(3), ..Default::default() };
        let provider = DummyEnvironmentProvider::new(behaviour);
        let started = tokio::time::Instant::now();
        provision(&provider, "env-slow").await.expect("delayed environment");
        assert!(started.elapsed() >= Duration::from_secs(3));
    }

    #[tokio::test]
    async fn the_runner_keeps_written_files_and_finds_no_programs() {
        let runner = DummyCommandRunner::default();
        let root = Path::new("/");
        runner.write_file(Path::new("/home/crew/.config/brief.md"), "brief").await.expect("write");
        assert!(runner.path_exists(Path::new("/home/crew/.config/brief.md")).await.expect("exists"));
        assert!(runner.path_exists(Path::new("/home/crew")).await.expect("parent directory"));
        assert_eq!(runner.run("cat", &["/home/crew/.config/brief.md"], root, &ChannelLabel::Default).await, Ok("brief".into()));
        runner.run("rm", &["-rf", "/home/crew"], root, &ChannelLabel::Default).await.expect("remove");
        assert!(!runner.path_exists(Path::new("/home/crew/.config/brief.md")).await.expect("removed"));
        runner.write_file(Path::new("/receipts/b"), "3\n").await.expect("receipt");
        let receipts = runner
            .run("sh", &["-c", "script", EXIT_RECEIPT_BATCH, "/receipts/a", "/receipts/b"], root, &ChannelLabel::Default)
            .await
            .expect("receipt batch");
        assert_eq!(receipts, "0\t-\n1\t3\n");
        let git = runner.run_output("git", &["--version"], root, &ChannelLabel::Default).await.expect("spawned");
        assert_eq!(git.exit_code, Some(127));
        assert!(!CommandRunner::exists(&runner, "claude", &["--version"]).await);
    }
}
