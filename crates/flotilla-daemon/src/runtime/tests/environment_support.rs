use super::*;
use flotilla_discovery_api::{ProviderCategory, ProviderDescriptor};
use flotilla_environment::environment::EnvironmentProvisionError;

pub(super) fn fixed_environment_tools(state_dir: impl Into<PathBuf>) -> EnvironmentToolProvisioner {
    EnvironmentToolProvisioner::fixed(
        DaemonHostPath::new("/opt/flotilla/bin/flotilla"),
        DaemonHostPath::new("/tmp/flotilla.sock"),
        DaemonHostPath::new("/opt/flotilla/bin/cleat"),
        DaemonHostPath::new("/opt/flotilla/lib/libghostty-vt.so.0"),
        state_dir.into(),
    )
}

pub(super) fn write_test_skill_sources(root: &Path) -> PathBuf {
    let skills = root.join("generation/skills");
    fs::create_dir_all(&skills).expect("create skill source manifest directory");
    fs::write(
            skills.join(".flotilla-sources.json"),
            r#"{"schema_version":5,"sources":[{"name":"mattpocock-skills","repository":"https://github.com/flotilla-org/mattpocock-skills.git","revision":"1111111111111111111111111111111111111111"},{"name":"rjw-skills","repository":"https://github.com/rjwittams/rjw-skills.git","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","paths":["plugins/rjw-sdlc/skills"]}]}"#,
        )
        .expect("write skill source manifest");
    skills
}

pub(super) struct SshProvisioningRecordingRunner {
    pub(super) commands: Arc<StdMutex<Vec<String>>>,
}

#[async_trait]
impl CommandRunner for SshProvisioningRecordingRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.commands.lock().expect("command log").push(cmd.to_string());
        ProcessCommandRunner.run(cmd, args, cwd, label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.commands.lock().expect("command log").push(cmd.to_string());
        ProcessCommandRunner.run_output(cmd, args, cwd, label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        ProcessCommandRunner.write_file(path, content).await
    }

    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        ProcessCommandRunner.write_file_from(source, destination).await
    }
}

/// A real skill source for launch scenarios: a local Git repository holding `name`,
/// the generation supply that authorizes it, and the crew's frozen selection of it.
pub(super) struct LocalSkillSource {
    pub(super) supply: PathBuf,
    pub(super) selected: flotilla_resources::SkillCatalogEntry,
}

impl LocalSkillSource {
    pub(super) fn new(root: &Path, name: &str) -> Self {
        let repository = root.join("skill-source");
        fs::create_dir_all(repository.join("skills").join(name)).expect("skill source");
        fs::write(repository.join("skills").join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: test\n---\n"))
            .expect("skill");
        // Git subprocess boundary: the controller's host-side fetch reads this repository.
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args)
                .current_dir(&repository)
                .output()
                .expect("spawn git");
            assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8(output.stdout).expect("utf-8").trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "pin"]);
        let revision = git(&["rev-parse", "HEAD"]);
        let supply = root.join("generation/skills");
        fs::create_dir_all(&supply).expect("supply");
        let source = repository.display().to_string();
        fs::write(
            supply.join(".flotilla-sources.json"),
            serde_json::json!({"schema_version": 5, "sources": [{"name": "local", "repository": source, "revision": revision}]})
                .to_string(),
        )
        .expect("supply manifest");
        let selected = flotilla_resources::SkillCatalogEntry {
            source: "local".into(),
            repository: source,
            revision,
            name: name.into(),
            path: format!("skills/{name}"),
        };
        Self { supply, selected }
    }

    /// The controller's host-side acquisition, over the real Git CLI. Its Git commands
    /// skip the host Git config guard, whose probes share a process-wide budget of eight
    /// that the many launch scenarios running at once would exhaust; the guard has its own
    /// tests, and these repositories are the test's own.
    pub(super) fn trees(root: &Path) -> flotilla_credentials::SkillTrees {
        flotilla_credentials::SkillTrees::new(
            Arc::new(flotilla_vcs::vcs::FlotillaVcs::object_cache(Arc::new(rigging_process::ProcessRunner::new(OwnRepositories)))),
            root.join("skill-source-cache"),
        )
    }
}

