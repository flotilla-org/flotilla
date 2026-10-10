use super::*;

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

pub(super) fn write_test_credentialed_skill_sources(root: &Path) -> PathBuf {
    let skills = root.join("generation/credentialed-skills");
    fs::create_dir_all(&skills).expect("create credentialed skill source manifest directory");
    fs::write(
            skills.join(".flotilla-sources.json"),
            r#"{"schema_version":5,"sources":[{"name":"mattpocock-skills","repository":"https://github.com/flotilla-org/mattpocock-skills.git","revision":"1111111111111111111111111111111111111111","credential":"github-skills-fork"}]}"#,
        )
        .expect("write credentialed skill source manifest");
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
}

pub(super) struct CredentialInteriorRunner(pub(super) DiscoveryMockRunner, pub(super) Option<Arc<AtomicBool>>);

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
        if cmd == "sh" && args.iter().any(|arg| arg.contains("flotilla-stage-skills")) {
            if let Some(staged) = &self.1 {
                staged.store(true, Ordering::SeqCst);
            }
            Ok("ok".to_string())
        } else if cmd == "mkdir"
                || cmd == "chmod"
                || (cmd == "sh" && args.iter().any(|arg| arg.contains("flotilla-skills-preflight")))
                || (cmd == "sh" && args.contains(&"flotilla-prune-skill-tokens"))
                // This runner stands in for the contained process boundary;
                // real scratch copying and credential links are tested in agent_material.
                || (cmd == "sh" && args.contains(&"flotilla-crew-home"))
        {
            Ok(String::new())
        } else {
            self.0.run(cmd, args, cwd, label).await
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.0.run_output(cmd, args, cwd, label).await
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel, _input: &[u8]) -> Result<String, String> {
        if cmd == "sh" && args.iter().any(|arg| arg.contains("flotilla-stage-skills")) {
            if let Some(staged) = &self.1 {
                staged.store(true, Ordering::SeqCst);
            }
        }
        Ok("ok".to_string())
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.0.exists(cmd, args).await
    }

    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        self.0.path_exists(path).await
    }

    async fn writable_scratch_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
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

    async fn status(&self) -> Result<flotilla_protocol::EnvironmentStatus, String> {
        Ok(flotilla_protocol::EnvironmentStatus::Running)
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

    async fn status(&self) -> Result<flotilla_protocol::EnvironmentStatus, String> {
        Err(self.status_error.clone())
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
    fn kind(&self) -> flotilla_core::providers::environment::EnvironmentKind {
        flotilla_core::providers::environment::EnvironmentKind::Docker
    }
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        _opts: &flotilla_core::providers::environment::PrepareOpts,
    ) -> Result<flotilla_core::providers::environment::PreparedEnvironment, String> {
        Ok(flotilla_core::providers::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_core::providers::environment::PreparedEnvironment,
        _opts: flotilla_core::providers::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, String> {
        self.handle.lock().await.take().ok_or_else(|| "test environment already created".to_string())
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
    fn kind(&self) -> flotilla_core::providers::environment::EnvironmentKind {
        flotilla_core::providers::environment::EnvironmentKind::Docker
    }
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        _opts: &flotilla_core::providers::environment::PrepareOpts,
    ) -> Result<flotilla_core::providers::environment::PreparedEnvironment, String> {
        Ok(flotilla_core::providers::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_core::providers::environment::PreparedEnvironment,
        _opts: flotilla_core::providers::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, String> {
        Err("not used".to_string())
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
    fn kind(&self) -> flotilla_core::providers::environment::EnvironmentKind {
        flotilla_core::providers::environment::EnvironmentKind::Docker
    }
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        opts: &flotilla_core::providers::environment::PrepareOpts,
    ) -> Result<flotilla_core::providers::environment::PreparedEnvironment, String> {
        *self.prepared_auth.lock().await = Some(opts.prepared_auth.clone());
        Ok(flotilla_core::providers::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_core::providers::environment::PreparedEnvironment,
        opts: flotilla_core::providers::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, String> {
        *self.create_opts.lock().await = Some(CreateOpts {
            tokens: opts.tokens,
            working_directory: opts.working_directory,
            provisioned_mounts: opts.provisioned_mounts,
            tools: opts.tools,
            cpu_limit: opts.cpu_limit,
            memory_policy: opts.memory_policy,
            image_pull_policy: Default::default(),
            prepared_auth: self.prepared_auth.lock().await.take().expect("prepared auth captured"),
        });
        Err("stop after capturing create options".to_string())
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
    fn kind(&self) -> flotilla_core::providers::environment::EnvironmentKind {
        flotilla_core::providers::environment::EnvironmentKind::Docker
    }
    async fn prepare(
        &self,
        _spec: &flotilla_resources::EnvironmentSpec,
        _opts: &flotilla_core::providers::environment::PrepareOpts,
    ) -> Result<flotilla_core::providers::environment::PreparedEnvironment, String> {
        Ok(flotilla_core::providers::environment::PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(
        &self,
        _id: EnvironmentId,
        _image: &flotilla_core::providers::environment::PreparedEnvironment,
        _opts: flotilla_core::providers::environment::ProvisionOpts,
    ) -> Result<EnvironmentHandle, String> {
        Err("not used".to_string())
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
    use flotilla_core::providers::environment::host_direct::HostDirectEnvironmentProvider;
    // The injected runner stands in for host subprocesses; adopting and
    // detaching the host itself must never invoke it.
    registry.environment_providers.insert(
        "host-fixture",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "host-fixture"),
        Arc::new(HostDirectEnvironmentProvider::new(Arc::new(DiscoveryMockRunner::builder().build()), HashMap::new())),
    );
}

pub(super) fn passthrough_registry() -> Arc<ProviderRegistry> {
    use flotilla_core::providers::{
        discovery::{ProviderCategory, ProviderDescriptor},
        registry::ProviderRegistry,
        terminal::passthrough::PassthroughTerminalPool,
    };

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
    use flotilla_core::providers::{
        discovery::{ProviderCategory, ProviderDescriptor},
        registry::ProviderRegistry,
        terminal::passthrough::PassthroughTerminalPool,
    };

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
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_composition: None,
                    image_build_ref: None,
                    memory_policy: Default::default(),
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
    flotilla_resources::apply_status_patch(
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

// The only fake boundary is the Docker CLI: preparation must inspect the
// admitted tag and resolve its local digest, without a real Docker daemon.
pub(super) struct HeldBaselineRunner {
    pub(super) image: String,
}

#[async_trait]
impl CommandRunner for HeldBaselineRunner {
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        assert_eq!(cmd, "docker");
        match args {
            ["image", "inspect", image] if *image == self.image => Ok(String::new()),
            ["--config", directory, "pull", image] if *image == self.image => {
                assert!(directory.contains("flotilla-anonymous-"), "baseline pull must isolate ambient credentials");
                Ok(String::new())
            }
            ["image", "inspect", "--format", "{{.Id}}", image] if *image == self.image => Ok(format!("sha256:{}", "a".repeat(64))),
            _ => panic!("unexpected Docker invocation: {args:?}"),
        }
    }
    async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        panic!("unexpected output invocation")
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        false
    }
}
