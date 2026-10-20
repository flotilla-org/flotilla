//! Dummy environments for load testing (#3206).
//!
//! A dummy-class provider instance that realises per-vessel environments
//! without containers. Provisioning waits a configured delay, fails at a
//! configured rate (drawn deterministically from the seed and environment ID),
//! and otherwise returns a handle whose command runner is an in-memory world:
//! files written by provisioning are kept, `test` and `cat` read them back, and
//! every other program is absent. Its environment marks itself with
//! [`DUMMY_FLEET_ENV`], so discovery inside it selects the other dummy
//! providers (terminal pool, agent adapter, VCS) and nothing real.
//!
//! The provider is selected like any other instance: a PlacementPolicy labels
//! its environments with [`super::ENVIRONMENT_PROVIDER_INSTANCE_LABEL`] set to
//! [`DUMMY_ENVIRONMENT_INSTANCE`].

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
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

/// `$0` of the agent exit-receipt batch script run by `flotilla_agent::process`.
const EXIT_RECEIPT_BATCH: &str = "flotilla-agent-exits";

/// Registry key of the dummy provider instance.
pub const DUMMY_ENVIRONMENT_INSTANCE: &str = "dummy";

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
    cleanup: Option<DummyEnvironmentCleanup>,
    owner: Arc<()>,
    handles: Mutex<HashMap<EnvironmentId, Arc<DummyEnvironment>>>,
}

impl DummyEnvironmentProvider {
    pub fn new(behaviour: DummyEnvironmentBehaviour) -> Self {
        Self { behaviour, cleanup: None, owner: Arc::new(()), handles: Mutex::new(HashMap::new()) }
    }

    pub fn with_cleanup(mut self, cleanup: DummyEnvironmentCleanup) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, HashMap<EnvironmentId, Arc<DummyEnvironment>>> {
        self.handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        super::ProviderKind::new(super::EnvironmentClass::Dummy, "dummy")
    }
    fn capabilities(&self) -> super::ProviderCapabilities {
        Self::capability_declaration()
    }

    fn legacy_input(&self) -> Option<EnvironmentInput> {
        Some(EnvironmentInput::Docker)
    }

    fn local_image_cache(&self) -> Option<&dyn LocalImageCache> {
        Some(self)
    }

    fn detection(&self) -> Option<&dyn super::EnvironmentDetection> {
        Some(self)
    }

    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        let docker = spec.docker.as_ref().filter(|_| spec.host_direct.is_none()).ok_or("dummy provider requires legacy image inputs")?;
        Ok(PreparedEnvironment::new(&self.owner, docker.image.clone()))
    }

    async fn provision(&self, id: EnvironmentId, prepared: &PreparedEnvironment, opts: ProvisionOpts) -> Result<EnvironmentHandle, String> {
        let image = prepared.get::<String>(&self.owner)?.clone();
        self.handles().retain(|_, handle| !handle.destroyed.load(Ordering::Acquire));
        if let Some(existing) = self.handles().get(&id) {
            return Ok(Arc::clone(existing) as EnvironmentHandle);
        }
        if !self.behaviour.realise_delay.is_zero() {
            tokio::time::sleep(self.behaviour.realise_delay).await;
        }
        if dummy_draw(self.behaviour.seed, "environment-failure", id.as_str()) < self.behaviour.failure_rate {
            return Err(format!("dummy environment {id} failed to realise (configured failure rate)"));
        }
        let mut environment: HashMap<String, String> = opts.tokens.into_iter().collect();
        environment.insert(DUMMY_FLEET_ENV.into(), "1".into());
        environment.insert("FLOTILLA_ENVIRONMENT_ID".into(), id.to_string());
        let handle = Arc::new(DummyEnvironment {
            container_name: format!("flotilla-dummy-{id}"),
            local_image_id: dummy_image_id(&image),
            image: ImageId::new(image),
            mounts: opts.provisioned_mounts,
            environment,
            runner: Arc::new(DummyCommandRunner::default()),
            id: id.clone(),
            destroyed: AtomicBool::new(false),
            cleanup: self.cleanup.clone(),
        });
        Ok(Arc::clone(self.handles().entry(id).or_insert(handle)) as EnvironmentHandle)
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(self
            .handles()
            .values()
            .filter(|handle| !handle.destroyed.load(Ordering::Acquire))
            .map(|handle| Arc::clone(handle) as EnvironmentHandle)
            .collect())
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
    cleanup: Option<DummyEnvironmentCleanup>,
    id: EnvironmentId,
    image: ImageId,
    local_image_id: String,
    container_name: String,
    mounts: Vec<ProvisionedMount>,
    environment: HashMap<String, String>,
    runner: Arc<DummyCommandRunner>,
}

#[async_trait]
impl ProvisionedEnvironment for DummyEnvironment {
    fn capabilities(&self) -> super::ProviderCapabilities {
        DummyEnvironmentProvider::capability_declaration()
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
            super::ObservationState::Running
        })
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(self.environment.clone())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner) as Arc<dyn CommandRunner>
    }
    async fn destroy(&self) -> Result<(), String> {
        if !self.destroyed.swap(true, Ordering::AcqRel) {
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
    async fn detect(&self, _image: Option<&str>, binary: &str, _args: &[&str], _scratch: &Path) -> Result<CommandOutput, String> {
        Ok(absent(binary))
    }
}

impl DummyEnvironmentProvider {
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
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "host".into(),
                image: image.into(),
                image_build_ref: None,
                declared_agent_adapters: Default::default(),
                required_agent_adapters: Default::default(),
                pull_policy: Default::default(),
                memory_policy: Default::default(),
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