/// A guard that admits every command: the launch scenarios' skill sources are repositories
/// the test created.
struct OwnRepositories;

#[async_trait]
impl rigging_process::CommandGuard for OwnRepositories {
    fn scratch_namespace(&self) -> &str {
        "flotilla"
    }
    async fn check(&self, _: &str, _: &[&str], _: &Path) -> Result<(), String> {
        Ok(())
    }
}

/// The vessel agent's content cache, modelled for launch scenarios (#3663): a pushed
/// archive is checked against its digest and its files listed, and each link into the
/// cache is recorded. Where the agent keeps an entry is its own; nothing here names it.
#[derive(Default)]
pub(super) struct ContentCache {
    entries: StdMutex<BTreeMap<String, Vec<String>>>,
    links: StdMutex<BTreeMap<PathBuf, String>>,
}

impl ContentCache {
    /// The files of the entry `path` is linked to, if it is linked to one.
    pub(super) fn linked_files(&self, path: impl AsRef<Path>) -> Option<Vec<String>> {
        let digest = self.links.lock().expect("links").get(path.as_ref()).cloned()?;
        self.entries.lock().expect("entries").get(&digest).cloned()
    }

    pub(super) fn push(&self, sha256: &str, file: &Path, unpack: flotilla_resources::PinUnpack) -> Result<(), String> {
        use sha2::Digest;
        let bytes = fs::read(file).map_err(|error| error.to_string())?;
        if format!("{:x}", sha2::Sha256::digest(&bytes)) != sha256 {
            return Err(format!("pushed bytes do not hash to {sha256}"));
        }
        let files = match unpack {
            flotilla_resources::PinUnpack::TarGz => archive_files(&bytes),
            other => panic!("the content cache model unpacks only tar.gz, not {other:?}"),
        };
        self.entries.lock().expect("entries").insert(sha256.to_string(), files);
        Ok(())
    }

    pub(super) fn link(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
        assert!(member.as_os_str().is_empty(), "skills link the whole entry");
        if !self.entries.lock().expect("entries").contains_key(sha256) {
            return Err(format!("no cache entry {sha256}"));
        }
        self.links.lock().expect("links").insert(path.to_path_buf(), sha256.to_string());
        Ok(())
    }

    /// The entry `path` is linked into, while the cache still holds it.
    pub(super) fn linked(&self, path: &Path) -> Option<String> {
        let digest = self.links.lock().expect("links").get(path).cloned()?;
        self.entries.lock().expect("entries").contains_key(&digest).then_some(digest)
    }
}

/// The regular files of a gzipped tar, by path.
pub(super) fn archive_files(bytes: &[u8]) -> Vec<String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut files = archive
        .entries()
        .expect("archive entries")
        .map(|entry| entry.expect("archive entry"))
        .filter(|entry| entry.header().entry_type().is_file())
        .map(|entry| entry.path().expect("entry path").display().to_string())
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// A contained vessel's agent: its file facilities run through the vessel's runner, as
/// the in-process agent runs them, and its content cache is `cache`.
struct CachedAgent {
    files: flotilla_environment::environment::vessel_ops::RunnerVesselAgent,
    cache: Arc<ContentCache>,
    forwards: Arc<StdMutex<Vec<ControllerForward>>>,
}

/// A controller forward an agent was asked to serve: its guest socket, guest loopback
/// address and controller socket.
pub(super) type ControllerForward = (PathBuf, Option<std::net::SocketAddr>, PathBuf);

#[async_trait]
impl flotilla_environment::environment::VesselAgent for CachedAgent {
    async fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    async fn forward_socket(&self, guest: &Path) -> Result<PathBuf, String> {
        self.files.forward_socket(guest).await
    }
    async fn push_tools(&self, tools: &[flotilla_environment::environment::EnvironmentTool]) -> Result<(), String> {
        if tools.is_empty() {
            return Ok(());
        }
        self.files.push_tools(tools).await
    }
    async fn forward_controller_socket(&self, guest: &Path, tcp: Option<std::net::SocketAddr>, controller: &Path) -> Result<(), String> {
        self.forwards.lock().expect("forwards").push((guest.into(), tcp, controller.into()));
        Ok(())
    }
    fn commands(&self) -> Arc<dyn CommandRunner> {
        self.files.commands()
    }
    async fn writable_base(&self) -> Result<PathBuf, String> {
        self.files.writable_base().await
    }
    async fn stat(&self, path: &Path) -> Result<Option<flotilla_environment::environment::EntryKind>, String> {
        self.files.stat(path).await
    }
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String> {
        self.files.create_dir(path, mode).await
    }
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        self.files.read(path, limit).await
    }
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        self.files.write(path, bytes, mode).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String> {
        self.files.symlink(path, target).await
    }
    async fn push_artifact(&self, sha256: &str, file: &Path, unpack: flotilla_resources::PinUnpack) -> Result<(), String> {
        self.cache.push(sha256, file, unpack)
    }
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
        self.cache.link(path, sha256, member)
    }
    async fn linked_artifact(&self, path: &Path) -> Result<Option<String>, String> {
        Ok(self.cache.linked(path))
    }
}

/// A contained vessel whose agent answers, with a content cache: the handle launch finds
/// among the controller's provisioned environments.
pub(super) struct CachedInterior {
    pub(super) inner: TestInteriorEnvironment,
    pub(super) cache: Arc<ContentCache>,
    pub(super) forwards: Arc<StdMutex<Vec<ControllerForward>>>,
}

#[async_trait]
impl ProvisionedEnvironment for CachedInterior {
    flotilla_discovery_testkit::injected_environment_handle!(agent);

    fn agent(&self) -> Arc<dyn flotilla_environment::environment::VesselAgent> {
        Arc::new(CachedAgent {
            files: flotilla_environment::environment::vessel_ops::RunnerVesselAgent(self.inner.runner()),
            cache: Arc::clone(&self.cache),
            forwards: Arc::clone(&self.forwards),
        })
    }

    fn id(&self) -> &EnvironmentId {
        self.inner.id()
    }

    fn image(&self) -> &ImageId {
        self.inner.image()
    }

    fn container_name(&self) -> Option<&str> {
        self.inner.container_name()
    }

    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        Vec::new()
    }

    async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
        self.inner.observe().await
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        self.inner.env_vars().await
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        self.inner.runner()
    }

    async fn destroy(&self) -> Result<(), String> {
        self.inner.destroy().await
    }
}

pub(super) struct CredentialInteriorRunner(pub(super) DiscoveryMockRunner);

#[derive(Default)]
pub(super) struct PersistentPathRecordingRunner {
    pub(super) writes: StdMutex<Vec<(PathBuf, String)>>,
}

#[async_trait]
impl CommandRunner for PersistentPathRecordingRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        Ok(String::new())
    }

    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }

    async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from("/home/crew/flotilla"))
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.writes.lock().expect("writes lock").push((path.to_path_buf(), content.to_string()));
        Ok(())
    }
}

#[async_trait]
impl CommandRunner for CredentialInteriorRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "sh" && (args.contains(&"flotilla-stat") || args.contains(&"flotilla-is-file")) {
            // Nothing vessel delivery reads has landed in this stand-in's homes.
            Ok("none\n".to_string())
        } else if cmd == "mkdir"
                || cmd == "chmod"
                // This runner stands in for the contained process boundary;
                // real scratch copying and credential links are tested in agent_material.
                || (cmd == "sh" && args.contains(&"flotilla-harness-home"))
                || (cmd == "sh" && args.contains(&"flotilla-requester-home"))
                || (cmd == "sh" && args.contains(&"flotilla-create-dir"))
                || (cmd == "sh" && args.contains(&"flotilla-symlink"))
        {
            Ok(String::new())
        } else {
            self.0.run(cmd, args, cwd, label).await
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        if cmd == "sh" && args == ["-c", "command -v claude"] {
            let available = self.0.exists("claude", &[]).await;
            return Ok(CommandOutput { stdout: "/tools/claude".into(), stderr: String::new(), exit_code: Some(i32::from(!available)) });
        }
        self.0.run_output(cmd, args, cwd, label).await
    }

    async fn run_with_input(
        &self,
        _cmd: &str,
        _args: &[&str],
        _cwd: &Path,
        _label: &ChannelLabel,
        _input: &[u8],
    ) -> Result<String, String> {
        Ok("ok".to_string())
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.0.exists(cmd, args).await
    }

    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        self.0.path_exists(path).await
    }

    // As a container's runner does, it declares a persistent config base apart from its
    // scratch base, so harness homes stay on the container's own disk (#4012).
    async fn writable_scratch_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from("/tmp/flotilla"))
    }

    async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from("/home/crew/flotilla"))
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        self.0.ensure_file(path, content).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.0.write_file(path, content).await
    }

    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        assert!(matches!(mode, 0o600 | 0o700));
        self.0.write_file(path, content).await
    }
}

pub(super) struct TestInteriorEnvironment {
    pub(super) id: EnvironmentId,
    pub(super) image: ImageId,
    pub(super) runner: Arc<dyn CommandRunner>,
    pub(super) env_vars: HashMap<String, String>,
    pub(super) destroyed: Arc<AtomicBool>,
}

#[async_trait]
impl ProvisionedEnvironment for TestInteriorEnvironment {
    flotilla_discovery_testkit::injected_environment_handle!(agent);

    /// A container whose resident agent never answered: reconciliation pushes it no tools.
    fn agent(&self) -> Arc<dyn flotilla_environment::environment::VesselAgent> {
        Arc::new(flotilla_environment::environment::vessel_ops::UnavailableAgent("test interior".into()))
    }

    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
    }

    fn local_image_id(&self) -> Option<&str> {
        Some("sha256:test-interior")
    }

    fn container_name(&self) -> Option<&str> {
        Some("test-interior")
    }

    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        Vec::new()
    }

    async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
        flotilla_environment::environment::ProviderObservation::new(flotilla_environment::environment::ObservationState::Running)
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(self.env_vars.clone())
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }

    async fn destroy(&self) -> Result<(), String> {
        self.destroyed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

pub(super) struct TestUncertainEnvironment {
    pub(super) id: EnvironmentId,
    pub(super) status_error: String,
}

#[async_trait]
impl ProvisionedEnvironment for TestUncertainEnvironment {
    flotilla_discovery_testkit::injected_environment_handle!();

    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        static IMAGE: std::sync::LazyLock<ImageId> = std::sync::LazyLock::new(|| ImageId::new("contained-image"));
        &IMAGE
    }

    fn container_name(&self) -> Option<&str> {
        Some("uncertain-container")
    }

    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        Vec::new()
    }

    async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
        flotilla_environment::environment::ProviderObservation::new(flotilla_environment::environment::ObservationState::Unknown {
            reason: flotilla_environment::environment::UnknownReason::ProviderApiError { message: self.status_error.clone() },
        })
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(HashMap::new())
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::new(DiscoveryMockRunner::builder().build())
    }

    async fn destroy(&self) -> Result<(), String> {
        Ok(())
    }
}

pub(super) struct TestInteriorEnvironmentProvider {
    pub(super) handle: Mutex<Option<EnvironmentHandle>>,
}

#[async_trait]
impl EnvironmentProvider for TestInteriorEnvironmentProvider {
    flotilla_discovery_testkit::injected_environment_provider!();
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        _opts: &flotilla_environment::environment::PrepareOpts,
    ) -> Result<flotilla_environment::environment::PreparedEnvironment, String> {
        Ok(flotilla_environment::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_environment::environment::PreparedEnvironment,
        _opts: flotilla_environment::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, EnvironmentProvisionError> {
        self.handle.lock().await.take().ok_or_else(|| "test environment already created".into())
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(Vec::new())
    }

    async fn destroy(&self, _container_id: &str) -> Result<(), String> {
        Err("not used".to_string())
    }
}

pub(super) struct AdoptionEnvironmentProvider {
    pub(super) handles: Vec<EnvironmentHandle>,
}

#[async_trait]
impl EnvironmentProvider for AdoptionEnvironmentProvider {
    flotilla_discovery_testkit::injected_environment_provider!();
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        _opts: &flotilla_environment::environment::PrepareOpts,
    ) -> Result<flotilla_environment::environment::PreparedEnvironment, String> {
        Ok(flotilla_environment::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_environment::environment::PreparedEnvironment,
        _opts: flotilla_environment::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, EnvironmentProvisionError> {
        Err("not used".into())
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(self.handles.clone())
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        let handle = self
            .handles
            .iter()
            .find(|handle| handle.container_name() == Some(container_id))
            .ok_or_else(|| format!("container {container_id} not found"))?;
        handle.destroy().await
    }
}

pub(super) struct CapturingFailingEnvironmentProvider {
    pub(super) create_opts: Mutex<Option<CreateOpts>>,
    pub(super) prepared_auth: Mutex<Option<PreparedEnvironmentAuth>>,
}

#[async_trait]
impl EnvironmentProvider for CapturingFailingEnvironmentProvider {
    flotilla_discovery_testkit::injected_environment_provider!();
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        opts: &flotilla_environment::environment::PrepareOpts,
    ) -> Result<flotilla_environment::environment::PreparedEnvironment, String> {
        *self.prepared_auth.lock().await = Some(opts.prepared_auth.clone());
        Ok(flotilla_environment::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_environment::environment::PreparedEnvironment,
        opts: flotilla_environment::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, EnvironmentProvisionError> {
        *self.create_opts.lock().await = Some(CreateOpts {
            tokens: opts.tokens,
            working_directory: opts.working_directory,
            provisioned_mounts: opts.provisioned_mounts,
            tools: opts.tools,
            cpu_limit: opts.cpu_limit,
            image_pull_policy: Default::default(),
            prepared_auth: self.prepared_auth.lock().await.take().expect("prepared auth captured"),
        });
        Err("stop after capturing create options".into())
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Err("not used".to_string())
    }

    async fn destroy(&self, _container_id: &str) -> Result<(), String> {
        Err("not used".to_string())
    }
}

#[derive(Default)]
pub(super) struct RejectingRegistryRunner {
    pub(super) calls: AtomicUsize,
}

#[async_trait]
impl CommandRunner for RejectingRegistryRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err("registry preflight must not run while waiting for agent material".to_string())
    }

    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err("registry preflight must not run while waiting for agent material".to_string())
    }

    async fn run_with_input(
        &self,
        _cmd: &str,
        _args: &[&str],
        _cwd: &Path,
        _label: &ChannelLabel,
        _input: &[u8],
    ) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err("registry preflight must not run while waiting for agent material".to_string())
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RegistryPreflightOutcome {
    Success,
    LoginFailure,
    PullFailure,
    MissingStore,
}

// Stands in for the registry CLI process, retaining the exact preflight artifact.
pub(super) struct RegistryPreflightRunner {
    pub(super) outcome: RegistryPreflightOutcome,
    pub(super) calls: AtomicUsize,
    pub(super) directory: Mutex<Option<PathBuf>>,
}

#[async_trait]
impl CommandRunner for RegistryPreflightRunner {
    async fn run(&self, _cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.directory.lock().await =
            Some(PathBuf::from(args.get(1).expect("registry CLI must receive a config directory after --config")));
        if self.outcome == RegistryPreflightOutcome::PullFailure {
            Err("pull refused".to_string())
        } else {
            Ok(String::new())
        }
    }
    async fn run_with_input(&self, _cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.directory.lock().await =
            Some(PathBuf::from(args.get(1).expect("registry CLI must receive a config directory after --config")));
        assert_eq!(input, b"registry-secret");
        if self.outcome == RegistryPreflightOutcome::LoginFailure {
            Err("login refused".to_string())
        } else {
            Ok(String::new())
        }
    }
    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        Err("unused".to_string())
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

pub(super) struct ListingEnvironmentProvider {
    pub(super) handle: EnvironmentHandle,
}

#[async_trait]
impl EnvironmentProvider for ListingEnvironmentProvider {
    flotilla_discovery_testkit::injected_environment_provider!();
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        _opts: &flotilla_environment::environment::PrepareOpts,
    ) -> Result<flotilla_environment::environment::PreparedEnvironment, String> {
        Ok(flotilla_environment::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_environment::environment::PreparedEnvironment,
        _opts: flotilla_environment::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, EnvironmentProvisionError> {
        Err("not used".into())
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Err("failed to parse provisioned mount metadata: corrupt label".to_string())
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        if self.handle.container_name() != Some(container_id) {
            return Err(format!("container {container_id} not found"));
        }
        self.handle.destroy().await
    }
}

pub(super) fn register_host_adoption(registry: &mut ProviderRegistry) {
    use flotilla_environment::environment::host_direct::HostDirectEnvironmentProvider;
    // The injected runner stands in for host subprocesses; adopting and
    // detaching the host itself must never invoke it.
    registry.environment_providers.insert(
        "host-direct",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "host-direct"),
        Arc::new(HostDirectEnvironmentProvider::new(Arc::new(DiscoveryMockRunner::builder().build()), HashMap::new())),
    );
}

pub(super) fn passthrough_registry() -> Arc<ProviderRegistry> {
    use flotilla_discovery::registry::ProviderRegistry;
    use flotilla_terminal::passthrough::PassthroughTerminalPool;

    let mut registry = ProviderRegistry::new();
    register_host_adoption(&mut registry);
    registry.terminal_pools.insert(
        "passthrough",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "passthrough"),
        Arc::new(PassthroughTerminalPool),
    );
    Arc::new(registry)
}

pub(super) fn passthrough_registry_with_environment(handle: EnvironmentHandle) -> Arc<ProviderRegistry> {
    use flotilla_discovery::registry::ProviderRegistry;
    use flotilla_terminal::passthrough::PassthroughTerminalPool;

    let mut registry = ProviderRegistry::new();
    register_host_adoption(&mut registry);
    registry.terminal_pools.insert(
        "passthrough",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "passthrough"),
        Arc::new(PassthroughTerminalPool),
    );
    registry.environment_providers.insert(
        "docker",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        Arc::new(TestInteriorEnvironmentProvider { handle: Mutex::new(Some(handle)) }),
    );
    Arc::new(registry)
}

pub(super) async fn create_ready_docker_environment(
    daemon: &InProcessDaemon,
    name: &str,
    container_id: &str,
    declared_agent_adapters: BTreeSet<String>,
) {
    let host_ref = daemon.local_host_id().expect("local host identity").to_string();
    let environments = daemon.resource_backend().using::<Environment>(NAMESPACE);
    environments
        .create(
            &empty_meta(name),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_build_ref: None,
                    host_ref,
                    image: "contained-image".to_string(),
                    declared_agent_adapters,
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                }),
            },
        )
        .await
        .expect("create environment record");
    flotilla_store::apply_status_patch(
        &environments,
        name,
        &EnvironmentStatusPatch::MarkReady {
            configured_limits: None,
            docker_container_id: Some(container_id.to_string()),
            image_ref: Some("contained-image".to_string()),
            local_image_id: Some("sha256:test-interior".to_string()),
            registry_digest: None,
        },
    )
    .await
    .expect("mark environment ready");
}

pub(super) fn adoption_registry(handles: Vec<EnvironmentHandle>) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    register_host_adoption(&mut registry);
    registry.environment_providers.insert(
        "docker",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        Arc::new(AdoptionEnvironmentProvider { handles }),
    );
    Arc::new(registry)
}
