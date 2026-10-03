#![allow(dead_code, unused_imports, clippy::empty_line_after_outer_attr)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    attachable::{shared_in_memory_attachable_store, AttachableSet, AttachableSetId, ProviderBinding, TerminalPurpose},
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    model::RepoModel,
    path_context::ExecutionEnvironmentPath,
    providers::{
        ai_utility::{AiUtility, ConvoyNames},
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        discovery::{
            test_support::{
                fake_discovery, fake_discovery_with_provider_set, fake_discovery_with_providers, fake_discovery_with_runner,
                fake_vcs_discovery, git_process_discovery, init_git_repo, init_git_repo_with_remote, DiscoveryMockRunner,
                FakeChangeRequest, FakeCheckoutManager, FakeDiscoveryProviders, FakeIssueProvider, FakePresentationManager,
                FakeTerminalPool, FakeVcsFactory, FakeVcsState, TestEnvVars,
            },
            DiscoveryRuntime, EnvironmentAssertion, EnvironmentBag, Factory, HostDetector, ProviderCategory, ProviderDescriptor,
            RepoDetector, UnmetRequirement,
        },
        environment::{EnvironmentHandle, ProvisionedEnvironment},
        terminal::TerminalPool,
        types::{ChangeRequest, CloudAgentSession, RepoCriteria, SessionStatus, Workspace},
        ChannelLabel, CommandRunner,
    },
    repository_inspection::{LocalCheckoutInspection, RepositoryContinuity, RepositoryInspection, RepositoryInspector},
};
use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    test_support::TestIssue,
    Checkout, CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandValue, ConvoyStartIntent, DaemonEvent, EnvironmentId,
    EnvironmentInfo, EnvironmentStatus, HostEnvironment, HostName, HostPath, HostProviderStatus, HostSummary, ImageId, IssueRef,
    IssueSelector, IssueSource, ManifestResolution, NodeId, NodeInfo, PeerConnectionState, ProviderData, RepoIdentity, RepoSelector,
    StepStatus, StreamKey, SystemInfo, ToolInventory, TopologyRoute,
};
use flotilla_resources::{
    apply_status_patch, controller_patches as convoy_controller_patches, implement_review_workflow_spec,
    single_agent_shepherd_workflow_spec, single_agent_workflow_spec, Checkout as ResourceCheckout, CheckoutPhase as ResourceCheckoutPhase,
    CheckoutSpec as ResourceCheckoutSpec, Convoy as ResourceConvoy, ConvoyPhase, CredentialConsumer, CredentialGrant,
    CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec,
    CredentialSpecSpec, DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, FulfilmentFacts, FulfilmentGrant, FulfilmentKind,
    FulfilmentKindSpec, HarnessFacts, Host as ResourceHost, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec,
    HostStatus, InputMeta, LifecycleAuthority, ModelFact, ModelFactSource, ObservedCheckoutSpec, PlacementPolicy, PlacementPolicySpec,
    Project, ProjectRepositorySpec, ProjectSpec, Regard, RegardExpiryPolicy, RegardSource, Repository, RepositoryKey, RepositoryRelation,
    RepositorySpec, ResourceBackend, ResourceError, SqliteBackend, Stance, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
    TerminalSessionStatusPatch, TypedResolver, WatchEvent, WatchStart, WorkPhase, WorkState, WorkflowSnapshot, WorkflowTemplate,
    AGENT_ADAPTERS_CAPABILITY, CONVOY_LABEL, HELD_CREDENTIALS_CAPABILITY, REPO_KEY_LABEL, REPO_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use futures::StreamExt;
use tokio::sync::Notify;

async fn admitted_convoy(backend: &ResourceBackend, role: &str) -> flotilla_resources::ResourceObject<ResourceConvoy> {
    let selector = BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_string(), role.to_string())]);
    backend
        .clone()
        .using::<ResourceConvoy>("flotilla")
        .list_matching_labels(&selector)
        .await
        .expect("list admitted convoy")
        .items
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("admitted convoy role {role}"))
}

struct FixedRemoteHostDetector {
    owner: &'static str,
    repo: &'static str,
}

struct MutableRemoteHostDetector {
    owner: &'static str,
    repo: Arc<std::sync::RwLock<String>>,
}

struct TestRepositoryInspector {
    host_ref: String,
    repository: Arc<std::sync::RwLock<String>>,
    fixed_repository_by_path: HashMap<PathBuf, String>,
    continuity: bool,
    inspection_failure: Option<String>,
}

#[async_trait]
impl RepositoryInspector for TestRepositoryInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        if let Some(error) = &self.inspection_failure {
            return Err(error.clone());
        }
        let repository = match self.fixed_repository_by_path.get(path) {
            Some(repository) => repository.clone(),
            None => self.repository.read().map_err(|_| "test repository identity lock poisoned".to_string())?.clone(),
        };
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote(format!("https://github.com/owner/{repository}"))?,
            checkout: LocalCheckoutInspection {
                path: path.to_path_buf(),
                host_ref: self.host_ref.clone(),
                git_ref: "main".to_string(),
                is_main: true,
            },
            transport_url: Some(format!("https://github.com/owner/{repository}")),
            replaces_prior_repository: false,
        })
    }

    async fn verify_continuity(&self, _path: &Path, _previous: &RepositorySpec) -> RepositoryContinuity {
        if self.continuity {
            RepositoryContinuity::Continuous { evidence: "test histories share a reachable ref".to_string() }
        } else {
            RepositoryContinuity::Unproven { evidence: "test histories are unrelated".to_string() }
        }
    }
}

async fn install_test_repository_inspector(daemon: &InProcessDaemon, repository: Arc<std::sync::RwLock<String>>) {
    daemon
        .set_repository_inspector(Arc::new(TestRepositoryInspector {
            host_ref: daemon.local_host_id().expect("local Host").to_string(),
            repository,
            fixed_repository_by_path: HashMap::new(),
            continuity: true,
            inspection_failure: None,
        }))
        .await;
}

fn qpath(host: &HostName, path: impl Into<PathBuf>) -> QualifiedPath {
    QualifiedPath::from_host_name(host, path.into())
}

#[async_trait]
impl RepoDetector for FixedRemoteHostDetector {
    async fn detect(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: &dyn flotilla_core::providers::CommandRunner,
        _env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        vec![EnvironmentAssertion::remote_host("github.com", self.owner, self.repo, "origin")]
    }
}

#[async_trait]
impl RepoDetector for MutableRemoteHostDetector {
    async fn detect(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: &dyn flotilla_core::providers::CommandRunner,
        _env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        let repo = self.repo.read().expect("mutable remote detector should not be poisoned").clone();
        vec![EnvironmentAssertion::remote_host("github.com", self.owner, repo, "origin")]
    }
}

struct RunnerEchoHostDetector {
    probe: &'static str,
    assertion_key: &'static str,
}

#[async_trait]
impl HostDetector for RunnerEchoHostDetector {
    async fn detect(
        &self,
        runner: &dyn CommandRunner,
        _env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        match runner.run("probe-env", &[self.probe], Path::new("/"), &ChannelLabel::Default).await {
            Ok(value) => vec![EnvironmentAssertion::env_var(self.assertion_key, value.trim())],
            Err(_) => Vec::new(),
        }
    }
}

struct EnvVarEchoHostDetector {
    env_var: &'static str,
    assertion_key: &'static str,
}

struct FixedEnvVarHostDetector {
    key: &'static str,
    value: &'static str,
}

#[async_trait]
impl HostDetector for FixedEnvVarHostDetector {
    async fn detect(
        &self,
        _runner: &dyn CommandRunner,
        _env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        vec![EnvironmentAssertion::env_var(self.key, self.value)]
    }
}

#[async_trait]
impl HostDetector for EnvVarEchoHostDetector {
    async fn detect(
        &self,
        _runner: &dyn CommandRunner,
        env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        env.get(self.env_var).map(|value| vec![EnvironmentAssertion::env_var(self.assertion_key, value)]).unwrap_or_default()
    }
}

struct HangingSshRunner {
    delay: Duration,
}

#[async_trait]
impl CommandRunner for HangingSshRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        if cmd == "probe-env" {
            return Ok("local".into());
        }
        if cmd == "ssh" && args.iter().any(|arg| arg.contains("buildbox.example")) {
            return Ok(String::new());
        }
        if cmd == "ssh" && args.iter().any(|arg| arg.contains("hangbox.example")) {
            tokio::time::sleep(self.delay).await;
            return Ok(String::new());
        }
        Err(format!("unexpected command: {cmd} {}", args.join(" ")))
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<flotilla_core::providers::CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(flotilla_core::providers::CommandOutput { stdout, stderr: String::new(), success: true }),
            Err(stderr) => Ok(flotilla_core::providers::CommandOutput { stdout: String::new(), stderr, success: false }),
        }
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

struct SlowCloudAgent {
    archive_started: Notify,
    archive_release: Notify,
}

impl SlowCloudAgent {
    fn new() -> Self {
        Self { archive_started: Notify::new(), archive_release: Notify::new() }
    }

    async fn wait_for_archive_start(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.archive_started.notified()).await.expect("archive should start");
    }

    fn release_archive(&self) {
        self.archive_release.notify_waiters();
    }
}

#[async_trait]
impl CloudAgentService for SlowCloudAgent {
    async fn list_sessions(&self, _: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        Ok(vec![("sess-1".into(), CloudAgentSession {
            title: "Slow Session".into(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        })])
    }

    async fn archive_session(&self, _: &str) -> Result<(), String> {
        self.archive_started.notify_waiters();
        self.archive_release.notified().await;
        Ok(())
    }

    async fn attach_command(&self, _: &str) -> Result<String, String> {
        Ok("attach slow-session".into())
    }
}

struct SlowCloudAgentFactory {
    agent: Arc<SlowCloudAgent>,
}

#[async_trait]
impl Factory for SlowCloudAgentFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn CloudAgentService;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::CloudAgent, "slow-agent", "Slow Agent", "AG", "Sessions", "session")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.agent) as Arc<dyn CloudAgentService>)
    }
}

fn slow_cloud_agent_discovery(agent: Arc<SlowCloudAgent>) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    runtime.factories.cloud_agents.push(Box::new(SlowCloudAgentFactory { agent }));
    runtime
}

struct SlowAiUtility {
    generation_started: Notify,
    generation_release: Notify,
}

impl SlowAiUtility {
    fn new() -> Self {
        Self { generation_started: Notify::new(), generation_release: Notify::new() }
    }

    async fn wait_for_generation_start(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.generation_started.notified()).await.expect("generation should start");
    }

    fn release_generation(&self) {
        self.generation_release.notify_waiters();
    }
}

#[async_trait]
impl AiUtility for SlowAiUtility {
    async fn generate_branch_name(&self, _: &str) -> Result<String, String> {
        self.generation_started.notify_waiters();
        self.generation_release.notified().await;
        Ok("feat/slow-branch".into())
    }
}

struct SlowAiUtilityFactory {
    utility: Arc<SlowAiUtility>,
    probes: Option<Arc<AtomicUsize>>,
}

#[async_trait]
impl Factory for SlowAiUtilityFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::AiUtility, "slow-ai")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        if let Some(probes) = &self.probes {
            probes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Arc::clone(&self.utility) as Arc<dyn AiUtility>)
    }
}

fn slow_ai_discovery(utility: Arc<SlowAiUtility>) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    runtime.factories.ai_utilities.push(Box::new(SlowAiUtilityFactory { utility, probes: None }));
    runtime
}

struct PanicOnceAiUtility {
    panicked: AtomicBool,
}

#[async_trait]
impl AiUtility for PanicOnceAiUtility {
    async fn generate_branch_name(&self, _: &str) -> Result<String, String> {
        Ok("fix/retried-convoy".into())
    }

    async fn generate_convoy_names(&self, _: &str) -> Result<ConvoyNames, String> {
        if !self.panicked.swap(true, Ordering::SeqCst) {
            panic!("injected convoy start worker panic");
        }
        Ok(ConvoyNames { name: "retried-convoy".into(), branch: "fix/retried-convoy".into() })
    }
}

struct PanicOnceAiUtilityFactory {
    utility: Arc<PanicOnceAiUtility>,
}

#[async_trait]
impl Factory for PanicOnceAiUtilityFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::AiUtility, "panic-once-ai")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.utility) as Arc<dyn AiUtility>)
    }
}

struct CountingConvoyAiUtility {
    calls: AtomicUsize,
    fail: AtomicBool,
}

#[async_trait]
impl AiUtility for CountingConvoyAiUtility {
    async fn generate_branch_name(&self, _: &str) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("unexpected-branch-only-call".into())
    }

    async fn generate_convoy_names(&self, _: &str) -> Result<ConvoyNames, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err("offline".into())
        } else {
            Ok(ConvoyNames { name: "generated-convoy".into(), branch: "fix/generated-convoy".into() })
        }
    }
}

struct CountingConvoyAiUtilityFactory {
    utility: Arc<CountingConvoyAiUtility>,
}

#[async_trait]
impl Factory for CountingConvoyAiUtilityFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::AiUtility, "counting-ai")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.utility) as Arc<dyn AiUtility>)
    }
}

struct TestProvisionedEnvironment {
    id: EnvironmentId,
    image: ImageId,
    runner: Arc<dyn CommandRunner>,
    env_vars: HashMap<String, String>,
}

#[async_trait]
impl ProvisionedEnvironment for TestProvisionedEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
    }

    fn container_name(&self) -> Option<&str> {
        None
    }

    fn provisioned_mounts(&self) -> Vec<flotilla_core::providers::environment::ProvisionedMount> {
        vec![]
    }

    async fn status(&self) -> Result<EnvironmentStatus, String> {
        Ok(EnvironmentStatus::Running)
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(self.env_vars.clone())
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }

    async fn destroy(&self) -> Result<(), String> {
        Ok(())
    }
}

struct EnvGatedTerminalPoolFactory {
    required_env_var: &'static str,
    pool: Arc<dyn TerminalPool>,
}

#[async_trait]
impl Factory for EnvGatedTerminalPoolFactory {
    type Output = dyn TerminalPool;
    type Descriptor = ProviderDescriptor;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(
            ProviderCategory::TerminalPool,
            "managed-bag-terminal-pool",
            "Managed Bag Terminals",
            "TP",
            "Terminals",
            "terminal",
        )
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        if env.find_env_var(self.required_env_var).is_some() {
            Ok(Arc::clone(&self.pool))
        } else {
            Err(vec![UnmetRequirement::MissingEnvVar(self.required_env_var.into())])
        }
    }
}

fn sample_remote_host_summary(name: &str) -> HostSummary {
    HostSummary {
        environment_id: EnvironmentId::host(HostId::new(format!("{name}-host"))),
        host_name: Some(HostName::new(name)),
        node: test_node(name),
        system: SystemInfo {
            home_dir: Some(PathBuf::from(format!("/home/{name}"))),
            os: Some("linux".into()),
            arch: Some("aarch64".into()),
            cpu_count: Some(4),
            memory_total_mb: Some(8192),
            environment: HostEnvironment::Container,
        },
        inventory: ToolInventory::default(),
        providers: vec![HostProviderStatus {
            category: "vcs".into(),
            name: "Git".into(),
            implementation: "git".into(),
            healthy: true,
            disabled_reason: None,
        }],
        environments: vec![],
    }
}

fn test_node(name: &str) -> NodeInfo {
    NodeInfo::new(NodeId::new(format!("node-{name}")), name)
}

fn snapshot_host(snapshot: &flotilla_protocol::HostSnapshot) -> HostName {
    HostName::new(snapshot.node.display_name.clone())
}

fn summary_host(summary: &HostSummary) -> HostName {
    HostName::new(summary.node.display_name.clone())
}

trait DaemonTestCompat {
    async fn set_configured_peer_names(&self, peers: Vec<HostName>);
    async fn set_peer_host_summaries(&self, summaries: HashMap<HostName, HostSummary>);
    async fn publish_peer_connection_status(&self, host: &HostName, status: PeerConnectionState);
    async fn publish_peer_summary(&self, host: &HostName, summary: HostSummary);
    async fn add_virtual_repo(
        &self,
        identity: RepoIdentity,
        repository_key: Option<flotilla_protocol::RepositoryKey>,
        synthetic_path: PathBuf,
        peers: Vec<(HostName, ProviderData)>,
        overlay_version: u64,
    ) -> Result<(), String>;
}

impl DaemonTestCompat for Arc<InProcessDaemon> {
    async fn set_configured_peer_names(&self, peers: Vec<HostName>) {
        InProcessDaemon::set_configured_peers(self.as_ref(), peers.into_iter().map(|host| test_node(host.as_str())).collect()).await;
    }

    async fn set_peer_host_summaries(&self, summaries: HashMap<HostName, HostSummary>) {
        InProcessDaemon::set_peer_host_summaries(
            self.as_ref(),
            summaries
                .into_iter()
                .map(|(host, mut summary)| {
                    summary.node = test_node(host.as_str());
                    (summary.environment_id.clone(), summary)
                })
                .collect(),
        )
        .await;
    }

    async fn publish_peer_connection_status(&self, host: &HostName, status: PeerConnectionState) {
        InProcessDaemon::publish_peer_connection_status(self.as_ref(), &test_node(host.as_str()), status).await;
    }

    async fn publish_peer_summary(&self, host: &HostName, mut summary: HostSummary) {
        summary.node = test_node(host.as_str());
        InProcessDaemon::publish_peer_summary(self.as_ref(), summary).await;
    }

    async fn add_virtual_repo(
        &self,
        identity: RepoIdentity,
        repository_key: Option<flotilla_protocol::RepositoryKey>,
        synthetic_path: PathBuf,
        peers: Vec<(HostName, ProviderData)>,
        overlay_version: u64,
    ) -> Result<(), String> {
        let _ = (peers, overlay_version);
        InProcessDaemon::add_virtual_repo(self.as_ref(), identity, repository_key, synthetic_path).await
    }
}

fn definitely_remote_host() -> HostName {
    if HostName::local().to_string() == "test-remote-host" {
        HostName::new("test-remote-host-alt")
    } else {
        HostName::new("test-remote-host")
    }
}

fn test_repo_identity() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

fn local_bare_remote_discovery() -> DiscoveryRuntime {
    let mut runtime = git_process_discovery(false);
    runtime.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));
    runtime
}

struct FailingChangeRequestTracker;

#[async_trait]
impl ChangeRequestTracker for FailingChangeRequestTracker {
    async fn list_change_requests(&self, _: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Err("change request listing failed".into())
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        Err(format!("change request {id} not found"))
    }

    async fn open_in_browser(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn merge_change_request(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn list_merged_branch_names(&self, _: usize) -> Result<Vec<String>, String> {
        Err("merged branch listing failed".into())
    }
}

struct CountingChangeRequestTracker {
    polls: AtomicUsize,
}

#[async_trait]
impl ChangeRequestTracker for CountingChangeRequestTracker {
    async fn list_change_requests(&self, _: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        Err(format!("change request {id} not found"))
    }

    async fn open_in_browser(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn merge_change_request(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn list_merged_branch_names(&self, _: usize) -> Result<Vec<String>, String> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

async fn daemon_for_cwd() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;
    (temp, repo, daemon)
}

async fn daemon_for_plain_dir() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;
    (temp, repo, daemon)
}

async fn daemon_for_plain_dir_with_discovery(discovery: DiscoveryRuntime) -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".to_string()))).await;
    (temp, repo, daemon)
}

fn test_config_store(config_dir: PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(config_dir.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(config_dir))
}

#[tokio::test]
async fn host_capability_provider_is_constructed_once_for_multiple_tracked_repositories() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).expect("create first repo");
    std::fs::create_dir_all(&repo_b).expect("create second repo");

    let probes = Arc::new(AtomicUsize::new(0));
    let mut discovery = fake_discovery(false);
    discovery
        .factories
        .ai_utilities
        .push(Box::new(SlowAiUtilityFactory { utility: Arc::new(SlowAiUtility::new()), probes: Some(Arc::clone(&probes)) }));

    let daemon =
        InProcessDaemon::new(vec![repo_a, repo_b], test_config_store(temp.path().join("config")), discovery, HostName::local()).await;

    assert_eq!(daemon.tracked_repo_paths().await.len(), 2);
    assert_eq!(probes.load(Ordering::SeqCst), 1, "host provider construction must not scale with repository count");
}

struct FreshIssueFactory;

#[async_trait]
impl Factory for FreshIssueFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn flotilla_core::providers::issue_tracker::IssueProvider;
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::IssueProvider, "fresh-issues")
    }
    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::new(FakeIssueProvider::new()))
    }
}

#[tokio::test]
async fn demanded_host_issue_provider_keeps_its_lifetime_without_a_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut discovery = fake_discovery(false);
    discovery.factories.issue_trackers = vec![Box::new(FreshIssueFactory)];
    let daemon = InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), discovery, HostName::local()).await;
    let source = IssueSource { service: "fake".into(), scope: "owner/repo".into() };
    let first = daemon.issue_provider_for_source(&source).await.expect("first provider");
    let second = daemon.issue_provider_for_source(&source).await.expect("retained provider");
    assert!(Arc::ptr_eq(&first, &second), "a live provider lease must preserve state such as ETag caches");
    drop(first);
    drop(second);
    let replacement = daemon.issue_provider_for_source(&source).await.expect("new provider lifetime");
    assert!(replacement.supports(&source));
}

#[tokio::test]
async fn fetch_issue_by_ref_does_not_require_a_tracked_checkout() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let reference =
        IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }, id: "747".into() };
    let mut issue = TestIssue::new("Source-addressed issue fetch seam").build();
    issue.reference = reference.clone();
    let provider = Arc::new(FakeIssueProvider::new());
    provider.add_issues(vec![(reference.id.clone(), issue)]).await;
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new()
            .with_issue_tracker(provider.clone() as Arc<dyn flotilla_core::providers::issue_tracker::IssueProvider>),
    );
    let daemon = InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), discovery, HostName::local()).await;

    let fetched = daemon.fetch_issue_by_ref(&reference).await.expect("source-addressed fetch should succeed");

    assert_eq!(fetched.reference, reference);
    assert_eq!(fetched.title, "Source-addressed issue fetch seam");
    assert!(daemon.list_repos().await.expect("list repos").is_empty());
}

#[tokio::test]
async fn resource_list_and_get_queries_return_wire_json() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    daemon
        .resource_backend()
        .using::<ResourceConvoy>("flotilla")
        .create(
            &InputMeta::builder().name("resource-demo".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create convoy");

    let listed = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceList {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    include_replicas: false,
                })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("list query");
    let CommandValue::ResourceRead(listed) = listed else { panic!("expected resource read") };
    assert_eq!(listed.resource_kind, "Convoy");
    assert_eq!(listed.records[0].record_type, flotilla_protocol::ResourceRecordType::Current);
    let listed_object = listed.records[0].object.as_ref().expect("listed object");
    assert_eq!(listed_object["apiVersion"], "flotilla.work/v1");
    assert_eq!(listed_object["kind"], "Convoy");
    assert_eq!(listed_object["metadata"]["name"], "resource-demo");

    let fetched = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceGet {
                    namespace: "flotilla".to_string(),
                    kind: "Convoy".to_string(),
                    name: "resource-demo".to_string(),
                })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("get query");
    let CommandValue::ResourceRead(fetched) = fetched else { panic!("expected resource read") };
    let fetched_object = fetched.records[0].object.as_ref().expect("fetched object");
    assert_eq!(fetched_object["metadata"]["name"], "resource-demo");
    assert_eq!(fetched_object["spec"]["workflow_ref"], "wf");
    assert_eq!(fetched.cursor.position().expect("decode cursor").0, listed.cursor.position().expect("decode cursor").0);

    daemon
        .resource_backend()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("missing-repository".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Missing repository".into(),
            default_workflow_ref: "wf".into(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            repositories: vec![ProjectRepositorySpec {
                repo: RepositoryKey("missing".into()),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: None,
            }],
            dispatch_policy: None,
        })
        .await
        .expect("create project with unresolved repository");
    let project = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceGet {
                    namespace: "flotilla".to_string(),
                    kind: "projects".to_string(),
                    name: "missing-repository".to_string(),
                })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("get project with unresolved source");
    let CommandValue::ResourceRead(project) = project else { panic!("expected project read") };
    let project_object = project.records[0].object.as_ref().expect("project object");
    let error = project_object["issueSourceResolutionError"].as_str().expect("plain resolution error");
    assert!(error.starts_with("repository missing:"), "{error}");
    assert!(!error.contains("RepositoryUnavailable"), "{error}");
}

#[tokio::test]
async fn convoy_explain_discharges_terminal_checkout_only_after_vessel_teardown() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let vessels = backend.clone().using::<flotilla_resources::Vessel>("flotilla");
    let change_requests = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let repo_ref = flotilla_resources::RepositoryKey("repo-a".to_string());
    let workflow = implement_review_workflow_spec();
    let mut status = flotilla_resources::ConvoyStatus {
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(WorkflowSnapshot {
            stall_nudges: Default::default(),
            supervision: None,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
        }),
        observed_workflow_ref: Some("implement-review@1".to_string()),
        ..Default::default()
    };
    status.work.insert("work".to_string(), WorkState {
        phase: WorkPhase::Complete,
        placement: Some(flotilla_resources::PlacementStatus {
            fields: BTreeMap::from([(
                "checkout_refs".to_string(),
                serde_json::json!(BTreeMap::from([(repo_ref.clone(), "checkout-dead".to_string())])),
            )]),
        }),
        ..WorkState::builder().phase(WorkPhase::Complete).build()
    });
    let convoy = convoys
        .create(
            &InputMeta::builder().name("terminal-checkout".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder()
                .workflow_ref("implement-review".to_string())
                .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
                    .url("https://github.com/owner/repo".to_string())
                    .repo_ref(repo_ref.clone())
                    .source_ref("feature".to_string())
                    .target_ref("main".to_string())
                    .workspace_slug("repo".to_string())
                    .subpaths(Vec::new())
                    .build()])
                .change_request(
                    flotilla_resources::BoundChangeRequest::builder()
                        .id("42".to_string())
                        .repository_ref(repo_ref.clone())
                        .title("terminal".to_string())
                        .build(),
                )
                .build(),
        )
        .await
        .expect("create convoy");
    convoys.update_status("terminal-checkout", &convoy.metadata.resource_version, &status).await.expect("publish landing status");
    let vessel = vessels
        .create(
            &InputMeta::builder()
                .name("terminal-checkout-work".to_string())
                .labels(BTreeMap::from([(flotilla_resources::CONVOY_LABEL.to_string(), "terminal-checkout".to_string())]))
                .build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: "terminal-checkout".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create live vessel");
    let observed_at = chrono::Utc::now();
    let record_name = flotilla_resources::change_request_record_name("github.com", "owner/repo", 42);
    let record = change_requests
        .create(
            &InputMeta::builder().name(record_name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("owner/repo".to_string())
                .number(42)
                .observing_authority("host-test".to_string())
                .build(),
        )
        .await
        .expect("create change request");
    change_requests
        .update_status(&record_name, &record.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
            head_sha: flotilla_resources::Observation::known("abc123".to_string(), observed_at),
            checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
            },
            mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
        })
        .await
        .expect("publish merged change request");

    let explain = || async {
        daemon
            .execute_query(
                Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryExplainConvoy {
                        namespace: Some("flotilla".to_string()),
                        name: "terminal-checkout".to_string(),
                    },
                },
                uuid::Uuid::new_v4(),
            )
            .await
            .expect("explain convoy")
    };
    let CommandValue::ConvoyExplanation(explanation) = explain().await else { panic!("expected convoy explanation") };
    assert!(!explanation.settlement.satisfied, "a live vessel must retain its missing-checkout expectation");
    assert!(explanation.settlement.unmet.iter().any(|unmet| unmet.reason == "missing_record" && unmet.subject == "checkout/checkout-dead"));

    vessels.delete(&vessel.metadata.name).await.expect("tear down vessel");
    let CommandValue::ConvoyExplanation(explanation) = explain().await else { panic!("expected convoy explanation") };
    assert!(explanation.settlement.satisfied, "terminal world evidence should discharge the torn-down vessel's checkout");
    assert!(explanation.settlement.unmet.is_empty());
}

#[tokio::test]
async fn convoy_explain_reports_observed_digest_anchor_and_mismatch() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let checkouts = backend.clone().using::<ResourceCheckout>("flotilla");
    let repo_ref = flotilla_resources::RepositoryKey("repo-a".to_string());
    let convoy = convoys
        .create(
            &InputMeta::builder().name("digest-anchor".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder()
                .workflow_ref("reviewless".to_string())
                .adopted_checkout_refs(BTreeMap::from([(repo_ref.clone(), "digest-checkout".to_string())]))
                .build(),
        )
        .await
        .expect("create convoy");
    let claim = flotilla_resources::SettlementClaimEvidence::builder()
        .refs(flotilla_resources::ReviewRefPair::builder().base("refs/heads/main".to_string()).head("refs/heads/topic".to_string()).build())
        .bundle_url("s3://reviews/digest-anchor/1/".to_string())
        .claimed_head_digest("claimed-digest".to_string())
        .build();
    let mut status = flotilla_resources::ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() };
    status.crew_work.insert(
        "work".to_string(),
        BTreeMap::from([(
            "coder".to_string(),
            flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Done).claim_evidence(claim).build(),
        )]),
    );
    convoys.update_status("digest-anchor", &convoy.metadata.resource_version, &status).await.expect("publish claim");

    let checkout = checkouts
        .create(
            &InputMeta::builder()
                .name("digest-checkout".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "digest-anchor".to_string())]))
                .build(),
            &ResourceCheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "topic".to_string(),
                path: "/tmp/digest-checkout".to_string(),
                repo_ref,
                host_ref: "host-test".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("create checkout");
    let observed_at = chrono::Utc::now().to_rfc3339();
    let checkout_status = |digest: &str| flotilla_resources::CheckoutStatus {
        phase: ResourceCheckoutPhase::Ready,
        integration: flotilla_resources::CheckoutIntegrationStatus {
            remote_refs: BTreeMap::from([(
                "refs/heads/topic".to_string(),
                flotilla_resources::RemoteRefObservation::builder().digest(digest.to_string()).observed_at(observed_at.clone()).build(),
            )]),
            ..Default::default()
        },
        ..Default::default()
    };
    let checkout = checkouts
        .update_status("digest-checkout", &checkout.metadata.resource_version, &checkout_status("other-digest"))
        .await
        .expect("publish mismatched observation");

    let explain = || async {
        daemon
            .execute_query(
                Command::builder()
                    .action(CommandAction::QueryExplainConvoy {
                        namespace: Some("flotilla".to_string()),
                        name: "digest-anchor".to_string(),
                    })
                    .build(),
                uuid::Uuid::new_v4(),
            )
            .await
            .expect("explain convoy")
    };
    let CommandValue::ConvoyExplanation(explanation) = explain().await else { panic!("expected convoy explanation") };
    assert!(!explanation.settlement.satisfied);
    assert!(explanation.settlement.unmet.iter().any(|unmet| unmet.reason == "digest_mismatch"));

    checkouts
        .update_status("digest-checkout", &checkout.metadata.resource_version, &checkout_status("claimed-digest"))
        .await
        .expect("publish matching observation");
    let CommandValue::ConvoyExplanation(explanation) = explain().await else { panic!("expected convoy explanation") };
    assert!(explanation.settlement.satisfied);
    assert_eq!(explanation.settlement.mode, "observed_digest");
}

#[tokio::test]
async fn resource_list_and_get_queries_return_local_non_replicated_resources() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    daemon
        .resource_backend()
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder().name("local-workflow".to_string()).build(),
            &flotilla_resources::WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
        )
        .await
        .expect("create workflow template");

    let listed = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceList {
                    namespace: "flotilla".to_string(),
                    kind: "workflowtemplates".to_string(),
                    include_replicas: true,
                })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("list query");
    let CommandValue::ResourceRead(listed) = listed else { panic!("expected resource read") };
    assert!(listed
        .records
        .iter()
        .any(|record| { record.object.as_ref().is_some_and(|object| object["metadata"]["name"] == "local-workflow") }));

    let fetched = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceGet {
                    namespace: "flotilla".to_string(),
                    kind: "WorkflowTemplate".to_string(),
                    name: "local-workflow".to_string(),
                })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("get query");
    let CommandValue::ResourceRead(fetched) = fetched else { panic!("expected resource read") };
    assert_eq!(fetched.records[0].object.as_ref().expect("fetched object")["metadata"]["name"], "local-workflow");
}

#[tokio::test]
async fn operator_can_remove_a_wrong_repository_remote_without_deleting_the_record() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let stable = "https://github.com/flotilla-org/flotilla";
    let wrong = "https://github.com/wrong/flotilla";
    let spec = RepositorySpec::remote(stable).expect("repository").with_declared_remotes([wrong, stable]).expect("remote declaration");
    let name = spec.key().to_string();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(name.clone()).build(), &spec)
        .await
        .expect("seed repository");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::RepositoryRemoteRemove {
                    namespace: "flotilla".to_string(),
                    name: name.clone(),
                    remote: wrong.to_string(),
                })
                .build(),
        )
        .await
        .expect("remove remote command");
    assert_eq!(recv_command_finished(&mut events, command_id).await, CommandValue::Ok);

    let repository = daemon.resource_backend().using::<Repository>("flotilla").get(&name).await.expect("repository remains");
    assert_eq!(repository.spec.remotes(), [stable]);
}

#[tokio::test]
async fn deleting_a_replica_from_another_host_refuses_with_its_origin() {
    let authority_temp = tempfile::tempdir().expect("authority tempdir");
    let replica_temp = tempfile::tempdir().expect("replica tempdir");
    let authority =
        InProcessDaemon::new(vec![], test_config_store(authority_temp.path().join("config")), fake_discovery(false), HostName::new("feta"))
            .await;
    let replica =
        InProcessDaemon::new(vec![], test_config_store(replica_temp.path().join("config")), fake_discovery(false), HostName::new("kiwi"))
            .await;
    let origin = authority.node_id().clone();
    let convoys = authority.resource_backend().using::<ResourceConvoy>("flotilla");
    convoys
        .create(
            &InputMeta::builder().name("remote-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create convoy at authority");
    let snapshot = convoys.list().await.expect("authority snapshot");
    replica
        .resource_backend()
        .replica_writer::<ResourceConvoy>(origin.clone(), "flotilla")
        .replace(&snapshot, chrono::Utc::now())
        .await
        .expect("replicate convoy");

    let mut events = replica.subscribe();
    let command_id = replica
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: "remote-convoy".to_string(),
                    replica_origin: None,
                })
                .build(),
        )
        .await
        .expect("submit delete");
    let result = recv_command_finished(&mut events, command_id).await;
    assert!(
        matches!(result, CommandValue::Error { ref message } if message.contains(origin.as_str())),
        "a replica delete must identify its origin: {result:?}"
    );
    assert!(convoys.get("remote-convoy").await.is_ok(), "authority record must remain");
    assert!(
        replica.resource_backend().including_replicas::<ResourceConvoy>("flotilla").get("remote-convoy").await.is_ok(),
        "replica must remain visible"
    );

    for action in [
        CommandAction::ResourceStatusPatch {
            namespace: "flotilla".to_string(),
            kind: "convoys".to_string(),
            name: "remote-convoy".to_string(),
            status: serde_json::json!({}),
        },
        CommandAction::ResourceApply {
            namespace: "flotilla".to_string(),
            document: serde_json::json!({
                "apiVersion": "flotilla.work/v1", "kind": "Convoy", "metadata": {"name": "remote-convoy"}, "spec": {}
            }),
        },
    ] {
        let command_id = replica.execute(Command::builder().action(action).build()).await.expect("submit mutation");
        let result = recv_command_finished(&mut events, command_id).await;
        assert!(
            matches!(result, CommandValue::Error { ref message } if message.contains(origin.as_str())),
            "a replica mutation must identify its origin: {result:?}"
        );
    }

    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let repository_name = repository_spec.key().to_string();
    let repositories = authority.resource_backend().using::<Repository>("flotilla");
    repositories
        .create(&InputMeta::builder().name(repository_name.clone()).build(), &repository_spec)
        .await
        .expect("create repository at authority");
    replica
        .resource_backend()
        .replica_writer::<Repository>(origin.clone(), "flotilla")
        .replace(&repositories.list().await.expect("repository snapshot"), chrono::Utc::now())
        .await
        .expect("replicate repository");
    let command_id = replica
        .execute(
            Command::builder()
                .action(CommandAction::RepositoryRemoteRemove {
                    namespace: "flotilla".to_string(),
                    name: repository_name,
                    remote: "https://github.com/flotilla-org/flotilla".to_string(),
                })
                .build(),
        )
        .await
        .expect("submit repository mutation");
    let result = recv_command_finished(&mut events, command_id).await;
    assert!(
        matches!(result, CommandValue::Error { ref message } if message.contains(origin.as_str())),
        "a repository replica mutation must identify its origin: {result:?}"
    );
}

#[tokio::test]
async fn orphaned_authority_record_can_be_collected_from_the_replica_store() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let authority_node = test_node("retired-authority");
    let origin = authority_node.node_id.clone();
    daemon.set_configured_peers(vec![authority_node.clone()]).await;

    let local = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    local
        .create(
            &InputMeta::builder().name("orphaned-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create authority record");
    let authority_snapshot = local.list().await.expect("list authority record");
    local.delete("orphaned-convoy").await.expect("authority store vanishes");
    daemon
        .resource_backend()
        .replica_writer::<ResourceConvoy>(origin.clone(), "flotilla")
        .replace(&authority_snapshot, chrono::Utc::now())
        .await
        .expect("retain stale replica after authority disappears");

    let mut events = daemon.subscribe();
    InProcessDaemon::publish_peer_connection_status(daemon.as_ref(), &authority_node, PeerConnectionState::Connected).await;
    let refused_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: "orphaned-convoy".to_string(),
                    replica_origin: Some(origin.clone()),
                })
                .build(),
        )
        .await
        .expect("request collection while authority is connected");
    let refused = recv_command_finished(&mut events, refused_id).await;
    assert!(
        matches!(refused, CommandValue::Error { ref message } if message.contains("is connected")),
        "live authorities must retain their deletion authority: {refused:?}"
    );

    InProcessDaemon::publish_peer_connection_status(daemon.as_ref(), &authority_node, PeerConnectionState::Disconnected).await;
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: "orphaned-convoy".to_string(),
                    replica_origin: Some(origin),
                })
                .build(),
        )
        .await
        .expect("collect orphaned replica");

    let result = recv_command_finished(&mut events, command_id).await;
    assert!(matches!(result, CommandValue::ResourceDeleted(_)), "unexpected collection result: {result:?}");
    assert!(
        daemon
            .resource_backend()
            .including_replicas::<ResourceConvoy>("flotilla")
            .list()
            .await
            .expect("list after collection")
            .items
            .is_empty(),
        "the stale authority projection must be collectable"
    );
}

#[tokio::test]
async fn deleting_a_missing_authoritative_resource_converges_across_peer_relay_cycles() {
    let authority_temp = tempfile::tempdir().expect("create authority tempdir");
    let authority = InProcessDaemon::new(
        vec![],
        test_config_store(authority_temp.path().join("config")),
        fake_discovery(false),
        HostName::new("authority"),
    )
    .await;
    let peer_one_temp = tempfile::tempdir().expect("create first peer tempdir");
    let peer_one = InProcessDaemon::new(
        vec![],
        test_config_store(peer_one_temp.path().join("config")),
        fake_discovery(false),
        HostName::new("peer-one"),
    )
    .await;
    let peer_two_temp = tempfile::tempdir().expect("create second peer tempdir");
    let peer_two = InProcessDaemon::new(
        vec![],
        test_config_store(peer_two_temp.path().join("config")),
        fake_discovery(false),
        HostName::new("peer-two"),
    )
    .await;
    let origin = authority.node_id().clone();
    let fixture_backend = ResourceBackend::InMemory(Default::default());
    let fixture = fixture_backend.using::<ResourceConvoy>("flotilla");
    fixture
        .create(
            &InputMeta::builder().name("lost-at-authority".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create stale source object");
    let stale_snapshot = fixture.list().await.expect("snapshot stale source object");
    let stale_object = stale_snapshot.items[0].clone();
    let stale_synced_at = chrono::Utc::now() - chrono::Duration::minutes(1);
    for daemon in [&authority, &peer_one, &peer_two] {
        daemon
            .resource_backend()
            .replica_writer::<ResourceConvoy>(origin.clone(), "flotilla")
            .replace(&stale_snapshot, stale_synced_at)
            .await
            .expect("seed stale replica");
    }

    let authority_convoys = authority.resource_backend().using::<ResourceConvoy>("flotilla");
    let listed = authority_convoys.list().await.expect("list before authority tombstone");
    let mut authority_watch = authority_convoys.watch(WatchStart::resuming_from(&listed)).await.expect("watch authority tombstones");
    let mut peer_one_relay =
        peer_one.resource_backend().including_replicas::<ResourceConvoy>("flotilla").watch().await.expect("watch first peer replicas");
    let mut authority_events = authority.subscribe();
    let delete_id = authority
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: "lost-at-authority".to_string(),
                    replica_origin: None,
                })
                .build(),
        )
        .await
        .expect("delete missing authority resource");
    let deleted = recv_command_finished(&mut authority_events, delete_id).await;
    assert!(matches!(deleted, CommandValue::ResourceDeleted(_)), "missing authority delete must succeed: {deleted:?}");

    let tombstone_event = tokio::time::timeout(Duration::from_secs(1), authority_watch.next())
        .await
        .expect("authority tombstone watch timeout")
        .expect("authority tombstone watch ended")
        .expect("authority tombstone watch failed");
    assert!(
        matches!(&tombstone_event, WatchEvent::DeletedByName(tombstone) if tombstone.name == "lost-at-authority"),
        "missing authority delete must emit a name tombstone: {tombstone_event:?}"
    );
    let tombstone_synced_at = chrono::Utc::now();
    for peer in [&peer_one, &peer_two] {
        peer.resource_backend()
            .replica_writer::<ResourceConvoy>(origin.clone(), "flotilla")
            .apply(tombstone_event.clone(), tombstone_synced_at)
            .await
            .expect("relay authority tombstone to peer");
    }

    let relayed = tokio::time::timeout(Duration::from_secs(1), peer_one_relay.next())
        .await
        .expect("peer relay tombstone timeout")
        .expect("peer relay watch ended")
        .expect("peer relay watch failed");
    let flotilla_resources::ReadWatchEvent::DeletedByName { tombstone, provenance } = relayed else {
        panic!("expected relayed name tombstone");
    };
    let flotilla_resources::ResourceProvenance::Replica { last_synced_at, .. } = provenance else {
        panic!("expected replica provenance");
    };
    peer_two
        .resource_backend()
        .replica_writer::<ResourceConvoy>(origin.clone(), "flotilla")
        .apply(WatchEvent::DeletedByName(tombstone), last_synced_at)
        .await
        .expect("apply peer relay tombstone");

    for daemon in [&authority, &peer_one, &peer_two] {
        daemon
            .resource_backend()
            .replica_writer::<ResourceConvoy>(origin.clone(), "flotilla")
            .apply(WatchEvent::Added(stale_object.clone()), stale_synced_at)
            .await
            .expect("exercise a stale relay cycle after deletion");
        assert!(
            daemon
                .resource_backend()
                .including_replicas::<ResourceConvoy>("flotilla")
                .list()
                .await
                .expect("list converged replicas")
                .items
                .is_empty(),
            "authority and both peers must remain absent after stale relay cycles"
        );
    }

    let repeated_id = authority
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: "lost-at-authority".to_string(),
                    replica_origin: None,
                })
                .build(),
        )
        .await
        .expect("repeat delete missing authority resource");
    let repeated = recv_command_finished(&mut authority_events, repeated_id).await;
    assert!(matches!(repeated, CommandValue::ResourceAlreadyDeleted(_)), "repeat delete must be explicit: {repeated:?}");
}

#[tokio::test]
async fn generic_resource_commands_create_usage_and_patch_its_typed_status() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let mut events = daemon.subscribe();
    let name = flotilla_resources::usage_record_name("codex", "ada@example.com");
    let create_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceApply {
                    namespace: "flotilla".to_string(),
                    document: serde_json::json!({
                        "kind": "Usage",
                        "metadata": {"name": name},
                        "spec": {"provider": "codex", "account": "Ada@Example.com"},
                    }),
                })
                .build(),
        )
        .await
        .expect("create usage command");
    let create_result = recv_command_finished(&mut events, create_id).await;
    assert!(matches!(create_result, CommandValue::ResourceObject(response) if response.kind == "Usage"));

    let observed_at = "2026-08-08T18:00:00Z".parse().expect("timestamp");
    let status = flotilla_resources::UsageStatus::builder()
        .windows(vec![flotilla_resources::UsageWindow::builder().name("weekly").used_percent(100.0).build()])
        .observed_at(observed_at)
        .build();

    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceStatusPatch {
                    namespace: "flotilla".to_string(),
                    kind: "usages".to_string(),
                    name: name.clone(),
                    status: serde_json::to_value(&status).expect("encode usage status"),
                })
                .build(),
        )
        .await
        .expect("patch usage status command");

    let result = recv_command_finished(&mut events, command_id).await;
    assert!(matches!(result, CommandValue::ResourceObject(response) if response.kind == "Usage"));

    let malformed_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceStatusPatch {
                    namespace: "flotilla".to_string(),
                    kind: "usages".to_string(),
                    name: name.clone(),
                    status: serde_json::json!({"windows": []}),
                })
                .build(),
        )
        .await
        .expect("malformed status patch command");
    let malformed_result = recv_command_finished(&mut events, malformed_id).await;
    assert!(matches!(malformed_result, CommandValue::Error { message } if message.contains("decode Usage status")));

    let stored = daemon.resource_backend().using::<flotilla_resources::Usage>("flotilla").get(&name).await.expect("stored usage");
    assert_eq!(stored.spec.provider, "codex");
    assert_eq!(stored.spec.account, "Ada@Example.com");
    assert_eq!(stored.status, Some(status));
}

#[tokio::test]
async fn manifest_resolution_command_persists_a_root_spec_token() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let roots = daemon.resource_backend().using::<flotilla_resources::ManifestRoot>("flotilla");
    roots
        .create(
            &InputMeta::builder().name("manifest-test".to_string()).build(),
            &flotilla_resources::ManifestRootSpec::builder()
                .host("local".to_string())
                .path("/tmp/manifests".to_string())
                .source("test".to_string())
                .build(),
        )
        .await
        .expect("create root");
    let policies = daemon.resource_backend().using::<PlacementPolicy>("flotilla");
    let mut meta = InputMeta::builder().name("resolve-me".to_string()).build();
    meta.annotations.insert("flotilla.work/manifest-path".into(), "policy.yaml".into());
    meta.annotations.insert("flotilla.work/manifest-source".into(), "test".into());
    meta.annotations.insert("flotilla.work/manifest-reconciler-root".into(), "local".into());
    policies.create(&meta, &PlacementPolicySpec::builder().pool("live".to_string()).build()).await.expect("create policy");
    let mut events = daemon.subscribe();

    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceManifestResolve {
                    namespace: "flotilla".to_string(),
                    kind: "PlacementPolicy".to_string(),
                    name: "resolve-me".to_string(),
                    resolution: ManifestResolution::Sync,
                    requested_by: "flotilla/implicit".to_string(),
                })
                .build(),
        )
        .await
        .expect("request manifest sync");
    let result = recv_command_finished(&mut events, command_id).await;
    let stored = policies.get("resolve-me").await.expect("policy");
    let root = roots.get("manifest-test").await.expect("root");
    let key = flotilla_resources::DocumentKey {
        path: "policy.yaml".into(),
        kind: "PlacementPolicy".into(),
        namespace: "flotilla".into(),
        name: "resolve-me".into(),
    };
    assert!(matches!(result, CommandValue::ResourceObject(response) if response.kind == "ManifestRoot"));
    assert_eq!(root.spec.resolutions[&key].action, flotilla_resources::ResolutionAction::Sync);
    assert!(!root.spec.resolutions[&key].token.is_empty());
    assert_eq!(root.spec.resolutions[&key].requested_by, "flotilla/implicit");
    assert!(!stored.metadata.annotations.contains_key("flotilla.work/manifest-resolution"));
}

#[tokio::test]
async fn resource_watch_streams_current_update_and_resumed_delete_without_loss() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let convoys = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("watched-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create convoy");
    let ignored = convoys
        .create(
            &InputMeta::builder().name("ignored-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("ignored-wf".to_string()).build(),
        )
        .await
        .expect("create ignored convoy");

    let mut rx = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceWatch {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: Some("watched-convoy".to_string()),
                    include_replicas: false,
                    replica_sources: false,
                    cursor: None,
                })
                .build(),
        )
        .await
        .expect("watch command");

    let watched = loop {
        match recv_event(&mut rx).await {
            DaemonEvent::CommandStepUpdate { command_id: id, status, .. } if id == command_id => {
                let StepStatus::Produced { value } = status else { continue };
                let CommandValue::ResourceWatchEvent(event) = *value else { continue };
                break event;
            }
            _ => {}
        }
    };
    assert_eq!(watched.records.len(), 1);
    assert_eq!(watched.records[0].record_type, flotilla_protocol::ResourceRecordType::Added);
    assert_eq!(watched.records[0].object.as_ref().expect("current object")["metadata"]["name"], "watched-convoy");
    assert!(matches!(watched.records[0].provenance, flotilla_protocol::ResourceRecordProvenance::Local { .. }));

    let _bookmark = loop {
        match recv_event(&mut rx).await {
            DaemonEvent::CommandStepUpdate { command_id: id, status, .. } if id == command_id => {
                let StepStatus::Produced { value } = status else { continue };
                let CommandValue::ResourceWatchEvent(event) = *value else { continue };
                if event.records[0].record_type == flotilla_protocol::ResourceRecordType::Bookmark {
                    break event;
                }
            }
            _ => {}
        }
    };

    let updated_spec = flotilla_resources::ConvoySpec::builder().workflow_ref("wf-v2".to_string()).build();
    convoys
        .update(
            &InputMeta::from(&ignored.metadata),
            &ignored.metadata.resource_version,
            &flotilla_resources::ConvoySpec::builder().workflow_ref("ignored-wf-v2".to_string()).build(),
        )
        .await
        .expect("update ignored convoy");
    convoys
        .update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &updated_spec)
        .await
        .expect("update watched convoy");
    let modified = loop {
        match recv_event(&mut rx).await {
            DaemonEvent::CommandStepUpdate { command_id: id, status, .. } if id == command_id => {
                let StepStatus::Produced { value } = status else { continue };
                let CommandValue::ResourceWatchEvent(event) = *value else { continue };
                if event.records[0].record_type == flotilla_protocol::ResourceRecordType::Modified {
                    break event;
                }
            }
            _ => {}
        }
    };
    let resume_cursor = modified.cursor.clone();
    assert_eq!(modified.records[0].object.as_ref().expect("modified object")["spec"]["workflow_ref"], "wf-v2");

    daemon.cancel(command_id).await.expect("cancel initial watch");
    assert!(matches!(recv_command_finished(&mut rx, command_id).await, CommandValue::Cancelled));
    convoys.delete("watched-convoy").await.expect("delete watched convoy");

    let mut resumed_events = daemon.subscribe();
    let resumed_command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceWatch {
                    namespace: "flotilla".to_string(),
                    kind: "convoys".to_string(),
                    name: Some("watched-convoy".to_string()),
                    include_replicas: false,
                    replica_sources: false,
                    cursor: Some(resume_cursor),
                })
                .build(),
        )
        .await
        .expect("resume watch command");
    let deleted = loop {
        match recv_event(&mut resumed_events).await {
            DaemonEvent::CommandStepUpdate { command_id: id, status, .. } if id == resumed_command_id => {
                let StepStatus::Produced { value } = status else { continue };
                let CommandValue::ResourceWatchEvent(event) = *value else { continue };
                if event.records[0].record_type == flotilla_protocol::ResourceRecordType::Deleted {
                    break event;
                }
            }
            _ => {}
        }
    };
    assert_eq!(deleted.records[0].object.as_ref().expect("deleted object")["metadata"]["name"], "watched-convoy");
    daemon.cancel(resumed_command_id).await.expect("cancel resumed watch");
}

async fn create_test_contained_policy(backend: &flotilla_resources::ResourceBackend, image: &str, agent_adapters: BTreeSet<String>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = match hosts.get("host-test").await {
        Ok(host) => host,
        Err(ResourceError::NotFound { .. }) => hosts
            .create(&InputMeta::builder().name("host-test".to_string()).build(), &HostSpec::default())
            .await
            .expect("test placement host create"),
        Err(error) => panic!("get test placement host: {error}"),
    };
    let mut status = host.status.unwrap_or_default();
    status.ready = true;
    status.heartbeat_at = Some(chrono::Utc::now());
    status.capabilities.insert("docker".to_string(), serde_json::json!(true));
    status.capabilities.insert("os".to_string(), serde_json::json!("linux"));
    status.disk_free_bytes = Some(100 * 1024 * 1024 * 1024);
    status.admission_free_space_floor_bytes = Some(20 * 1024 * 1024 * 1024);
    hosts.update_status("host-test", &host.metadata.resource_version, &status).await.expect("publish test placement host capacity");
    let policy = PlacementPolicySpec::builder()
        .pool("passthrough".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: "host-test".into(),
            image: image.into(),
            pull_policy: Default::default(),
            agent_adapters,
            default_cwd: Some("/workspace".into()),
            env: Default::default(),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".into() },
        })
        .build();
    backend
        .clone()
        .using::<PlacementPolicy>("flotilla")
        .create(&InputMeta::builder().name("docker-test".to_string()).build(), &policy)
        .await
        .expect("contained policy create");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &InputMeta::builder().name("docker-test".to_string()).build(),
            &FulfilmentKindSpec::from_policy(&policy, "linux").expect("docker kind"),
        )
        .await
        .expect("docker kind create");
}

async fn create_test_convoy_project(backend: &flotilla_resources::ResourceBackend, issue_source_bindings: Option<IssueSource>) {
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository create");
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent".to_string()).build(), &single_agent_workflow_spec())
        .await
        .expect("workflow create");
    create_test_contained_policy(backend, "flotilla-test", BTreeSet::from(["codex".to_string()])).await;
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: issue_source_bindings.into_iter().map(Into::into).collect(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project create");
}

#[tokio::test]
async fn fork_stance_refuses_reviewless_dispatch_and_admits_implement_review() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let repository = RepositorySpec::remote("https://forgejo.lab/fork-issues/zellij")
        .expect("fork repository")
        .with_upstream("https://github.com/zellij-org/zellij", RepositoryRelation::Fork)
        .expect("upstream");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository create");
    for (name, workflow) in [("single-agent", single_agent_workflow_spec()), ("implement-review", implement_review_workflow_spec())] {
        backend
            .clone()
            .using::<WorkflowTemplate>("flotilla")
            .create(&InputMeta::builder().name(name.to_string()).build(), &workflow)
            .await
            .expect("workflow create");
    }
    create_test_contained_policy(&backend, "flotilla-test", BTreeSet::from(["codex".to_string(), "claude-code".to_string()])).await;
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&InputMeta::builder().name("claude-max".to_string()).build(), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "test@example.com".to_string() },
            source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("Claude credential create");
    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &InputMeta::builder().name("claude-max-contained".to_string()).build(),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["zellij".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("Claude credential grant create");
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get("host-test").await.expect("test placement host");
    let mut status = host.status.expect("test placement host status");
    status.ready = true;
    status.heartbeat_at = Some(chrono::Utc::now());
    status.capabilities.insert(HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"]));
    hosts.update_status("host-test", &host.metadata.resource_version, &status).await.expect("publish held Claude credential");
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("zellij".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Zellij".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: vec![flotilla_resources::IssueSourceBindingSpec::builder()
                .source(IssueSource { service: "https://forgejo.lab".into(), scope: "fork-issues/zellij".into() })
                .alias("zellij".to_string())
                .build()],
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project create");

    let mut events = daemon.subscribe();
    let start = |name: &str, workflow_ref: &str| {
        Command::builder()
            .action(CommandAction::ConvoyStart {
                intent: Box::new(ConvoyStartIntent {
                    namespace: None,
                    project_ref: "zellij".into(),
                    change_request: None,
                    issues: Vec::new(),
                    name: Some(name.to_string()),
                    branch: Some(format!("stack/{name}")),
                    workflow_ref: Some(workflow_ref.to_string()),
                    inputs: Vec::new(),
                    instruction: None,
                    placement_policy: Some("docker-test".into()),
                    needs: Vec::new(),
                    escalation_reason: None,
                    agent_overrides: Vec::new(),
                    skills: Vec::new(),
                    auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                }),
            })
            .build()
    };
    let rejected_id = daemon.execute(start("reviewless", "single-agent")).await.expect("dispatch command");
    assert_eq!(recv_command_finished(&mut events, rejected_id).await, CommandValue::Error {
        message: "workflow single-agent not permitted for fork-stance repository — use implement-review".to_string()
    });

    let repositories = backend.clone().using::<Repository>("flotilla");
    let stored = repositories.get(&repository.key().to_string()).await.expect("fork repository");
    repositories
        .update(
            &InputMeta::from(&stored.metadata),
            &stored.metadata.resource_version,
            &repository.clone().with_allow_reviewless_workflows(true),
        )
        .await
        .expect("explicit reviewless override");
    let overridden_id = daemon.execute(start("overridden", "single-agent")).await.expect("override dispatch command");
    assert_eq!(recv_command_finished(&mut events, overridden_id).await, CommandValue::ConvoyStarted {
        name: "overridden@zellij".into(),
        attach_plan: None,
        binding: None
    });

    let admitted_id = daemon.execute(start("reviewed", "implement-review")).await.expect("dispatch command");
    assert_eq!(recv_command_finished(&mut events, admitted_id).await, CommandValue::ConvoyStarted {
        name: "reviewed@zellij".into(),
        attach_plan: None,
        binding: None
    });
    let convoy = admitted_convoy(&backend, "reviewed").await;
    assert_eq!(convoy.spec.workflow_ref, "implement-review");
    let workflow = backend.using::<WorkflowTemplate>("flotilla").get("implement-review").await.expect("implement-review workflow");
    assert_eq!(workflow.spec.vessels[0].crew.len(), 2);
    assert!(matches!(
        &workflow.spec.vessels[0].crew[1].source,
        flotilla_resources::CrewSource::Agent { selector, brief_template: Some(template), .. }
            if selector.capability == "code-review" && template == "diff-review"
    ));
}

#[tokio::test]
async fn convoy_start_adopts_pr_identity_and_defaults_to_shepherd_workflow() {
    let response = concat!(
        "HTTP/2.0 200 OK\r\nEtag: \"pr-1071\"\r\nContent-Type: application/json\r\n\r\n",
        r#"{"number":1071,"title":"Convoy adoption of an existing PR","head":{"ref":"feat/existing-pr"},"base":{"ref":"main"},"state":"open","body":"Existing implementation work.","draft":false,"merged_at":null}"#,
    );
    let runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("git", &["--version"], Ok("git version 2.43.0".to_string()))
            .on_run("gh", &["api", "--include", "repos/owner/repo/pulls/1071"], Ok(response.to_string()))
            .on_run(
                "gh",
                &["api", "--include", "repos/owner/repo/pulls/1071", "-H", "If-None-Match: \"pr-1071\""],
                Ok("HTTP/2.0 304 Not Modified\r\nEtag: \"pr-1071\"\r\n\r\n".to_string()),
            )
            .build(),
    );
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon = InProcessDaemon::new(
        Vec::new(),
        test_config_store(temp.path().join("config")),
        fake_discovery_with_runner(false, runner),
        HostName::local(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh available for discovery");
    let backend = daemon.resource_backend();
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent-shepherd".to_string()).build(), &single_agent_shepherd_workflow_spec())
        .await
        .expect("shepherd workflow create");
    let repository_spec = RepositorySpec::remote("https://github.com/owner/repo").expect("repository spec");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("repository create");
    create_test_contained_policy(&backend, "flotilla-test", BTreeSet::from(["codex".to_string()])).await;
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".to_string(),
            default_workflow_ref: "single-agent".to_string(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository_key.clone(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("trunk".to_string()),
            }],
        })
        .await
        .expect("project create");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".to_string(),
                        change_request: Some("1071".to_string()),
                        issues: Vec::new(),
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("PR adoption command accepted");

    assert_eq!(recv_command_finished(&mut events, command_id).await, CommandValue::ConvoyStarted {
        name: "convoy-adoption-of-an-existing-pr-1071@flotilla".to_string(),
        attach_plan: None,
        binding: None,
    });
    let convoy = admitted_convoy(&backend, "convoy-adoption-of-an-existing-pr-1071").await;
    assert_eq!(convoy.spec.workflow_ref, "single-agent-shepherd");
    assert_eq!(convoy.spec.r#ref.as_deref(), Some("feat/existing-pr"));
    assert_eq!(
        convoy.spec.change_request,
        Some(flotilla_resources::BoundChangeRequest {
            id: "1071".to_string(),
            repository_ref: repository_key.clone(),
            title: "Convoy adoption of an existing PR".to_string(),
        })
    );
    assert_eq!(convoy.spec.repositories[0].source_ref, "main");
    assert_eq!(convoy.spec.repositories[0].target_ref, "main");
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let name = flotilla_resources::change_request_record_name("github.com", "owner/repo", 1071);
    let record = records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("owner/repo".to_string())
                .number(1071)
                .observing_authority("test".to_string())
                .build(),
        )
        .await
        .expect("observed change request");
    let at = chrono::Utc::now();
    records
        .update_status(&name, &record.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, at),
            head_sha: flotilla_resources::Observation::unknown(at),
            checks: flotilla_resources::Observation::unknown(at),
            review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: flotilla_resources::Observation::unknown(at) },
            mergeable: flotilla_resources::Observation::unknown(at),
        })
        .await
        .expect("observed status");
    assert_eq!(
        daemon
            .resolve_convoy_change_request(std::slice::from_ref(&repository_key), "feat/existing-pr", Some("1071"))
            .await
            .expect("repeated change request resolution"),
        Some(flotilla_protocol::ConvoyChangeRequest {
            id: "1071".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key,
        })
    );
}

#[tokio::test]
async fn fork_stance_refuses_change_request_merge_without_calling_provider() {
    let provider = Arc::new(FakeChangeRequest::new());
    provider
        .add_change_requests(vec![("42".to_string(), ChangeRequest {
            title: "Keep landing human-owned".to_string(),
            branch: "stack/fork-fix".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            body: None,
            provider_name: "fake-cr".to_string(),
            provider_display_name: "Fake PRs".to_string(),
        })])
        .await;
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_change_request(provider.clone() as Arc<dyn ChangeRequestTracker>),
    );
    let (_temp, repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    daemon.add_repo(&repo).await.expect("adopt test checkout into resources");
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("reconcile repository identity");
    let repository_key = daemon.repository_key_for_path(&repo).await.expect("tracked repository key");
    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    let stored = repositories.get(&repository_key.to_string()).await.expect("stored repository");
    let fork_spec =
        stored.spec.clone().with_upstream("https://github.com/upstream/repo", RepositoryRelation::Fork).expect("fork provenance");
    repositories
        .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, &fork_spec)
        .await
        .expect("apply fork stance");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::MergeChangeRequest { id: "42".to_string(), confirmed: true })
                .context_repo(RepoSelector::Path(repo))
                .build(),
        )
        .await
        .expect("dispatch merge command");

    assert_eq!(recv_command_finished(&mut events, command_id).await, CommandValue::Error {
        message: "merging change request 42 is forbidden for fork-stance repository; landing is human-only".to_string(),
    });
    let (_, request) = provider.get_change_request("42").await.expect("change request should remain available");
    assert_eq!(request.status, flotilla_protocol::ChangeRequestStatus::Open);
}

async fn create_test_host_direct_policy(
    backend: &flotilla_resources::ResourceBackend,
    policy_name: &str,
    host_ref: &str,
    priority: i32,
    agent_adapters: BTreeSet<String>,
) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.create(&InputMeta::builder().name(host_ref.to_string()).build(), &HostSpec::default()).await.expect("host create");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(agent_adapters))].into_iter().collect(),
            heartbeat_at: Some(chrono::Utc::now()),
            ready: true,
            disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
            admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
            resource_store: None,
            ..HostStatus::default()
        })
        .await
        .expect("host status update");
    let policy = PlacementPolicySpec::builder()
        .pool("passthrough".to_string())
        .priority(priority)
        .host_direct(HostDirectPlacementPolicySpec {
            host_ref: host_ref.to_string(),
            checkout: HostDirectPlacementPolicyCheckout::Worktree,
        })
        .build();
    backend
        .clone()
        .using::<PlacementPolicy>("flotilla")
        .create(&InputMeta::builder().name(policy_name.to_string()).build(), &policy)
        .await
        .expect("host-direct policy create");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &InputMeta::builder().name(policy_name.to_string()).build(),
            &FulfilmentKindSpec::from_policy(&policy, "linux").expect("direct kind"),
        )
        .await
        .expect("host-direct kind create");
}

async fn start_capability_convoy(daemon: &InProcessDaemon, name: &str, configure: impl FnOnce(&mut ConvoyStartIntent)) -> CommandValue {
    let mut intent = ConvoyStartIntent::builder()
        .project_ref("flotilla".to_string())
        .name(name.to_string())
        .branch(format!("work/{name}"))
        .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
        .build();
    configure(&mut intent);
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command::builder().action(CommandAction::ConvoyStart { intent: Box::new(intent) }).build())
        .await
        .expect("dispatch capability convoy");
    recv_command_finished(&mut events, command_id).await
}

#[tokio::test]
async fn capability_admission_names_unobserved_kind_facts() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-unobserved", "unobserved", 100, BTreeSet::from(["codex".to_string()])).await;

    let result = start_capability_convoy(&daemon, "unobserved-harness", |intent| {
        intent.needs.push("harness:claude-code>=2.1.300".to_string());
    })
    .await;
    assert!(
        matches!(&result, CommandValue::Error { message } if message.contains("facts not yet observed for kind host-direct-unobserved")),
        "{result:?}"
    );
    let structurally_refused = start_capability_convoy(&daemon, "unobserved-and-wrong-platform", |intent| {
        intent.needs.push("harness:claude-code>=2.1.300".to_string());
        intent.needs.push("platform:windows".to_string());
    })
    .await;
    assert!(
        matches!(&structurally_refused, CommandValue::Error { message } if message.contains("uncovered `platform:windows`") && !message.contains("facts not yet observed")),
        "{structurally_refused:?}"
    );
}

#[tokio::test]
async fn ad_hoc_matrix_need_is_refused_before_placement() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    let result = start_capability_convoy(&daemon, "ad-hoc-matrix", |intent| {
        intent.needs.push("platform:$matrix".to_string());
    })
    .await;
    assert!(
        matches!(&result, CommandValue::Error { message } if message.contains("platform:$matrix is only valid on workflow roles or Project role needs")),
        "{result:?}"
    );
}

#[tokio::test]
async fn model_selector_admission_names_unobserved_kind_facts() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(
        &backend,
        "host-direct-model-unknown",
        "model-unknown",
        100,
        BTreeSet::from(["claude-code".to_string()]),
    )
    .await;

    let result = start_capability_convoy(&daemon, "model-not-yet-observed", |intent| {
        intent.agent_overrides.push(flotilla_protocol::AgentOverride {
            capability: "code".to_string(),
            adapter: "claude-code".to_string(),
            model: Some("preview".to_string()),
        });
    })
    .await;
    assert!(
        matches!(&result, CommandValue::Error { message } if message.contains("facts not yet observed for kind") && message.contains("host-direct-model-unknown")),
        "{result:?}"
    );
}

#[tokio::test]
async fn capability_placement_prefers_docker_and_records_pinned_escalation() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-linux", "direct-linux", 100, BTreeSet::from(["codex".to_string()])).await;

    let docs = start_capability_convoy(&daemon, "docs-only", |_| {}).await;
    assert!(matches!(docs, CommandValue::ConvoyStarted { .. }), "{docs:?}");
    let docs = admitted_convoy(&backend, "docs-only").await;
    assert_eq!(docs.status.expect("admitted status").placement_decision.expect("placement").policy_name, "docker-test");

    let refused = start_capability_convoy(&daemon, "pinned-without-reason", |intent| {
        intent.placement_policy = Some("host-direct-linux".to_string());
    })
    .await;
    assert!(
        matches!(refused, CommandValue::Error { message } if message.contains("--escalation-reason") && message.contains("docker-test"))
    );

    let escalated = start_capability_convoy(&daemon, "pinned-with-reason", |intent| {
        intent.placement_policy = Some("host-direct-linux".to_string());
        intent.escalation_reason = Some("operator needs host tools".to_string());
    })
    .await;
    assert!(matches!(escalated, CommandValue::ConvoyStarted { .. }), "{escalated:?}");
    let escalated = admitted_convoy(&backend, "pinned-with-reason").await;
    let decision = escalated.status.expect("admitted status").placement_decision.expect("placement");
    assert_eq!(decision.policy_name, "host-direct-linux");
    assert_eq!(decision.minimal_alternatives, ["docker-test"]);
    assert_eq!(decision.escalation_reason.as_deref(), Some("operator needs host tools"));
}

#[tokio::test]
async fn capability_admission_queues_on_sleeping_minimal_host() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-sleeping", "sleeping-host", 0, BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("sleeping-host").await.expect("host");
    let mut status = host.status.expect("host status");
    status.ready = false;
    status.sleeping_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    hosts.update_status("sleeping-host", &host.metadata.resource_version, &status).await.expect("sleeping host");

    let result = start_capability_convoy(&daemon, "sleeping-capacity", |intent| {
        intent.needs.push("host_account_reach".to_string());
    })
    .await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let convoy = admitted_convoy(&backend, "sleeping-capacity").await;
    let decision = convoy.status.expect("status").placement_decision.expect("placement");
    assert_eq!(decision.policy_name, "host-direct-sleeping");
    let allocation = decision.allocation.expect("frozen allocation");
    let selected = allocation.candidates.iter().find(|candidate| candidate.kind == allocation.chosen_kind).expect("candidate");
    assert!(!selected.available);
    assert!(selected.sleeping_until.is_some());
}

#[tokio::test]
async fn capability_admission_refuses_unready_host_without_sleep_intent() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-stopped", "stopped-host", 0, BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("stopped-host").await.expect("host");
    let mut status = host.status.expect("host status");
    status.ready = false;
    hosts.update_status("stopped-host", &host.metadata.resource_version, &status).await.expect("stopped host");

    let result = start_capability_convoy(&daemon, "stopped-host-refusal", |intent| {
        intent.needs.push("host_account_reach".to_string());
        intent.placement_policy = Some("host-direct-stopped".to_string());
    })
    .await;
    assert!(
        matches!(&result, CommandValue::Error { message } if message.contains("host-direct-stopped") && message.contains("stopped-host") && message.contains("not ready")),
        "{result:?}"
    );

    let host = hosts.get("stopped-host").await.expect("host after refusal");
    let mut status = host.status.expect("host status after refusal");
    status.ready = true;
    status.heartbeat_at = Some(chrono::Utc::now() - chrono::Duration::seconds(flotilla_resources::HEARTBEAT_READY_TTL_SECS + 1));
    hosts.update_status("stopped-host", &host.metadata.resource_version, &status).await.expect("stale heartbeat");
    let stale = start_capability_convoy(&daemon, "stale-host-refusal", |intent| {
        intent.needs.push("host_account_reach".to_string());
        intent.placement_policy = Some("host-direct-stopped".to_string());
    })
    .await;
    assert!(matches!(&stale, CommandValue::Error { message } if message.contains("heartbeat is stale")), "{stale:?}");
}

#[tokio::test]
async fn capability_admission_prefers_linux_when_macos_is_reserved() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "macos-scarce", "comte", 0, BTreeSet::from(["codex".to_string()])).await;
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let kind = kinds.get("macos-scarce").await.expect("macOS kind");
    let mut spec = kind.spec.clone();
    spec.grants.remove(&FulfilmentGrant::Platform("linux".to_string()));
    spec.grants.insert(FulfilmentGrant::Platform("macos".to_string()));
    kinds.update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec).await.expect("macOS grants");
    create_test_host_direct_policy(&backend, "linux-available", "feta", 0, BTreeSet::from(["codex".to_string()])).await;

    let started = start_capability_convoy(&daemon, "reserved-unpinned", |intent| {
        intent.needs.push("host_account_reach".to_string());
    })
    .await;
    assert!(matches!(started, CommandValue::ConvoyStarted { .. }), "{started:?}");
    let convoy = admitted_convoy(&backend, "reserved-unpinned").await;
    let decision = convoy.status.expect("status").placement_decision.expect("placement");
    assert_eq!(decision.policy_name, "linux-available");
    assert_eq!(decision.allocation.expect("allocation").reservation_reason, None);
}

#[tokio::test]
async fn capability_admission_uses_only_macos_without_escalation_and_explains_fallback() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "macos-scarce", "comte", 0, BTreeSet::from(["codex".to_string()])).await;
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let kind = kinds.get("macos-scarce").await.expect("macOS kind");
    let mut spec = kind.spec.clone();
    spec.grants.remove(&FulfilmentGrant::Platform("linux".to_string()));
    spec.grants.insert(FulfilmentGrant::Platform("macos".to_string()));
    kinds.update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec).await.expect("macOS grants");

    let started = start_capability_convoy(&daemon, "reserved-only", |intent| {
        intent.needs.push("host_account_reach".to_string());
    })
    .await;
    assert!(matches!(started, CommandValue::ConvoyStarted { .. }), "{started:?}");
    let convoy = admitted_convoy(&backend, "reserved-only").await;
    let decision = convoy.status.expect("status").placement_decision.expect("placement");
    assert_eq!(decision.policy_name, "macos-scarce");
    assert_eq!(decision.escalation_reason, None);
    let allocation = decision.allocation.expect("allocation");
    assert_eq!(allocation.chosen_kind, "macos-scarce");
    assert_eq!(allocation.reservation_reason.as_deref(), Some("no unreserved capacity covers the needs"));
    assert!(allocation.candidates.iter().any(|candidate| candidate.kind == "macos-scarce" && candidate.reserved_for_platform));
}

#[tokio::test]
async fn capability_admission_pin_to_macos_requires_escalation_when_linux_exists() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "macos-scarce", "comte", 0, BTreeSet::from(["codex".to_string()])).await;
    create_test_host_direct_policy(&backend, "linux-available", "feta", 0, BTreeSet::from(["codex".to_string()])).await;
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let kind = kinds.get("macos-scarce").await.expect("macOS kind");
    let mut spec = kind.spec.clone();
    spec.grants.remove(&FulfilmentGrant::Platform("linux".to_string()));
    spec.grants.insert(FulfilmentGrant::Platform("macos".to_string()));
    kinds.update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec).await.expect("macOS grants");

    let pinned = start_capability_convoy(&daemon, "reserved-pinned", |intent| {
        intent.needs.push("host_account_reach".to_string());
        intent.placement_policy = Some("macos-scarce".to_string());
    })
    .await;
    assert!(matches!(&pinned, CommandValue::Error { message } if message.contains("--escalation-reason")), "{pinned:?}");

    let escalated = start_capability_convoy(&daemon, "reserved-escalated", |intent| {
        intent.needs.push("host_account_reach".to_string());
        intent.placement_policy = Some("macos-scarce".to_string());
        intent.escalation_reason = Some("operator needs this host".to_string());
    })
    .await;
    assert!(matches!(escalated, CommandValue::ConvoyStarted { .. }), "{escalated:?}");
    let convoy = admitted_convoy(&backend, "reserved-escalated").await;
    let allocation = convoy.status.expect("status").placement_decision.expect("placement").allocation.expect("allocation");
    assert_eq!(allocation.chosen_kind, "macos-scarce");
    assert!(allocation.candidates.iter().any(|candidate| candidate.kind == "macos-scarce" && candidate.reserved_for_platform));
}

#[tokio::test]
async fn capability_placement_selects_each_vessel_from_its_own_role_needs() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-tools", "direct-tools", 0, BTreeSet::new()).await;
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .vessels(vec![
            flotilla_resources::VesselRequirement::builder()
                .name("docs".to_string())
                .crew(vec![flotilla_resources::CrewSpec::builder()
                    .role("writer".to_string())
                    .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
                    .build()])
                .build(),
            flotilla_resources::VesselRequirement::builder()
                .name("tools".to_string())
                .crew(vec![flotilla_resources::CrewSpec::builder()
                    .role("operator".to_string())
                    .needs(BTreeSet::from([flotilla_resources::CapabilityNeed::HostAccountReach]))
                    .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
                    .build()])
                .build(),
        ])
        .build();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("mixed-needs".to_string()).build(), &workflow)
        .await
        .expect("workflow");

    let result = start_capability_convoy(&daemon, "mixed-needs", |intent| {
        intent.workflow_ref = Some("mixed-needs".to_string());
    })
    .await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let admitted = admitted_convoy(&backend, "mixed-needs").await;
    let docs = flotilla_resources::vessel_placement_pin(&admitted, "docs").expect("docs pin");
    let tools = flotilla_resources::vessel_placement_pin(&admitted, "tools").expect("tools pin");
    assert_eq!(docs.decision.policy_name, "docker-test");
    assert_eq!(tools.decision.policy_name, "host-direct-tools");
    assert_ne!(docs.policy_ref, tools.policy_ref);
}

#[tokio::test]
async fn allocation_shares_equal_need_roles_and_explains_grouping() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    let crew = |role: &str| {
        flotilla_resources::CrewSpec::builder()
            .role(role.to_string())
            .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
            .build()
    };
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder().name("shared-needs".to_string()).build(),
            &flotilla_resources::WorkflowTemplateSpec::builder()
                .roles(vec![crew("coder"), crew("reviewer")])
                .handoffs(vec![flotilla_resources::RoleHandoff { from: "coder".to_string(), to: "reviewer".to_string() }])
                .build(),
        )
        .await
        .expect("workflow");
    let result = start_capability_convoy(&daemon, "shared-needs", |intent| intent.workflow_ref = Some("shared-needs".to_string())).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let admitted = admitted_convoy(&backend, "shared-needs").await;
    let snapshot_name = admitted.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).expect("pinned workflow");
    let snapshot = backend.definitions::<WorkflowTemplate>("flotilla").get(snapshot_name).await.expect("snapshot");
    assert_eq!(snapshot.spec.vessels.len(), 1);
    assert_eq!(snapshot.spec.vessels[0].crew.len(), 2);
    let result = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".to_string()), name: "shared-needs".to_string() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain");
    let CommandValue::ConvoyExplanation(explanation) = result else { panic!("expected explanation: {result:?}") };
    assert_eq!(explanation.allocation.len(), 1);
    assert!(explanation.allocation[0].crossed_handoffs.is_empty());
}

#[tokio::test]
async fn allocation_separates_linux_coder_from_gui_verifier() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-gui", "gui-host", 0, BTreeSet::new()).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("gui-host").await.expect("host");
    let mut status = host.status.expect("host status");
    status.fulfilment_facts.insert("host-direct-gui".to_string(), FulfilmentFacts { gui_session_logged_in: true, ..Default::default() });
    hosts.update_status(&host.metadata.name, &host.metadata.resource_version, &status).await.expect("GUI facts");
    let crew = |role: &str, need: &str| {
        flotilla_resources::CrewSpec::builder()
            .role(role.to_string())
            .needs(BTreeSet::from([need.parse().expect("need")]))
            .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
            .build()
    };
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .roles(vec![crew("coder", "platform:linux"), crew("verify", "gui_session")])
        .build();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("gui-verify".to_string()).build(), &workflow)
        .await
        .expect("workflow");
    let result = start_capability_convoy(&daemon, "gui-verify", |intent| intent.workflow_ref = Some("gui-verify".to_string())).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let admitted = admitted_convoy(&backend, "gui-verify").await;
    assert_eq!(flotilla_resources::vessel_placement_pin(&admitted, "coder").expect("coder pin").decision.policy_name, "docker-test");
    assert_eq!(
        flotilla_resources::vessel_placement_pin(&admitted, "verify").expect("verifier pin").decision.policy_name,
        "host-direct-gui"
    );
}

#[tokio::test]
async fn allocation_expands_project_platform_matrix() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    let projects = backend.using::<Project>("flotilla");
    let project = projects.get("flotilla").await.expect("project");
    let mut project_spec = project.spec.clone();
    project_spec.platform_matrix = vec!["macos".to_string(), "windows".to_string()];
    projects.update(&InputMeta::from(&project.metadata), &project.metadata.resource_version, &project_spec).await.expect("update matrix");
    for platform in ["macos", "windows"] {
        let kind_name = format!("direct-{platform}");
        create_test_host_direct_policy(&backend, &kind_name, &format!("host-{platform}"), 0, BTreeSet::new()).await;
        let kinds = backend.using::<FulfilmentKind>("flotilla");
        let kind = kinds.get(&kind_name).await.expect("kind");
        let mut spec = kind.spec.clone();
        spec.grants.remove(&flotilla_resources::FulfilmentGrant::Platform("linux".to_string()));
        spec.grants.insert(flotilla_resources::FulfilmentGrant::Platform(platform.to_string()));
        kinds.update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec).await.expect("platform kind");
    }
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .roles(vec![flotilla_resources::CrewSpec::builder()
            .role("verify".to_string())
            .needs(BTreeSet::from(["platform:$matrix".parse().expect("matrix need")]))
            .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
            .build()])
        .build();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("matrix-verify".to_string()).build(), &workflow)
        .await
        .expect("workflow");
    let result = start_capability_convoy(&daemon, "matrix-verify", |intent| intent.workflow_ref = Some("matrix-verify".to_string())).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let admitted = admitted_convoy(&backend, "matrix-verify").await;
    let snapshot_name = admitted.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).expect("pinned workflow");
    let snapshot = backend.definitions::<WorkflowTemplate>("flotilla").get(snapshot_name).await.expect("snapshot");
    assert_eq!(snapshot.spec.vessels.iter().map(|vessel| vessel.name.as_str()).collect::<Vec<_>>(), ["verify[macos]", "verify[windows]"]);
}

#[tokio::test]
async fn allocation_backtracks_a_legacy_union_no_kind_covers() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "direct-linux", "host-linux", 0, BTreeSet::new()).await;
    create_test_host_direct_policy(&backend, "direct-macos", "host-macos", 0, BTreeSet::new()).await;
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let kind = kinds.get("direct-macos").await.expect("kind");
    let mut spec = kind.spec.clone();
    spec.grants.remove(&flotilla_resources::FulfilmentGrant::Platform("linux".to_string()));
    spec.grants.insert(flotilla_resources::FulfilmentGrant::Platform("macos".to_string()));
    kinds.update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec).await.expect("mac kind");
    let crew = |role: &str, platform: &str| {
        flotilla_resources::CrewSpec::builder()
            .role(role.to_string())
            .needs(BTreeSet::from([flotilla_resources::CapabilityNeed::Platform(platform.to_string())]))
            .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
            .build()
    };
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .vessels(vec![
            flotilla_resources::VesselRequirement::builder()
                .name("work".to_string())
                .crew(vec![crew("coder", "linux"), crew("verifier", "macos")])
                .build(),
            flotilla_resources::VesselRequirement::builder()
                .name("prepare".to_string())
                .crew(vec![flotilla_resources::CrewSpec::builder()
                    .role("planner".to_string())
                    .needs(BTreeSet::from([flotilla_resources::CapabilityNeed::HostAccountReach]))
                    .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
                    .build()])
                .build(),
        ])
        .handoffs(vec![flotilla_resources::RoleHandoff { from: "planner".to_string(), to: "verifier".to_string() }])
        .build();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("backtrack".to_string()).build(), &workflow)
        .await
        .expect("workflow");
    let result = start_capability_convoy(&daemon, "backtrack", |intent| intent.workflow_ref = Some("backtrack".to_string())).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let admitted = admitted_convoy(&backend, "backtrack").await;
    let snapshot_name = admitted.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).expect("pinned workflow");
    let snapshot = backend.definitions::<WorkflowTemplate>("flotilla").get(snapshot_name).await.expect("snapshot");
    assert_eq!(snapshot.spec.vessels.len(), 3);
    assert!(snapshot.spec.vessels.iter().any(|vessel| vessel.name == "work[coder]"));
    assert!(snapshot.spec.vessels.iter().any(|vessel| vessel.name == "work[verifier]"));
    assert!(snapshot
        .spec
        .allocation
        .iter()
        .any(|decision| { decision.vessel == "work[verifier]" && decision.crossed_handoffs == ["planner -> verifier"] }));
}

#[tokio::test]
async fn conflicting_role_platform_needs_split_and_name_uncovered_role() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![
                flotilla_resources::CrewSpec::builder()
                    .role("linux".to_string())
                    .needs(BTreeSet::from([flotilla_resources::CapabilityNeed::Platform("linux".to_string())]))
                    .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
                    .build(),
                flotilla_resources::CrewSpec::builder()
                    .role("windows".to_string())
                    .needs(BTreeSet::from([flotilla_resources::CapabilityNeed::Platform("windows".to_string())]))
                    .source(flotilla_resources::CrewSource::Tool { command: "true".to_string() })
                    .build(),
            ])
            .build()])
        .build();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("conflicting-platforms".to_string()).build(), &workflow)
        .await
        .expect("workflow");
    let result = start_capability_convoy(&daemon, "conflicting-platforms", |intent| {
        intent.workflow_ref = Some("conflicting-platforms".to_string());
    })
    .await;
    assert!(
        matches!(&result, CommandValue::Error { message }
        if message.contains("role `windows`") && message.contains("platform:windows")),
        "{result:?}"
    );
}

#[tokio::test]
async fn requested_model_derives_harness_need_and_selects_host_direct() {
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    create_test_host_direct_policy(&backend, "host-direct-model", "direct-model", 0, BTreeSet::from(["claude-code".to_string()])).await;
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&InputMeta::builder().name("claude-max".to_string()).build(), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "test@example.com".to_string() },
            source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("Claude credential");
    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &InputMeta::builder().name("claude-work".to_string()).build(),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("Claude grant");
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get("direct-model").await.expect("model host");
    let mut status = host.status.expect("host status");
    status.fulfilment_facts.insert("host-direct-model".to_string(), FulfilmentFacts {
        harnesses: BTreeMap::from([("claude-code".to_string(), HarnessFacts {
            version: "2.1.300".to_string(),
            models: BTreeMap::from([
                ("preview".to_string(), ModelFact { usable: true, source: ModelFactSource::Probe }),
                ("unavailable".to_string(), ModelFact { usable: false, source: ModelFactSource::Probe }),
                ("mixed".to_string(), ModelFact { usable: false, source: ModelFactSource::Probe }),
            ]),
        })]),
        observed_at: chrono::Utc::now(),
        ..Default::default()
    });
    // A second observed harness that rejects `unavailable` but has no verdict
    // on `mixed`, so `mixed` is rejected by only some observed harnesses.
    status.fulfilment_facts.insert("unprobed-model-kind".to_string(), FulfilmentFacts {
        harnesses: BTreeMap::from([("claude-code".to_string(), HarnessFacts {
            version: "2.1.300".to_string(),
            models: BTreeMap::from([("unavailable".to_string(), ModelFact { usable: false, source: ModelFactSource::Probe })]),
        })]),
        observed_at: chrono::Utc::now(),
        ..Default::default()
    });
    status.capabilities.insert(HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"]));
    hosts.update_status("direct-model", &host.metadata.resource_version, &status).await.expect("publish model facts");

    let result = start_capability_convoy(&daemon, "model-driven", |intent| {
        intent.agent_overrides.push(flotilla_protocol::AgentOverride {
            capability: "code".to_string(),
            adapter: "claude-code".to_string(),
            model: Some("preview".to_string()),
        });
    })
    .await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let admitted = admitted_convoy(&backend, "model-driven").await;
    let snapshot_name = admitted.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).expect("pinned workflow");
    let snapshot = backend.definitions::<WorkflowTemplate>("flotilla").get(snapshot_name).await.expect("workflow snapshot");
    let status = admitted.status.expect("admitted status");
    assert_eq!(status.placement_decision.expect("placement").policy_name, "host-direct-model");
    let needs = &snapshot.spec.vessels[0].crew[0].needs;
    assert!(needs.contains(&flotilla_resources::CapabilityNeed::Harness {
        adapter: "claude-code".to_string(),
        minimum_version: "2.1.300".to_string(),
    }));
    let result = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".to_string()), name: "model-driven".to_string() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain query");
    let CommandValue::ConvoyExplanation(explanation) = result else {
        panic!("explanation should be available after admission: {result:?}");
    };
    assert!(explanation.role_needs.values().flatten().any(|need| need == "harness:claude-code>=2.1.300"));
    assert_eq!(explanation.vessel_placements["work"].policy_name, "host-direct-model");
    let refused = start_capability_convoy(&daemon, "model-unavailable", |intent| {
        intent.agent_overrides.push(flotilla_protocol::AgentOverride {
            capability: "code".to_string(),
            adapter: "claude-code".to_string(),
            model: Some("unavailable".to_string()),
        });
    })
    .await;
    assert!(
        matches!(refused, CommandValue::Error { message } if message.contains("no observed claude-code harness accepts model unavailable"))
    );
    // A model the harness has no fact about is unknown, not rejected: admit
    // without a version floor.
    let admitted = start_capability_convoy(&daemon, "model-unknown", |intent| {
        intent.agent_overrides.push(flotilla_protocol::AgentOverride {
            capability: "code".to_string(),
            adapter: "claude-code".to_string(),
            model: Some("unprobed".to_string()),
        });
    })
    .await;
    assert!(!matches!(admitted, CommandValue::Error { .. }), "unknown model acceptance must not refuse: {admitted:?}");
    // Refuse only when every observed harness explicitly rejects: one rejection
    // plus one unknown verdict admits.
    let admitted = start_capability_convoy(&daemon, "model-mixed", |intent| {
        intent.agent_overrides.push(flotilla_protocol::AgentOverride {
            capability: "code".to_string(),
            adapter: "claude-code".to_string(),
            model: Some("mixed".to_string()),
        });
    })
    .await;
    assert!(!matches!(admitted, CommandValue::Error { .. }), "a partial rejection must not refuse: {admitted:?}");
}

#[tokio::test]
async fn issue_platform_need_refuses_without_a_covering_kind() {
    let source = IssueSource { service: "https://github.com".to_string(), scope: "flotilla-org/flotilla".to_string() };
    let reference = IssueRef { source: source.clone(), id: "2142".to_string() };
    let mut issue = TestIssue::new("Windows-only bug").id("2142").with_labels(vec!["needs:platform:windows".to_string()]).build();
    issue.reference = reference.clone();
    let provider = Arc::new(FakeIssueProvider::new());
    provider.add_issues(vec![(reference.id.clone(), issue)]).await;
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_tracker(provider as Arc<dyn flotilla_core::providers::issue_tracker::IssueProvider>),
    );
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    create_test_convoy_project(&daemon.resource_backend(), Some(source)).await;
    let result = start_capability_convoy(&daemon, "windows-bug", |intent| {
        intent.issues.push(IssueSelector::Reference(reference));
    })
    .await;
    assert!(
        matches!(result, CommandValue::Error { message } if message.contains("platform:windows") && message.contains("no fulfilment kind"))
    );
}

#[tokio::test]
async fn host_direct_convoy_start_uses_minimal_available_kind() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"local-host\"\n").expect("write daemon config");
    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(config_base)), fake_discovery(false), HostName::local()).await;
    let local_host_ref = daemon.local_host_id().expect("local host id").to_string();
    let backend = daemon.resource_backend();
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository create");
    let workflow = single_agent_workflow_spec();

    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent".to_string()).build(), &workflow)
        .await
        .expect("workflow create");
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project create");
    create_test_host_direct_policy(&backend, "host-direct-a-empty", "empty-host", 200, BTreeSet::new()).await;
    create_test_host_direct_policy(&backend, "host-direct-b-remote", "remote-host", 100, BTreeSet::from(["codex".to_string()])).await;
    create_test_host_direct_policy(&backend, "host-direct-z-local", &local_host_ref, -100, BTreeSet::from(["codex".to_string()])).await;

    let mut events = daemon.subscribe();
    let implicit_command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("local-default".into()),
                        branch: Some("fix/local-default".into()),
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("start command accepted");

    let implicit_result = recv_command_finished(&mut events, implicit_command_id).await;
    assert_eq!(implicit_result, CommandValue::ConvoyStarted { name: "local-default@flotilla".into(), attach_plan: None, binding: None });

    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("explicit-default".into()),
                        branch: Some("fix/explicit-default".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("explicitly acknowledged start command accepted");

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("start command should finish");
    assert_eq!(result, CommandValue::ConvoyStarted { name: "explicit-default@flotilla".into(), attach_plan: None, binding: None });
    let convoy = admitted_convoy(&backend, "explicit-default").await;
    assert_eq!(convoy.spec.placement_policy.as_deref(), Some("host-direct-b-remote"));
    let decision =
        convoy.status.and_then(|status| status.placement_decision).expect("admission should persist the complete placement decision");
    assert_eq!(decision.policy_name, "host-direct-b-remote");
    assert!(decision.refused_candidates.is_empty());
    assert_eq!(decision.viable_not_selected.len(), 1);
    assert_eq!(decision.viable_not_selected[0].policy_name, "host-direct-z-local");
    assert_eq!(decision.viable_not_selected[0].reason, "minimal alternative");
}

#[tokio::test]
async fn convoy_start_rejects_agent_adapter_missing_from_docker_placement() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository create");
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent".to_string()).build(), &single_agent_workflow_spec())
        .await
        .expect("workflow create");
    create_test_contained_policy(&backend, "ubuntu:24.04", BTreeSet::new()).await;
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project create");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("missing-adapter".into()),
                        branch: Some("fix/missing-adapter".into()),
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: Some("docker-test".into()),
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("start command accepted");
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("start command should finish");

    assert!(matches!(result, CommandValue::Error { message } if message.contains("workflow requires agent adapter `codex`")));
    assert!(matches!(
        backend.using::<ResourceConvoy>("flotilla").get("missing-adapter").await,
        Err(flotilla_resources::ResourceError::NotFound { .. })
    ));

    let legacy_command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "missing-adapter-legacy".into(),
                    workflow_ref: "single-agent".into(),
                    inputs: Vec::new(),
                    repository_url: None,
                    r#ref: Some("fix/missing-adapter-legacy".into()),
                    project_ref: Some("flotilla".into()),
                    placement_policy: Some("docker-test".into()),
                    adopted_checkout: None,
                })
                .build(),
        )
        .await
        .expect("legacy create command accepted");
    let legacy_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == legacy_command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("legacy create command should finish");

    assert_eq!(legacy_result, CommandValue::Error {
        message: "workflow requires agent adapter `codex`, which is not available in placement `docker-test` (image `ubuntu:24.04`)"
            .to_string()
    });
    assert!(matches!(
        backend.using::<ResourceConvoy>("flotilla").get("missing-adapter-legacy").await,
        Err(flotilla_resources::ResourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn convoy_start_accepts_project_list_identifier() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository create");
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent".to_string()).build(), &single_agent_workflow_spec())
        .await
        .expect("workflow create");
    create_test_contained_policy(&backend, "flotilla-test", BTreeSet::from(["codex".to_string()])).await;
    backend
        .clone()
        .definitions::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project create");

    let list_result = daemon
        .execute_query(Command::builder().action(CommandAction::QueryProjectList {}).build(), uuid::Uuid::new_v4())
        .await
        .expect("project list");
    let CommandValue::ProjectList(projects) = list_result else {
        panic!("expected project list, got {list_result:?}");
    };
    let listed = projects.projects.first().expect("listed project");
    let project_identifiers = [format!("{}/{}", listed.namespace, listed.name), listed.address.human_label()];

    let mut events = daemon.subscribe();
    for (index, project_ref) in project_identifiers.into_iter().enumerate() {
        let name = format!("listed-project-{index}");
        let start_id = daemon
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyStart {
                        intent: Box::new(ConvoyStartIntent {
                            namespace: None,
                            project_ref,
                            change_request: None,
                            issues: Vec::new(),
                            name: Some(name.clone()),
                            branch: Some(format!("fix/{name}")),
                            workflow_ref: None,
                            inputs: Vec::new(),
                            instruction: None,
                            placement_policy: None,
                            needs: Vec::new(),
                            escalation_reason: None,
                            agent_overrides: Vec::new(),
                            skills: Vec::new(),
                            auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                        }),
                    })
                    .build(),
            )
            .await
            .expect("convoy start command accepted");

        assert_eq!(recv_command_finished(&mut events, start_id).await, CommandValue::ConvoyStarted {
            name: format!("{name}@flotilla"),
            attach_plan: None,
            binding: None
        });
        let convoy = admitted_convoy(&backend, &name).await;
        assert_eq!(convoy.spec.project_ref.as_deref(), Some("flotilla"));
    }
}

#[tokio::test]
async fn convoy_start_unknown_project_reports_resolved_reference_tried() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let mut events = daemon.subscribe();

    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "missing".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("unknown-project".into()),
                        branch: Some("fix/unknown-project".into()),
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("convoy start command accepted");

    assert_eq!(recv_command_finished(&mut events, command_id).await, CommandValue::Error {
        message: "project flotilla/missing is not ready: resource not found: missing (tried flotilla/missing)".into()
    });
}

#[tokio::test]
async fn convoy_start_admits_fully_specified_issue_intent_as_one_persisted_snapshot() {
    let reference =
        IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/planning".into() }, id: "732".into() };
    let reference_two =
        IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/planning".into() }, id: "733".into() };
    let mut issue = TestIssue::new("Start convoy from an issue").id("732").with_labels(vec!["enhancement".into()]).build();
    issue.reference = reference.clone();
    issue.body = Some("Capture the issue at admission time.".into());
    let mut issue_two = TestIssue::new("Carry a second issue").id("733").with_labels(vec!["quick-win".into()]).build();
    issue_two.reference = reference_two.clone();
    issue_two.body = Some("Include this issue in the same convoy.".into());
    let provider = Arc::new(FakeIssueProvider::new());
    provider.add_issues(vec![(reference.id.clone(), issue.clone()), (reference_two.id.clone(), issue_two.clone())]).await;
    let utility = Arc::new(CountingConvoyAiUtility { calls: AtomicUsize::new(0), fail: AtomicBool::new(false) });
    let mut discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_tracker(provider as Arc<dyn flotilla_core::providers::issue_tracker::IssueProvider>),
    );
    discovery.factories.ai_utilities.push(Box::new(CountingConvoyAiUtilityFactory { utility: Arc::clone(&utility) }));
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    daemon.connect_surface(uuid::Uuid::new_v4(), flotilla_protocol::SurfaceDeclaration::ambient_for_namespace("flotilla"));
    let backend = daemon.resource_backend();
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/planning")
        .expect("repository spec")
        .update_remotes("https://github-ssh-backend.example/flotilla-org/planning")
        .expect("live alias remote");
    let secondary_repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("secondary repository spec");
    for spec in [&repository, &secondary_repository] {
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(spec.key().to_string()).build(), spec)
            .await
            .expect("repository create");
    }
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent".to_string()).build(), &single_agent_workflow_spec())
        .await
        .expect("workflow create");
    create_test_contained_policy(&backend, "flotilla-test", BTreeSet::from(["codex".to_string()])).await;
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: vec![flotilla_resources::IssueSourceBindingSpec::builder()
                .source(reference.source.clone())
                .alias("planning".to_string())
                .build()],
            dispatch_policy: None,
            repositories: vec![
                ProjectRepositorySpec {
                    repo: repository.key(),
                    alias: None,
                    roles: Default::default(),
                    subpath: None,
                    default_branch: Some("main".into()),
                },
                ProjectRepositorySpec {
                    repo: secondary_repository.key(),
                    alias: None,
                    roles: Default::default(),
                    subpath: None,
                    default_branch: Some("main".into()),
                },
            ],
        })
        .await
        .expect("project create");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: vec![IssueSelector::Alias { alias: "planning".into(), id: reference.id.clone() }],
                        name: Some("issue-732".into()),
                        branch: Some("fix/issue-732".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: vec![("review".into(), "required".into())],
                        instruction: Some("Keep the snapshot durable.".into()),
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("start command accepted");
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("start command should finish");

    assert_eq!(result, CommandValue::ConvoyStarted { name: "issue-732@flotilla".into(), attach_plan: None, binding: None });
    let persisted = admitted_convoy(&backend, "issue-732").await;
    assert_eq!(persisted.spec.project_ref.as_deref(), Some("flotilla"));
    assert_eq!(persisted.spec.workflow_ref, "single-agent");
    assert_eq!(persisted.spec.dispatching_principal_ref, flotilla_protocol::PrincipalRef::implicit_for_namespace("flotilla"));
    assert_eq!(persisted.spec.r#ref.as_deref(), Some("fix/issue-732"));
    assert_eq!(persisted.spec.placement_policy.as_deref(), Some("docker-test"));
    assert_eq!(persisted.spec.repositories.len(), 2);
    assert_eq!(persisted.spec.instruction.as_deref(), Some("Keep the snapshot durable."));
    let regards = backend.using::<Regard>("flotilla").list().await.expect("list dispatcher regards");
    assert!(
        regards.items.iter().all(|regard| regard.spec.target.name != "issue-732"),
        "--no-attach must leave the convoy outside the dispatching principal's searchlight"
    );
    let persisted_issue = persisted.spec.issues.first().expect("issue snapshot");
    assert_eq!(persisted_issue.reference, reference);
    assert_eq!(persisted_issue.repository_ref, Some(repository.key()));
    assert_eq!(persisted_issue.snapshot.title, issue.title);
    assert_eq!(persisted_issue.snapshot.body, issue.body);
    assert_eq!(utility.calls.load(Ordering::SeqCst), 0, "fully specified admission must not call AI");

    let project_read = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceGet { namespace: "flotilla".into(), kind: "projects".into(), name: "flotilla".into() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("get project");
    let CommandValue::ResourceRead(project_read) = project_read else { panic!("expected project read") };
    let project_object = project_read.records[0].object.as_ref().expect("project object");
    assert_eq!(project_object["spec"]["issue_source_bindings"].as_array().expect("explicit bindings").len(), 1);
    assert!(project_object["spec"].get("issue_sources").is_none());
    assert_eq!(project_object["resolvedIssueSources"].as_array().expect("resolved sources").len(), 2);
    assert!(project_object["resolvedIssueSources"]
        .as_array()
        .expect("resolved sources")
        .iter()
        .any(|source| { source["service"] == "https://github.com" && source["scope"] == "flotilla-org/planning" }));

    for (service, scope, name, expected_error) in [
        ("github.com", "flotilla-org/planning/", "host-issue", None),
        ("github.com", "flotilla-org/wrong", "wrong-scope", Some("available issue sources: https://github.com flotilla-org/flotilla")),
        ("http://github.com", "flotilla-org/planning", "wrong-scheme", Some("did you mean `https://github.com`?")),
    ] {
        let issue_ref = IssueRef { source: IssueSource { service: service.into(), scope: scope.into() }, id: reference.id.clone() };
        let command_id = daemon
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyStart {
                        intent: Box::new(ConvoyStartIntent {
                            namespace: None,
                            project_ref: "flotilla".into(),
                            change_request: None,
                            issues: vec![IssueSelector::Reference(issue_ref)],
                            name: Some(name.into()),
                            branch: Some(format!("fix/{name}")),
                            workflow_ref: Some("single-agent".into()),
                            inputs: Vec::new(),
                            instruction: None,
                            placement_policy: None,
                            needs: Vec::new(),
                            escalation_reason: None,
                            agent_overrides: Vec::new(),
                            skills: Vec::new(),
                            auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                        }),
                    })
                    .build(),
            )
            .await
            .expect("source-addressed start accepted");
        let result = recv_command_finished(&mut events, command_id).await;
        if let Some(expected_error) = expected_error {
            let CommandValue::Error { message } = result else { panic!("expected mismatch error: {result:?}") };
            assert!(message.contains(expected_error), "{message}");
            assert!(message.contains("https://github.com flotilla-org/planning"), "{message}");
        } else {
            assert_eq!(result, CommandValue::ConvoyStarted { name: format!("{name}@flotilla"), attach_plan: None, binding: None });
            assert_eq!(admitted_convoy(&backend, name).await.spec.issues[0].reference, reference);
        }
    }

    let bare_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: vec![IssueSelector::Id(reference.id.clone())],
                        name: Some("ambiguous-issue".into()),
                        branch: Some("fix/ambiguous-issue".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("bare issue command accepted");
    assert_eq!(recv_command_finished(&mut events, bare_id).await, CommandValue::Error {
        message: "issue 732 requires an alias because project flotilla has 2 issue sources".into()
    });

    let default_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("default-regard".into()),
                        branch: Some("fix/default-regard".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Default,
                    }),
                })
                .build(),
        )
        .await
        .expect("default start command accepted");
    assert_eq!(recv_command_finished(&mut events, default_id).await, CommandValue::ConvoyStarted {
        name: "default-regard@flotilla".into(),
        attach_plan: None,
        binding: None
    });
    let default_convoy = admitted_convoy(&backend, "default-regard").await;
    let regards = backend.using::<Regard>("flotilla").list().await.expect("list default dispatcher regard");
    let regard = regards
        .items
        .iter()
        .find(|regard| regard.spec.target.name == default_convoy.metadata.name)
        .expect("default implicit dispatcher regard");
    assert_eq!(regard.spec.principal_ref, default_convoy.spec.dispatching_principal_ref);
    assert_eq!(regard.spec.source, RegardSource::Implicit { policy: "convoy-dispatch".to_string() });
    assert_eq!(regard.spec.expiry, RegardExpiryPolicy::Decaying { expires_after_seconds: 300 });

    let batch_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: vec![
                            IssueSelector::Reference(reference.clone()),
                            IssueSelector::Reference(reference_two.clone()),
                            IssueSelector::Reference(reference_two.clone()),
                        ],
                        name: Some("batch-732-733".into()),
                        branch: Some("fix/batch-732-733".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: Some("Fix both issues in one convoy.".into()),
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("batch command accepted");
    let batch_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == batch_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("batch start should finish");
    assert_eq!(batch_result, CommandValue::ConvoyStarted { name: "batch-732-733@flotilla".into(), attach_plan: None, binding: None });
    let batch = admitted_convoy(&backend, "batch-732-733").await;
    assert_eq!(batch.spec.issues.iter().map(|issue| &issue.reference).collect::<Vec<_>>(), vec![&reference, &reference_two]);
    assert_eq!(batch.spec.instruction.as_deref(), Some("Fix both issues in one convoy."));
    let regards = backend.using::<Regard>("flotilla").list().await.expect("list batch dispatcher regards");
    assert!(
        regards.items.iter().all(|regard| regard.spec.target.name != "batch-732-733"),
        "batch --no-attach must not add a dispatch regard"
    );

    utility.fail.store(true, Ordering::SeqCst);
    let fallback_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: vec![IssueSelector::Reference(reference)],
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("offline fallback command accepted");
    let fallback_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == fallback_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("offline fallback should finish");
    assert_eq!(fallback_result, CommandValue::ConvoyStarted {
        name: "start-convoy-from-an-issue-732@flotilla".into(),
        attach_plan: None,
        binding: None,
    });
    let fallback = admitted_convoy(&backend, "start-convoy-from-an-issue-732").await;
    assert_eq!(fallback.spec.r#ref.as_deref(), Some("start-convoy-from-an-issue-732"));
    assert_eq!(utility.calls.load(Ordering::SeqCst), 1);

    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("explicit-workflow".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Explicit workflow".into(),
            default_workflow_ref: "missing-default".into(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project with unresolved default should persist");
    let empty_bindings_project = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryResourceGet {
                    namespace: "flotilla".into(),
                    kind: "projects".into(),
                    name: "explicit-workflow".into(),
                })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("get project with derived source");
    let CommandValue::ResourceRead(empty_bindings_project) = empty_bindings_project else { panic!("expected project read") };
    let project_object = empty_bindings_project.records[0].object.as_ref().expect("project object");
    assert!(project_object["spec"]["issue_source_bindings"].as_array().expect("explicit bindings").is_empty());
    assert_eq!(project_object["resolvedIssueSources"][0]["service"], "https://github.com");
    assert_eq!(project_object["resolvedIssueSources"][0]["scope"], "flotilla-org/planning");
    let explicit_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: Some("flotilla".into()),
                        project_ref: "explicit-workflow".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("explicit-workflow".into()),
                        branch: Some("fix/explicit-workflow".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("explicit workflow command accepted");
    let explicit_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == explicit_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("explicit workflow should not consult the missing default");
    assert_eq!(explicit_result, CommandValue::ConvoyStarted {
        name: "explicit-workflow@explicit-workflow".into(),
        attach_plan: None,
        binding: None,
    });

    let wrong_namespace_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: Some("other".into()),
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("wrong-namespace".into()),
                        branch: Some("fix/wrong-namespace".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("namespace rejection command accepted");
    let wrong_namespace_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == wrong_namespace_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("wrong namespace should fail visibly");
    assert!(matches!(wrong_namespace_result, CommandValue::Error { message } if message.contains("not served by this daemon")));
    assert!(matches!(
        backend.using::<ResourceConvoy>("flotilla").get("wrong-namespace").await,
        Err(flotilla_resources::ResourceError::NotFound { .. })
    ));

    let invalid_branch_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("invalid-branch".into()),
                        branch: Some("bad branch".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("invalid branch command accepted");
    let invalid_branch_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == invalid_branch_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("invalid branch should fail before persistence");
    assert!(matches!(invalid_branch_result, CommandValue::Error { message } if message.contains("valid git branch")));
    assert!(matches!(
        backend.using::<ResourceConvoy>("flotilla").get("invalid-branch").await,
        Err(flotilla_resources::ResourceError::NotFound { .. })
    ));

    drop(temp);
}

#[tokio::test]
async fn convoy_start_completes_both_names_with_one_ai_call() {
    let utility = Arc::new(CountingConvoyAiUtility { calls: AtomicUsize::new(0), fail: AtomicBool::new(false) });
    let mut discovery = fake_discovery(false);
    discovery.factories.ai_utilities.push(Box::new(CountingConvoyAiUtilityFactory { utility: Arc::clone(&utility) }));
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    let backend = daemon.resource_backend();
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository create");
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("single-agent".to_string()).build(), &single_agent_workflow_spec())
        .await
        .expect("workflow create");
    create_test_contained_policy(&backend, "flotilla-test", BTreeSet::from(["codex".to_string()])).await;
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("flotilla".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "Flotilla".into(),
            default_workflow_ref: "single-agent".into(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: repository.key(),
                alias: None,
                roles: Default::default(),
                subpath: None,
                default_branch: Some("main".into()),
            }],
        })
        .await
        .expect("project create");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: Some("Implement the admission snapshot.".into()),
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                    }),
                })
                .build(),
        )
        .await
        .expect("start command accepted");
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("start command should finish");

    assert_eq!(result, CommandValue::ConvoyStarted { name: "generated-convoy@flotilla".into(), attach_plan: None, binding: None });
    let persisted = admitted_convoy(&backend, "generated-convoy").await;
    assert_eq!(persisted.spec.r#ref.as_deref(), Some("fix/generated-convoy"));
    assert_eq!(utility.calls.load(Ordering::SeqCst), 1);
    drop(temp);
}

#[tokio::test]
async fn convoy_admission_generates_records_and_enforces_one_live_role_generation() {
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    let command = Command::builder()
        .action(CommandAction::ConvoyStart {
            intent: Box::new(
                ConvoyStartIntent::builder()
                    .project_ref("flotilla".to_string())
                    .name("governor".to_string())
                    .branch("governor".to_string())
                    .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                    .build(),
            ),
        })
        .build();
    let mut events = daemon.subscribe();

    let first_id = daemon.execute(command.clone()).await.expect("first admission");
    assert_eq!(recv_command_finished(&mut events, first_id).await, CommandValue::ConvoyStarted {
        name: "governor@flotilla".to_string(),
        attach_plan: None,
        binding: None,
    });
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first = convoys.list().await.expect("list first generation").items.pop().expect("first generation");
    assert!(first.metadata.name.starts_with("convoy-"));
    assert_ne!(first.metadata.name, "governor");
    assert_eq!(first.metadata.labels.get(flotilla_resources::PROJECT_LABEL).map(String::as_str), Some("flotilla"));
    assert_eq!(first.metadata.labels.get(flotilla_resources::ROLE_LABEL).map(String::as_str), Some("governor"));
    assert_eq!(first.metadata.labels.get(flotilla_resources::GENERATION_LABEL).map(String::as_str), Some("1"));

    let duplicate_id = daemon.execute(command.clone()).await.expect("duplicate admission result");
    assert_eq!(recv_command_finished(&mut events, duplicate_id).await, CommandValue::Error {
        message: "live convoy governor@flotilla generation 1 already exists".to_string(),
    });

    convoys
        .update_status(&first.metadata.name, &first.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("settle first generation");
    let second_id = daemon.execute(command).await.expect("second generation admission");
    assert!(
        matches!(recv_command_finished(&mut events, second_id).await, CommandValue::ConvoyStarted { name, .. } if name == "governor@flotilla")
    );
    let generations = convoys.list().await.expect("list generations").items;
    assert_eq!(generations.len(), 2, "terminal history must be retained");
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2));

    let delete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: None, name: "governor@flotilla".to_string(), force: true })
                .build(),
        )
        .await
        .expect("delete by role address");
    assert_eq!(recv_command_finished(&mut events, delete_id).await, CommandValue::Ok);
    assert_eq!(convoys.list().await.expect("list after addressed delete").items.len(), 1);

    let terminal_delete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: None, name: "governor@flotilla".to_string(), force: true })
                .build(),
        )
        .await
        .expect("delete sole terminal generation by role address");
    assert_eq!(recv_command_finished(&mut events, terminal_delete_id).await, CommandValue::Ok);
    assert!(convoys.list().await.expect("list after terminal generation delete").items.is_empty());
    drop(temp);
}

#[tokio::test]
async fn convoy_delete_reaps_a_landed_pre_identity_record_and_its_terminal_sessions() {
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("command-builder".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("legacy-workflow".to_string()).build(),
        )
        .await
        .expect("create pre-identity convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Landed,
            ..Default::default()
        })
        .await
        .expect("mark pre-identity convoy landed");

    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    sessions
        .create(
            &InputMeta::builder()
                .name("legacy-session".to_string())
                .labels(BTreeMap::from([(flotilla_resources::CONVOY_LABEL.to_string(), "command-builder".to_string())]))
                .build(),
            &TerminalSessionSpec::builder()
                .env_ref("legacy-environment".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Tool { command: "legacy-agent".to_string() })
                .cwd("/workspace".to_string())
                .pool("cleat".to_string())
                .build(),
        )
        .await
        .expect("create legacy terminal session");

    let mut events = daemon.subscribe();
    let delete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: None, name: "command-builder".to_string(), force: false })
                .build(),
        )
        .await
        .expect("delete command accepted");

    assert_eq!(recv_command_finished(&mut events, delete_id).await, CommandValue::Ok);
    assert!(matches!(convoys.get("command-builder").await, Err(ResourceError::NotFound { .. })));
    assert!(matches!(sessions.get("legacy-session").await, Err(ResourceError::NotFound { .. })));
    drop(temp);
}

#[tokio::test]
async fn landed_convoy_teardown_accepts_clean_squash_merge_after_branch_deletion() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let repo_ref = RepositoryKey("repo-a".to_string());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &InputMeta::builder().name("merged-deleted-branch".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("implement-review".to_string()).build(),
        )
        .await
        .expect("create convoy");
    let mut status = flotilla_resources::ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() };
    status.work.insert("work".to_string(), WorkState {
        phase: WorkPhase::Complete,
        placement: Some(flotilla_resources::PlacementStatus {
            fields: BTreeMap::from([(
                "checkout_refs".to_string(),
                serde_json::json!(BTreeMap::from([(repo_ref.clone(), "merged-checkout".to_string())])),
            )]),
        }),
        ..WorkState::builder().phase(WorkPhase::Complete).build()
    });
    let convoy = convoys.update_status("merged-deleted-branch", &convoy.metadata.resource_version, &status).await.expect("mark landed");
    let checkouts = backend.clone().using::<ResourceCheckout>("flotilla");
    let checkout = checkouts
        .create(
            &InputMeta::builder()
                .name("merged-checkout".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "merged-deleted-branch".to_string())]))
                .build(),
            &ResourceCheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "deleted-feature".to_string(),
                path: "/tmp/merged-checkout".to_string(),
                repo_ref,
                host_ref: "host-test".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("create checkout");
    let observed_at = chrono::Utc::now().to_rfc3339();
    let condition = |value| flotilla_resources::IntegrationCondition::builder().value(value).observed_at(observed_at.clone()).build();
    let integration = flotilla_resources::CheckoutIntegrationStatus {
        clean: condition(flotilla_resources::ConditionValue::True),
        pushed: condition(flotilla_resources::ConditionValue::False),
        landed: condition(flotilla_resources::ConditionValue::True),
        landed_evidence: Some(
            flotilla_resources::LandedEvidence::builder().change_request_id("127".to_string()).checkout_head_in_merged_head(true).build(),
        ),
        change_request: Some(
            flotilla_resources::ChangeRequestObservation::builder()
                .id("127".to_string())
                .state(flotilla_resources::ChangeRequestState::Merged)
                .mergeability(flotilla_resources::ChangeRequestMergeability::Unknown)
                .observed_at(observed_at)
                .build(),
        ),
        ..Default::default()
    };
    let checkout = checkouts
        .update_status("merged-checkout", &checkout.metadata.resource_version, &flotilla_resources::CheckoutStatus {
            integration,
            ..Default::default()
        })
        .await
        .expect("publish integration observation");

    daemon
        .verify_convoy_teardown_gate_for_checkouts(&convoy, std::slice::from_ref(&checkout), false)
        .await
        .expect("merged PR makes deleted branch safe to reclaim");

    let mut advanced = checkout.clone();
    advanced
        .status
        .as_mut()
        .expect("checkout status")
        .integration
        .landed_evidence
        .as_mut()
        .expect("landing evidence")
        .checkout_head_in_merged_head = false;
    assert!(daemon.verify_convoy_teardown_gate_for_checkouts(&convoy, &[advanced], false).await.is_err());

    let mut mismatched = checkout.clone();
    mismatched
        .status
        .as_mut()
        .expect("checkout status")
        .integration
        .landed_evidence
        .as_mut()
        .expect("landing evidence")
        .change_request_id = "different-pr".to_string();
    assert!(daemon.verify_convoy_teardown_gate_for_checkouts(&convoy, &[mismatched], false).await.is_err());

    let mut dirty = checkout;
    dirty.status.as_mut().expect("checkout status").integration.clean.value = flotilla_resources::ConditionValue::False;
    assert!(daemon.verify_convoy_teardown_gate_for_checkouts(&convoy, &[dirty], false).await.is_err());
}

#[tokio::test]
async fn landed_convoy_teardown_accepts_merged_produced_subject_when_checkout_status_is_missing() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let repo_ref = RepositoryKey("repo-a".to_string());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &InputMeta::builder().name("landed-missing-status".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder()
                .workflow_ref("implement-review".to_string())
                .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
                    .url("https://github.com/owner/repo".to_string())
                    .repo_ref(repo_ref.clone())
                    .source_ref("main".to_string())
                    .target_ref("main".to_string())
                    .workspace_slug("repo".to_string())
                    .subpaths(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("create convoy");
    let mut status = flotilla_resources::ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() };
    status.discover_subject(
        flotilla_protocol::Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".to_string(), scope: "owner/repo".to_string() },
            id: "42".to_string(),
        },
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Claim,
        chrono::Utc::now(),
    );
    status.work.insert("work".to_string(), WorkState {
        phase: WorkPhase::Complete,
        placement: Some(flotilla_resources::PlacementStatus {
            fields: BTreeMap::from([(
                "checkout_refs".to_string(),
                serde_json::json!(BTreeMap::from([(repo_ref.clone(), "missing-status".to_string())])),
            )]),
        }),
        ..WorkState::builder().phase(WorkPhase::Complete).build()
    });
    let convoy = convoys.update_status("landed-missing-status", &convoy.metadata.resource_version, &status).await.expect("mark landed");
    let checkout = backend
        .clone()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &InputMeta::builder()
                .name("missing-status".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "landed-missing-status".to_string())]))
                .build(),
            &ResourceCheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
                repo_ref,
                env_ref: "host-direct-test".to_string(),
                r#ref: "feature".to_string(),
                base_ref: Some("main".to_string()),
                target_path: temp.path().join("removed-worktree").display().to_string(),
                clone_ref: "clone-test".to_string(),
            }),
        )
        .await
        .expect("create checkout without status");
    let record_name = flotilla_resources::change_request_record_name("github.com", "owner/repo", 42);
    let changes = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record = changes
        .create(
            &InputMeta::builder().name(record_name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("owner/repo".to_string())
                .number(42)
                .observing_authority("host-test".to_string())
                .build(),
        )
        .await
        .expect("create change request");
    assert!(daemon.verify_convoy_teardown_gate_for_checkouts(&convoy, std::slice::from_ref(&checkout), false).await.is_err());
    let now = chrono::Utc::now();
    changes
        .update_status(&record_name, &record.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, now),
            head_sha: flotilla_resources::Observation::known("abc".to_string(), now),
            checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, now),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(false, now),
            },
            mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, now),
        })
        .await
        .expect("mark change request merged");

    daemon.verify_convoy_teardown_gate_for_checkouts(&convoy, &[checkout], false).await.expect("merged change request permits reclaim");
}

#[tokio::test]
async fn convoy_start_acknowledges_while_admission_is_in_flight() {
    let utility = Arc::new(SlowAiUtility::new());
    let discovery = slow_ai_discovery(Arc::clone(&utility));
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;

    let mut execution = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move {
            daemon
                .execute(
                    Command::builder()
                        .action(CommandAction::ConvoyStart {
                            intent: Box::new(ConvoyStartIntent {
                                namespace: None,
                                project_ref: "flotilla".into(),
                                change_request: None,
                                issues: Vec::new(),
                                name: None,
                                branch: None,
                                workflow_ref: None,
                                inputs: Vec::new(),
                                instruction: None,
                                placement_policy: None,
                                needs: Vec::new(),
                                escalation_reason: None,
                                agent_overrides: Vec::new(),
                                skills: Vec::new(),
                                auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
                            }),
                        })
                        .build(),
                )
                .await
        }
    });
    utility.wait_for_generation_start().await;

    let command_id = tokio::time::timeout(Duration::from_millis(100), &mut execution)
        .await
        .expect("convoy start should acknowledge while admission is still in flight")
        .expect("execute task should not panic")
        .expect("convoy start should be accepted");

    let mut events = daemon.subscribe();
    utility.release_generation();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("convoy start should finish after admission is released");
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }));

    drop(temp);
}

#[tokio::test]
async fn convoy_start_rejects_the_same_project_start_while_admission_is_in_flight() {
    let reference =
        IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }, id: "782".into() };
    let mut issue = TestIssue::new("Convoy start freezes the TUI").id("782").build();
    issue.reference = reference.clone();
    let provider = Arc::new(FakeIssueProvider::new());
    provider.add_issues(vec![(reference.id.clone(), issue)]).await;
    let utility = Arc::new(SlowAiUtility::new());
    let mut discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_tracker(provider as Arc<dyn flotilla_core::providers::issue_tracker::IssueProvider>),
    );
    discovery.factories.ai_utilities.push(Box::new(SlowAiUtilityFactory { utility: Arc::clone(&utility), probes: None }));
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, Some(reference.source.clone())).await;
    let command = Command::builder()
        .action(CommandAction::ConvoyStart {
            intent: Box::new(ConvoyStartIntent {
                namespace: None,
                project_ref: "flotilla".into(),
                change_request: None,
                issues: vec![IssueSelector::Reference(reference)],
                name: None,
                branch: None,
                workflow_ref: None,
                inputs: Vec::new(),
                instruction: Some("Implement issue 782".into()),
                placement_policy: None,
                needs: Vec::new(),
                escalation_reason: None,
                agent_overrides: Vec::new(),
                skills: Vec::new(),
                auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
            }),
        })
        .build();
    let mut events = daemon.subscribe();

    let first_id = daemon.execute(command.clone()).await.expect("first convoy start should be accepted");
    utility.wait_for_generation_start().await;
    let duplicate_id = daemon.execute(command).await.expect("duplicate command should receive an asynchronous result");
    let duplicate_result = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == duplicate_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("duplicate convoy start should finish without waiting for admission");
    assert!(matches!(duplicate_result, CommandValue::Error { message } if message.contains("already in progress")));

    utility.release_generation();
    let first_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == first_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("first convoy start should finish after admission is released");
    assert!(matches!(first_result, CommandValue::ConvoyStarted { .. }));

    drop(temp);
}

#[tokio::test]
async fn convoy_start_worker_panic_finishes_the_command_and_allows_retry() {
    let utility = Arc::new(PanicOnceAiUtility { panicked: AtomicBool::new(false) });
    let mut discovery = fake_discovery(false);
    discovery.factories.ai_utilities.push(Box::new(PanicOnceAiUtilityFactory { utility }));
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    create_test_convoy_project(&daemon.resource_backend(), None).await;
    let command = Command::builder()
        .action(CommandAction::ConvoyStart {
            intent: Box::new(
                ConvoyStartIntent::builder()
                    .project_ref("flotilla".to_string())
                    .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                    .build(),
            ),
        })
        .build();
    let mut events = daemon.subscribe();

    let first_id = daemon.execute(command.clone()).await.expect("first convoy start should be accepted");
    let first_result = recv_command_finished(&mut events, first_id).await;
    assert!(matches!(first_result, CommandValue::Error { message } if message.contains("worker panicked")));

    let retry_id = daemon.execute(command).await.expect("matching convoy start retry should be accepted");
    let retry_result = recv_command_finished(&mut events, retry_id).await;
    assert_eq!(retry_result, CommandValue::ConvoyStarted { name: "retried-convoy@flotilla".into(), attach_plan: None, binding: None });

    drop(temp);
}

#[tokio::test]
async fn convoy_start_reports_failed_work_without_waiting_for_auto_attach_timeout() {
    let (temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(fake_discovery(false)).await;
    let backend = daemon.resource_backend();
    create_test_convoy_project(&backend, None).await;
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        change_request: None,
                        issues: Vec::new(),
                        name: Some("bootstrap-failure".into()),
                        branch: Some("fix/bootstrap-failure".into()),
                        workflow_ref: Some("single-agent".into()),
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        needs: Vec::new(),
                        escalation_reason: None,
                        agent_overrides: Vec::new(),
                        skills: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Always,
                    }),
                })
                .build(),
        )
        .await
        .expect("convoy start should be accepted");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let record_name = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let selector = BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_string(), "bootstrap-failure".to_string())]);
            if let Some(convoy) = convoys.list_matching_labels(&selector).await.expect("list bootstrap convoy").items.into_iter().next() {
                break convoy.metadata.name;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("convoy should be persisted");

    let workflow = single_agent_workflow_spec();
    apply_status_patch(
        &convoys,
        &record_name,
        &convoy_controller_patches::bootstrap(
            WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: workflow.turn_delivery,
                vessels: workflow.vessels,
            },
            "single-agent".into(),
            BTreeMap::new(),
            BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Pending).build())]),
            BTreeMap::new(),
            ConvoyPhase::Pending,
            None,
        ),
    )
    .await
    .expect("convoy bootstrap patch");

    let work_message = "agent adapter claude cannot realize contained stance";
    apply_status_patch(
        &convoys,
        &record_name,
        &convoy_controller_patches::roll_up_work("work".into(), WorkPhase::Failed, chrono::Utc::now(), Some(work_message.into())),
    )
    .await
    .expect("work failure patch");
    apply_status_patch(
        &convoys,
        &record_name,
        &convoy_controller_patches::fail_convoy(BTreeMap::new(), chrono::Utc::now(), Some("convoy bootstrap failed".into())),
    )
    .await
    .expect("convoy failure patch");

    let result = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(error) => panic!("command event receive failed: {error:?}"),
            }
        }
    })
    .await
    .expect("failed convoy should stop auto-attach promptly");
    assert!(matches!(result, CommandValue::Error { message } if message.contains(work_message)));

    drop(temp);
}

fn static_ssh_test_discovery(runner: Arc<dyn CommandRunner>) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    runtime.runner = runner;
    runtime.env = Arc::new(TestEnvVars::default());
    runtime.host_detectors = vec![Box::new(RunnerEchoHostDetector { probe: "REMOTE_MARKER", assertion_key: "REMOTE_MARKER" })];
    runtime
}

fn static_ssh_test_discovery_with_env_and_detectors(
    runner: Arc<dyn CommandRunner>,
    env: Arc<dyn flotilla_core::providers::discovery::EnvVars>,
    host_detectors: Vec<Box<dyn HostDetector>>,
) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    runtime.runner = runner;
    runtime.env = env;
    runtime.host_detectors = host_detectors;
    runtime
}

fn write_static_environment_config(config_dir: &Path, contents: &str) {
    std::fs::create_dir_all(config_dir).expect("create config dir");
    let rendered = if contents.contains("machine_id") { contents.to_owned() } else { format!("machine_id = \"test-machine\"\n{contents}") };
    std::fs::write(config_dir.join("daemon.toml"), rendered).expect("write daemon config");
}

async fn daemon_for_plain_dir_with_local_environment_id(local_environment_id: &str) -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config_dir = temp.path().join("config");
    let config = test_config_store(config_dir.clone());
    let discovery = fake_discovery(false);
    let machine_state_dir =
        flotilla_core::host_identity::resolve_local_environment_state_dir(&config_dir, Some("test-machine"), &*discovery.runner).await;
    std::fs::create_dir_all(&machine_state_dir).expect("create machine-scoped state dir");
    std::fs::write(machine_state_dir.join("environment-id"), format!("{local_environment_id}\n")).expect("seed environment id");
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    (temp, repo, daemon)
}

fn checkout_state_for_repo(repo: &Path, branch: &str) -> Arc<std::sync::RwLock<FakeVcsState>> {
    FakeVcsState::builder(repo.to_path_buf())
        .checkout_raw(repo.join(branch), Checkout {
            branch: branch.into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            host_name: None,
            environment_id: None,
        })
        .build()
}

#[tokio::test]
async fn configured_static_ssh_environments_are_registered_with_environment_scoped_bags() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("git", &["--version"], Ok("git version 2.43.0".into()))
            .on_run("env", &[], Ok(String::new()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'env'",
                ],
                Ok("XDG_STATE_HOME=/var/state\nTERM=screen-256color\nCOLORTERM=truecolor\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'cat' '/var/state/flotilla/environment-id'",
                ],
                Ok("buildbox-env-id\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'cat' '/var/state/flotilla/host-id'",
                ],
                Ok("buildbox-host-id\n".into()),
            )
            .build(),
    );

    let mut discovery = fake_discovery(false);
    discovery.runner = ssh_runner;
    discovery.host_detectors = vec![
        Box::new(flotilla_core::providers::discovery::detectors::generic::CommandDetector::new(
            "git",
            &["--version"],
            flotilla_core::providers::discovery::detectors::generic::parse_first_dotted_version,
        )),
        Box::new(flotilla_core::providers::discovery::detectors::generic::EnvVarDetector::new("TERM")),
        Box::new(flotilla_core::providers::discovery::detectors::generic::EnvVarDetector::new("COLORTERM")),
    ];
    let daemon = InProcessDaemon::new(vec![repo], Arc::new(ConfigStore::with_base(config_dir)), discovery, HostName::local()).await;

    let remote_env_id = EnvironmentId::new("buildbox-env-id");
    let managed_ids = daemon.managed_environment_ids_for_test();
    assert!(managed_ids.contains(daemon.local_environment_id()));
    assert!(managed_ids.contains(&remote_env_id));

    let local_bag = daemon.environment_bag_for_test(daemon.local_environment_id()).expect("local bag");
    assert_eq!(local_bag.find_env_var("TERM"), None);

    let remote_bag = daemon.environment_bag_for_test(&remote_env_id).expect("remote bag");
    assert_eq!(remote_bag.find_env_var("TERM"), Some("screen-256color"));
    assert_eq!(remote_bag.find_env_var("COLORTERM"), Some("truecolor"));
}

#[tokio::test]
async fn static_ssh_environment_display_name_is_visible_without_detector_support() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"
display_name = "Build Box"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("git", &["--version"], Ok("git version 2.43.0".into()))
            .on_run("env", &[], Ok(String::new()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'env'",
                ],
                Ok("HOME=/home/build\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'cat' '/home/build/.local/state/flotilla/environment-id'",
                ],
                Ok("buildbox-visible-id\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'cat' '/home/build/.local/state/flotilla/host-id'",
                ],
                Ok("buildbox-visible-host-id\n".into()),
            )
            .build(),
    );

    let daemon = InProcessDaemon::new(
        vec![repo],
        Arc::new(ConfigStore::with_base(config_dir)),
        static_ssh_test_discovery(ssh_runner),
        HostName::local(),
    )
    .await;

    let local_environment_id = daemon.local_host_summary().await.environment_id;
    let status = daemon.get_host_status_internal(&local_environment_id).await.expect("host status");
    let visible = status
        .visible_environments
        .iter()
        .find_map(|environment| match environment {
            EnvironmentInfo::Direct { id, host_id, display_name, .. }
                if id == &EnvironmentId::host(HostId::new("buildbox-visible-host-id"))
                    && host_id.as_ref().map(HostId::as_str) == Some("buildbox-visible-host-id") =>
            {
                Some(display_name.clone())
            }
            _ => None,
        })
        .expect("static ssh direct environment should be visible");

    assert_eq!(visible.as_deref(), Some("Build Box"));
}

#[tokio::test]
async fn broken_static_ssh_environment_does_not_break_local_startup() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"

[environments.brokenbox]
hostname = "brokenbox.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("probe-env", &["REMOTE_MARKER"], Ok("local".into()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'probe-env' 'REMOTE_MARKER'",
                ],
                Ok("buildbox".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "brokenbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Err("ssh failed".into()),
            )
            .build(),
    );

    let daemon = InProcessDaemon::new(
        vec![repo.clone()],
        Arc::new(ConfigStore::with_base(config_dir)),
        static_ssh_test_discovery(ssh_runner),
        HostName::local(),
    )
    .await;

    assert!(daemon.tracked_repo_identity_for_path(&repo).await.is_some(), "repo should still be tracked");

    let managed_ids = daemon.managed_environment_ids_for_test();
    assert!(managed_ids.contains(daemon.local_environment_id()));
    assert!(managed_ids.contains(&EnvironmentId::new("static-ssh-6275696c64626f78")));
    assert!(!managed_ids.contains(&EnvironmentId::new("static-ssh-62726f6b656e626f78")));
}

#[tokio::test]
async fn static_ssh_environment_detection_does_not_reuse_local_env_vars() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("probe-env", &["REMOTE_MARKER"], Ok("local".into()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .build(),
    );

    let daemon = InProcessDaemon::new(
        vec![repo],
        Arc::new(ConfigStore::with_base(config_dir)),
        static_ssh_test_discovery_with_env_and_detectors(
            ssh_runner,
            Arc::new(TestEnvVars::new([("LOCAL_ONLY_SECRET", "secret-value")])),
            vec![Box::new(EnvVarEchoHostDetector { env_var: "LOCAL_ONLY_SECRET", assertion_key: "LOCAL_ONLY_SECRET" })],
        ),
        HostName::local(),
    )
    .await;

    let local_bag = daemon.environment_bag_for_test(daemon.local_environment_id()).expect("local bag");
    assert_eq!(local_bag.find_env_var("LOCAL_ONLY_SECRET"), Some("secret-value"));

    let remote_bag = daemon.environment_bag_for_test(&EnvironmentId::new("static-ssh-6275696c64626f78")).expect("remote bag");
    assert_eq!(remote_bag.find_env_var("LOCAL_ONLY_SECRET"), None);
}

#[tokio::test]
async fn selected_static_ssh_repo_discovery_does_not_treat_local_git_checkout_as_remote_checkout() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    init_git_repo(&repo);

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("git", &["--version"], Ok("git version 2.43.0".into()))
            .on_run("env", &[], Ok("TERM=xterm-256color\n".into()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'env'",
                ],
                Ok("TERM=xterm-256color\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'rev-parse' '--is-inside-work-tree'", repo.display()).as_str(),
                ],
                Err("fatal: not a git repository".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'rev-parse' '--abbrev-ref' '@{{upstream}}'", repo.display()).as_str(),
                ],
                Err("fatal: not a git repository".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'remote'", repo.display()).as_str(),
                ],
                Err("fatal: not a git repository".into()),
            )
            .build(),
    );

    let mut discovery = fake_discovery(false);
    discovery.runner = ssh_runner;
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(config_dir)), discovery, HostName::local()).await;

    let result = daemon
        .discover_repo_for_environment_for_test(&repo, &EnvironmentId::new("static-ssh-6275696c64626f78"))
        .await
        .expect("discover repo in remote direct environment");

    assert!(result.repo_bag.find_vcs_checkout(flotilla_core::providers::discovery::VcsKind::Git).is_none());
    assert!(
        result.registry.provider_infos().iter().all(|(category, name)| { !(category == ProviderCategory::Vcs.slug() && name == "Git") }),
        "remote discovery should not activate git from the daemon-local checkout path"
    );
}

#[tokio::test]
async fn static_ssh_registration_times_out_hung_hosts_and_keeps_startup_moving() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"

[environments.hangbox]
hostname = "hangbox.example"
"#,
    );

    let daemon = tokio::time::timeout(
        Duration::from_secs(7),
        InProcessDaemon::new(
            vec![repo],
            Arc::new(ConfigStore::with_base(config_dir)),
            static_ssh_test_discovery(Arc::new(HangingSshRunner { delay: Duration::from_secs(6) })),
            HostName::local(),
        ),
    )
    .await
    .expect("daemon startup should not hang indefinitely");

    let managed_ids = daemon.managed_environment_ids_for_test();
    assert!(managed_ids.contains(&EnvironmentId::new("static-ssh-6275696c64626f78")));
    assert!(!managed_ids.contains(&EnvironmentId::new("static-ssh-68616e67626f78")));
}

#[tokio::test]
async fn temporary_static_ssh_environment_ids_are_injective_for_distinct_config_keys() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments."build box"]
hostname = "buildbox.example"

[environments."build-box"]
hostname = "builddash.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("probe-env", &["REMOTE_MARKER"], Ok("local".into()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'probe-env' 'REMOTE_MARKER'",
                ],
                Ok("box".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "builddash.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "builddash.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'probe-env' 'REMOTE_MARKER'",
                ],
                Ok("dash".into()),
            )
            .build(),
    );

    let daemon = InProcessDaemon::new(
        vec![repo],
        Arc::new(ConfigStore::with_base(config_dir)),
        static_ssh_test_discovery(ssh_runner),
        HostName::local(),
    )
    .await;

    let managed_ids = daemon.managed_environment_ids_for_test();
    assert!(managed_ids.contains(&EnvironmentId::new("static-ssh-6275696c6420626f78")));
    assert!(managed_ids.contains(&EnvironmentId::new("static-ssh-6275696c642d626f78")));
}

fn init_bare_git_remote(path: &Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(path)
        .status()
        .expect("run git init --bare");
    assert!(status.success(), "git init --bare should succeed");
}

fn init_git_repo_with_local_bare_remote(path: &Path, remote_path: &Path) -> RepoIdentity {
    init_bare_git_remote(remote_path);
    init_git_repo_with_remote(path, remote_path.to_str().expect("remote path utf8"))
}

async fn daemon_for_fake_repo() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>, RepoIdentity) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let state =
        FakeVcsState::builder(repo.clone()).branch("main", true).remote_branch("main").checkout("main").is_main(true).build().build();

    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".into()))).await;
    daemon.add_repo(&repo).await.expect("adopt test checkout into resources");
    let identity = daemon.tracked_repo_identity_for_path(&repo).await.expect("identity");
    (temp, repo, daemon, identity)
}

#[tokio::test]
async fn refresh_preserves_repository_spec_fork_stance_and_explicit_removal() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".to_string()))).await;
    let repository = RepositorySpec::remote("https://github.com/owner/repo").expect("repository spec");
    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("materialize repository");
    let stored = repositories.get(&repository.key().to_string()).await.expect("stored repository");
    let fork = stored.spec.clone().with_upstream("https://github.com/upstream/repo", RepositoryRelation::Fork).expect("fork stance");
    repositories.update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, &fork).await.expect("set fork stance");

    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh fork repository");
    let stored = repositories.get(&repository.key().to_string()).await.expect("stored repository");
    assert!(stored.spec.is_fork(), "refresh should preserve replicated fork provenance");

    repositories
        .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, &repository)
        .await
        .expect("remove fork stance");
    daemon.refresh(&RepoSelector::Path(repo)).await.expect("refresh repository after explicit removal");

    let stored = repositories.get(&repository.key().to_string()).await.expect("stored repository");
    assert!(stored.spec.upstream().is_none(), "refresh should preserve explicit fork provenance removal");
}

#[tokio::test]
async fn strict_refresh_surfaces_repository_inspection_failures() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    daemon
        .set_repository_inspector(Arc::new(TestRepositoryInspector {
            host_ref: daemon.local_host_id().expect("local Host").to_string(),
            repository: Arc::new(std::sync::RwLock::new("repo".to_string())),
            fixed_repository_by_path: HashMap::new(),
            continuity: true,
            inspection_failure: Some("repository metadata unavailable".to_string()),
        }))
        .await;

    assert_eq!(daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("best-effort refresh"), None);
    let error = daemon.refresh_strict(&RepoSelector::Path(repo.clone())).await.expect_err("strict refresh must fail");
    assert_eq!(error, format!("inspect repository {} during refresh: repository metadata unavailable", repo.display()));
}

async fn daemon_for_duplicate_fake_repos() -> (tempfile::TempDir, PathBuf, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).expect("create repo-a dir");
    std::fs::create_dir_all(&repo_b).expect("create repo-b dir");

    let state_a = FakeVcsState::builder(repo_a.clone()).branch("main", true).checkout("main").is_main(true).build().build();
    let state_b = FakeVcsState::builder(repo_b.clone()).branch("main", true).checkout("main").is_main(true).build().build();

    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![Box::new(FakeVcsFactory::new(state_a)), Box::new(FakeVcsFactory::new(state_b))];
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![repo_a.clone(), repo_b.clone()], config, discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".to_string()))).await;
    (temp, repo_a, repo_b, daemon)
}

#[tokio::test]
async fn list_hosts_does_not_materialize_configured_peers_without_host_environment_identity() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon.set_configured_peer_names(vec![HostName::new("remote")]).await;

    let hosts = daemon.list_hosts_internal().await.expect("list hosts");

    assert!(hosts
        .hosts
        .iter()
        .any(|entry| { entry.node.as_ref().is_some_and(|node| node.node_id == daemon.node_id().clone()) && entry.is_local }));
    assert!(!hosts.hosts.iter().any(|entry| entry.node == Some(test_node("remote"))));
}

#[tokio::test]
async fn get_host_providers_returns_local_summary_and_unmapped_remote_host_is_absent() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon.set_configured_peer_names(vec![HostName::new("remote")]).await;
    daemon.publish_peer_connection_status(&HostName::new("remote"), PeerConnectionState::Connected).await;

    let local_environment_id = daemon.local_host_summary().await.environment_id;
    let local = daemon.get_host_providers_internal(&local_environment_id).await.expect("local host providers should resolve");
    assert_eq!(local.node.node_id, daemon.node_id().clone());
    assert_eq!(local.node.display_name, daemon.host_name().as_str());
    assert_eq!(summary_host(&local.summary), *daemon.host_name());

    assert!(!daemon
        .list_hosts_internal()
        .await
        .expect("list hosts")
        .hosts
        .into_iter()
        .any(|entry| entry.node.as_ref().is_some_and(|node| node.node_id == test_node("remote").node_id)));
}

#[tokio::test]
async fn get_repo_providers_uses_preferred_root_environment_host_discovery_for_local_repo() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::env_var("LOCAL_MARKER", "local")))
        .expect("replace local environment bag");

    let providers = daemon.get_repo_providers_internal(&RepoSelector::Path(repo)).await.expect("repo providers should resolve");

    assert!(
        providers
            .host_discovery
            .iter()
            .any(|entry| entry.kind == "env_var_set" && entry.detail.get("key").map(String::as_str) == Some("LOCAL_MARKER")),
        "host discovery should report the preferred local environment bag"
    );
}

#[tokio::test]
async fn local_host_queries_include_visible_environments_without_changing_summary_environments() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    let direct_environment_id = EnvironmentId::new("direct-visible-env");
    let direct_host_id = HostId::new("direct-visible-host");
    daemon
        .register_direct_environment_for_test(
            direct_environment_id.clone(),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("DISPLAY_NAME", "direct-visible")),
            Some(direct_host_id.clone()),
        )
        .expect("register direct environment");

    let provisioned_environment_id = EnvironmentId::new("provisioned-visible-env");
    let provisioned_handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
        id: provisioned_environment_id.clone(),
        image: ImageId::new("mock:image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
    });
    daemon
        .register_provisioned_environment_for_test(
            provisioned_environment_id.clone(),
            provisioned_handle,
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("DISPLAY_NAME", "provisioned-visible")),
        )
        .expect("register provisioned environment");

    let local_environment_id = daemon.local_host_summary().await.environment_id;
    let status = daemon.get_host_status_internal(&local_environment_id).await.expect("host status");
    let providers = daemon.get_host_providers_internal(&local_environment_id).await.expect("host providers");

    let status_ids: Vec<_> = status
        .visible_environments
        .iter()
        .map(|environment| match environment {
            EnvironmentInfo::Direct { id, .. } | EnvironmentInfo::Provisioned { id, .. } => id.clone(),
        })
        .collect();
    let provider_ids: Vec<_> = providers
        .visible_environments
        .iter()
        .map(|environment| match environment {
            EnvironmentInfo::Direct { id, .. } | EnvironmentInfo::Provisioned { id, .. } => id.clone(),
        })
        .collect();

    let local_host_id = daemon.local_host_id().expect("local host id");
    assert!(status_ids.contains(&EnvironmentId::host(local_host_id.clone())));
    assert!(status_ids.contains(&EnvironmentId::host(direct_host_id.clone())));
    assert!(status_ids.contains(&provisioned_environment_id));
    assert_eq!(status_ids, provider_ids, "host status and provider queries should expose the same visible environments");

    let direct_visible = status
        .visible_environments
        .iter()
        .find(|environment| matches!(environment, EnvironmentInfo::Direct { id, .. } if id == &EnvironmentId::host(direct_host_id.clone())))
        .expect("direct visible environment should be present");
    match direct_visible {
        EnvironmentInfo::Direct { host_id, .. } => assert_eq!(host_id.as_ref(), Some(&direct_host_id)),
        _ => unreachable!("already filtered to direct environment"),
    }

    let summary = status.summary.expect("local host summary");
    assert!(
        summary.environments.iter().all(|environment| matches!(environment, EnvironmentInfo::Provisioned { .. })),
        "host summary environments must remain provisioned-only"
    );
    assert!(summary.environments.iter().any(|environment| match environment {
        EnvironmentInfo::Provisioned { id, .. } => id == &provisioned_environment_id,
        _ => false,
    }));
    assert!(
        summary.environments.iter().all(|environment| match environment {
            EnvironmentInfo::Direct { id, .. } => id != &EnvironmentId::host(direct_host_id.clone()),
            EnvironmentInfo::Provisioned { .. } => true,
        }),
        "direct environments must not leak into HostSummary.environments"
    );
}

#[tokio::test]
async fn get_topology_includes_configured_but_disconnected_peers() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    // Configure two peers but only set routes for one
    daemon.set_configured_peer_names(vec![HostName::new("connected"), HostName::new("unreachable")]).await;
    daemon
        .set_topology_routes(vec![TopologyRoute {
            target: test_node("connected"),
            next_hop: test_node("connected"),
            direct: true,
            connected: true,
            fallbacks: vec![],
            last_attempt: None,
            last_error: None,
        }])
        .await;

    let topology = daemon.get_topology().await.expect("topology");

    // Should have entries for both peers
    assert_eq!(topology.routes.len(), 2, "should include both connected and disconnected peers");

    let connected = topology.routes.iter().find(|r| r.target == test_node("connected")).expect("connected peer");
    assert!(connected.connected);
    assert!(connected.direct);

    let unreachable = topology.routes.iter().find(|r| r.target == test_node("unreachable")).expect("unreachable peer");
    assert!(!unreachable.connected, "configured-but-never-connected peer should show as disconnected");
    assert!(unreachable.direct, "disconnected peer should show as direct (no relay known)");
    assert!(unreachable.fallbacks.is_empty());
}

#[tokio::test]
async fn daemon_uses_persisted_local_environment_id() {
    let (temp, repo, daemon) = daemon_for_plain_dir_with_local_environment_id("test-local-environment-id").await;

    assert_eq!(daemon.local_environment_id().as_str(), "test-local-environment-id");

    drop(daemon);

    let restarted =
        InProcessDaemon::new(vec![repo], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    assert_eq!(restarted.local_environment_id().as_str(), "test-local-environment-id");
}

#[tokio::test]
async fn daemon_restart_preserves_standing_convoy_and_terminal_session() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config = test_config_store(temp.path().join("config"));
    let database_path = temp.path().join("resources.sqlite");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&database_path).expect("open resource store"));
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery(false),
        HostName::local(),
        backend.clone(),
    )
    .await;

    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    convoys
        .create(
            &InputMeta::builder().name("standing-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("standing".to_string()).build(),
        )
        .await
        .expect("create standing convoy");
    apply_status_patch(&convoys, "standing-convoy", &flotilla_resources::ConvoyStatusPatch::RollUpPhase {
        phase: ConvoyPhase::Active,
        started_at: Some(chrono::Utc::now()),
        finished_at: None,
    })
    .await
    .expect("mark convoy active");

    let terminals = backend.clone().using::<TerminalSession>("flotilla");
    terminals
        .create(
            &InputMeta::builder().name("standing-cleat-session".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("host-direct-test".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Tool { command: "cleat attach standing".to_string() })
                .cwd("/workspace".to_string())
                .pool("cleat".to_string())
                .build(),
        )
        .await
        .expect("create terminal session");
    apply_status_patch(&terminals, "standing-cleat-session", &TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-standing".to_string(),
        pid: None,
        started_at: chrono::Utc::now(),
        crew: None,
        launch_command: "cleat attach standing".to_string(),
        delivered_message_id: None,
    })
    .await
    .expect("mark terminal running");

    drop(daemon);
    drop(convoys);
    drop(terminals);
    drop(backend);

    let restarted_backend = ResourceBackend::Sqlite(SqliteBackend::open(&database_path).expect("reopen resource store"));
    let _restarted =
        InProcessDaemon::new_with_resource_backend(Vec::new(), config, fake_discovery(false), HostName::local(), restarted_backend.clone())
            .await;

    let convoy =
        restarted_backend.clone().using::<ResourceConvoy>("flotilla").get("standing-convoy").await.expect("standing convoy after restart");
    assert_eq!(convoy.status.expect("convoy status").phase, ConvoyPhase::Active);
    let terminal =
        restarted_backend.using::<TerminalSession>("flotilla").get("standing-cleat-session").await.expect("terminal session after restart");
    assert_eq!(terminal.status.expect("terminal status").phase, TerminalSessionPhase::Running);
}

#[tokio::test]
async fn daemon_uses_config_machine_id_for_local_node_identity_storage() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(config_dir.join("daemon.toml"), "machine_id = \"override-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![repo], Arc::new(ConfigStore::with_base(&config_dir)), fake_discovery(false), HostName::local()).await;

    assert!(
        config_dir.join("identity/override-machine/node.key").exists(),
        "daemon should use configured machine id for node identity storage"
    );
    assert!(
        config_dir.join("identity/override-machine/node.pub").exists(),
        "daemon should persist the public key alongside the private key"
    );
    assert_eq!(daemon.node_id().as_str().len(), 32);
}

#[tokio::test]
async fn daemon_uses_persisted_fingerprint_backed_node_id() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("create config dir");

    let discovery = fake_discovery(false);
    let daemon =
        InProcessDaemon::new(vec![repo.clone()], test_config_store(config_dir.clone()), discovery, HostName::new("display-host")).await;

    let first_node_id = daemon.node_id().clone();
    assert_eq!(first_node_id.as_str().len(), 32, "node id should be a 16-byte hex fingerprint");
    assert_ne!(first_node_id.as_str(), "display-host", "display name must remain separate from node identity");

    drop(daemon);

    let restarted = InProcessDaemon::new(
        vec![repo],
        test_config_store(config_dir.clone()),
        fake_discovery(false),
        HostName::new("renamed-display-host"),
    )
    .await;

    assert_eq!(*restarted.node_id(), first_node_id, "restarting should reuse the persisted node keypair");
    assert_eq!(restarted.host_name().as_str(), "renamed-display-host", "display name should not affect node identity");
    let identity_dir = config_dir.join("identity");
    assert!(identity_dir.exists(), "node identity storage should live under the config identity dir");
}

#[tokio::test]
async fn adopted_checkout_reconciliation_repairs_partial_durable_creation() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let durable = daemon.resource_backend().using::<ResourceCheckout>("flotilla");
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>("flotilla");
    let spec = ResourceCheckoutSpec::Observed(
        ObservedCheckoutSpec::builder()
            .r#ref("feature/reconcile".to_string())
            .path("/work/reconcile".to_string())
            .repo_ref(flotilla_resources::RepositoryKey("widgets-api".to_string()))
            .host_ref("host-01".to_string())
            .is_main(false)
            .build(),
    );
    durable
        .create(
            &InputMeta::builder()
                .name("adopted-checkout-reconcile".to_string())
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Adopted),
            &spec,
        )
        .await
        .expect("durable checkout create should succeed");

    daemon.reconcile_adopted_checkouts("flotilla").await.expect("adopted checkout reconciliation should succeed");

    let durable_checkout = durable.get("adopted-checkout-reconcile").await.expect("durable checkout should remain");
    let durable_status = durable_checkout.status.as_ref().expect("repaired durable status");
    assert_eq!(durable_status.phase, ResourceCheckoutPhase::Ready);
    assert_eq!(durable_status.path.as_deref(), Some("/work/reconcile"));
    let observed_checkout = observed.get("adopted-checkout-reconcile").await.expect("observed checkout should be projected");
    assert_eq!(observed_checkout.spec, durable_checkout.spec);
    assert_eq!(observed_checkout.status, durable_checkout.status);
    assert_eq!(observed_checkout.metadata.labels, durable_checkout.metadata.labels);
}

#[tokio::test]
async fn adopted_checkout_reconciliation_isolates_an_observed_name_collision() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let daemon =
        InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let durable = daemon.resource_backend().using::<ResourceCheckout>("flotilla");
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>("flotilla");
    let checkout_spec = |path: &str| {
        ResourceCheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("feature/reconcile".to_string())
                .path(path.to_string())
                .repo_ref(flotilla_resources::RepositoryKey("widgets-api".to_string()))
                .host_ref("host-01".to_string())
                .is_main(false)
                .build(),
        )
    };
    for (name, path) in [("a-collision", "/work/collision"), ("z-valid", "/work/valid")] {
        durable
            .create(
                &InputMeta::builder().name(name.to_string()).build().with_lifecycle_authority(LifecycleAuthority::Adopted),
                &checkout_spec(path),
            )
            .await
            .expect("durable checkout create should succeed");
    }
    observed
        .create(
            &InputMeta::builder().name("a-collision".to_string()).build().with_lifecycle_authority(LifecycleAuthority::Observed),
            &checkout_spec("/work/unrelated"),
        )
        .await
        .expect("unrelated observed checkout should be created");

    let error = daemon.reconcile_adopted_checkouts("flotilla").await.expect_err("the name collision should be reported");

    assert!(error.contains("a-collision"), "{error}");
    let collision = observed.get("a-collision").await.expect("colliding observation should remain");
    assert_eq!(collision.metadata.lifecycle_authority().expect("authority should parse"), Some(LifecycleAuthority::Observed));
    let valid = observed.get("z-valid").await.expect("other durable adopted checkouts should still be projected");
    assert_eq!(valid.metadata.lifecycle_authority().expect("authority should parse"), Some(LifecycleAuthority::Adopted));
}

#[tokio::test]
async fn tracking_does_not_materialize_when_project_name_is_occupied() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let state = FakeVcsState::builder(repo.clone()).branch("main", true).checkout("main").is_main(true).path(&repo).build().build();
    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let daemon = InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".to_string()))).await;

    let tracked = RepositorySpec::remote("https://github.com/owner/repo").expect("tracked repository spec");
    let other = RepositorySpec::remote("https://github.com/owner/other").expect("other repository spec");
    let projects = daemon.resource_backend().using::<Project>("flotilla");
    projects
        .create(&InputMeta::builder().name("repo".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "repo suite".to_string(),
            default_workflow_ref: "single-agent".to_string(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![
                ProjectRepositorySpec { repo: tracked.key(), alias: None, roles: Default::default(), subpath: None, default_branch: None },
                ProjectRepositorySpec { repo: other.key(), alias: None, roles: Default::default(), subpath: None, default_branch: None },
            ],
        })
        .await
        .expect("generated-name occupant should be creatable");

    daemon.add_repo(&repo).await.expect("tracked repository should be added");

    let materialized = projects.list().await.expect("project list should succeed");
    assert_eq!(materialized.items.len(), 1);
    let occupant = materialized.items.iter().find(|project| project.metadata.name == "repo").expect("collision occupant should remain");
    assert_eq!(occupant.spec.repositories.len(), 2);
}

#[tokio::test]
async fn repository_identity_change_does_not_materialize_project_when_superseded_repository_is_missing() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let state = FakeVcsState::builder(repo.clone()).branch("main", true).checkout("main").is_main(true).path(&repo).build().build();
    let discovered_repo = Arc::new(std::sync::RwLock::new("old-repo".to_string()));
    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(MutableRemoteHostDetector { owner: "owner", repo: Arc::clone(&discovered_repo) }));

    let daemon = InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::clone(&discovered_repo)).await;
    daemon.add_repo(&repo).await.expect("add repository with old identity");

    let old_key = RepositorySpec::remote("https://github.com/owner/old-repo").expect("old repository spec").key();
    let new_key = RepositorySpec::remote("https://github.com/owner/new-repo").expect("new repository spec").key();
    let durable = daemon.resource_backend().using::<ResourceCheckout>("flotilla");
    durable
        .create(
            &InputMeta::builder().name("durable-old-checkout".to_string()).build(),
            &ResourceCheckoutSpec::Observed(
                ObservedCheckoutSpec::builder()
                    .r#ref("main".to_string())
                    .path(repo.to_string_lossy().into_owned())
                    .repo_ref(old_key.clone())
                    .host_ref("host-test".to_string())
                    .is_main(true)
                    .build(),
            ),
        )
        .await
        .expect("durable checkout create should succeed");

    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    repositories.delete(&old_key.to_string()).await.expect("simulate already-disappeared old Repository");
    *discovered_repo.write().expect("mutable remote detector should not be poisoned") = "new-repo".to_string();

    daemon.add_repo(&repo).await.expect("migrate repository identity with missing superseded Repository");

    repositories.get(&new_key.to_string()).await.expect("new Repository should be materialized");
    assert!(matches!(repositories.get(&old_key.to_string()).await, Err(flotilla_resources::ResourceError::NotFound { .. })));
    let projects = daemon.resource_backend().using::<Project>("flotilla").list().await.expect("project list");
    assert!(projects.items.is_empty());
}

struct ForgeAliasInspector {
    path: PathBuf,
    remote: &'static str,
    host_ref: &'static str,
}

fn lab_forge_spec() -> flotilla_resources::ForgeSpec {
    use flotilla_resources::{ForgeKind, ForgeSpec};

    ForgeSpec::builder()
        .forge_id("flotilla-lab".to_string())
        .kind(ForgeKind::Forgejo)
        .hosts(BTreeSet::from([
            "forgejo.lab.flotilla.work".to_string(),
            "manchego.lab.flotilla.work".to_string(),
            "forgejo-manchego".to_string(),
        ]))
        .https_url("https://forgejo.lab.flotilla.work".to_string())
        .git_ssh_host("manchego.lab.flotilla.work".to_string())
        .build()
}

#[tokio::test]
async fn discovery_resolves_origin_forge_and_binds_both_forgejo_sources() {
    use flotilla_resources::Forge;

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    init_git_repo_with_remote(&repo, "git@forgejo.lab.flotilla.work:lab/flotilla.git");
    std::fs::write(temp.path().join("lab-forgejo-coder-token"), "test-token\n").expect("write test token");
    let daemon =
        InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), git_process_discovery(false), HostName::local())
            .await;
    daemon
        .resource_backend()
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &lab_forge_spec())
        .await
        .expect("declare Forge");

    let result = daemon.discover_repo_for_environment_for_test(&repo, daemon.local_environment_id()).await.expect("discover providers");
    assert_eq!(result.host_repo_bag.find_origin_forge().expect("origin Forge").forge_id, "flotilla-lab");
    assert_eq!(result.host_repo_bag.repo_identity().expect("repo identity").authority, "forgejo.lab.flotilla.work");
    assert!(result.registry.issue_trackers.get("forgejo").is_some(), "{:?}", result.unmet);
    assert!(result.registry.change_requests.get("forgejo").is_some());
    assert!(result.registry.change_requests.get("github").is_none());
    daemon
        .issue_provider_for_source(&IssueSource { service: "https://forgejo.lab.flotilla.work".into(), scope: "lab/flotilla".into() })
        .await
        .expect("source-addressed Forgejo provider");
}

#[async_trait]
impl RepositoryInspector for ForgeAliasInspector {
    async fn inspect_path(&self, _path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        let remote = self.remote;
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote(flotilla_resources::canonicalize_repo_url(remote)?)?,
            checkout: LocalCheckoutInspection {
                path: self.path.clone(),
                host_ref: self.host_ref.to_string(),
                git_ref: "main".to_string(),
                is_main: true,
            },
            transport_url: Some(remote.to_string()),
            replaces_prior_repository: false,
        })
    }
}

#[tokio::test]
async fn forge_alias_observations_from_two_hosts_record_canonical_live_remote_additively() {
    use flotilla_resources::{Forge, InMemoryBackend};

    let temp = tempfile::tempdir().expect("create tempdir");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let forge = lab_forge_spec();
    backend
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
        .await
        .expect("declare Forge");
    let canonical = "https://forgejo.lab.flotilla.work/robert/ghostty-ops";
    let aliases = ["https://forgejo-manchego/robert/ghostty-ops", "https://manchego.lab.flotilla.work/robert/ghostty-ops"];

    for (index, remote) in
        ["forgejo-manchego:robert/ghostty-ops.git", "git@manchego.lab.flotilla.work:robert/ghostty-ops.git"].into_iter().enumerate()
    {
        let path = temp.path().join(format!("host-{index}")).join("ghostty-ops");
        std::fs::create_dir_all(&path).expect("create checkout path");
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            test_config_store(temp.path().join(format!("config-{index}"))),
            fake_discovery(false),
            HostName::local(),
            backend.clone(),
        )
        .await;
        daemon
            .set_repository_inspector(Arc::new(ForgeAliasInspector {
                path: path.clone(),
                remote,
                host_ref: if index == 0 { "host-kiwi" } else { "host-manchego" },
            }))
            .await;
        daemon.add_repo(&path).await.expect("observe checkout");

        let repositories = backend.using::<Repository>("flotilla");
        let items = repositories.list().await.expect("list Repositories").items;
        assert_eq!(items.len(), 1, "host observations share one Repository");
        assert_eq!(items[0].spec.live_remote(), Some(canonical));
        let expected =
            if index == 0 { BTreeSet::from([canonical, aliases[0]]) } else { BTreeSet::from([canonical, aliases[0], aliases[1]]) };
        assert_eq!(items[0].spec.remotes().iter().map(String::as_str).collect::<BTreeSet<_>>(), expected);
    }
}

#[tokio::test]
async fn forge_identity_sweep_merges_split_repositories_and_project_members() {
    use flotilla_resources::{
        ConvoyEnsure, ConvoyEnsureSpec, CrewSource, CrewSpec, Forge, LandingCredentialScope, ProjectRepositoryRole, RepositoryIdentity,
        Selector, VesselRequirement, WorkflowTemplateSpec,
    };

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("ghostty-ops");
    std::fs::create_dir_all(&repo).expect("create repository path");
    let daemon =
        InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    daemon
        .set_repository_inspector(Arc::new(ForgeAliasInspector {
            path: repo.clone(),
            remote: "https://manchego.lab.flotilla.work/robert/ghostty-ops.git",
            host_ref: "host-test",
        }))
        .await;
    let forge = lab_forge_spec();
    let front = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops").expect("front spec");
    let ssh = RepositorySpec::remote("https://manchego.lab.flotilla.work/robert/ghostty-ops")
        .expect("ssh host spec")
        .with_allow_reviewless_workflows(true);
    let alias = RepositorySpec::remote("https://forgejo-manchego/robert/ghostty-ops").expect("host alias spec");
    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    for spec in [&front, &ssh, &alias] {
        repositories.create(&InputMeta::builder().name(spec.key().to_string()).build(), spec).await.expect("legacy Repository");
    }
    let projects = daemon.resource_backend().definitions::<Project>("flotilla");
    projects
        .create(&InputMeta::builder().name("ghostty".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "ghostty".to_string(),
            default_workflow_ref: "single-agent".to_string(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![
                ProjectRepositorySpec {
                    repo: front.key(),
                    alias: None,
                    roles: [ProjectRepositoryRole::Code].into(),
                    subpath: None,
                    default_branch: Some("main".to_string()),
                },
                ProjectRepositorySpec {
                    repo: ssh.key(),
                    alias: None,
                    roles: [ProjectRepositoryRole::Ops].into(),
                    subpath: None,
                    default_branch: Some("main".to_string()),
                },
                ProjectRepositorySpec {
                    repo: alias.key(),
                    alias: None,
                    roles: [ProjectRepositoryRole::Ops].into(),
                    subpath: None,
                    default_branch: Some("main".to_string()),
                },
            ],
        })
        .await
        .expect("legacy project");

    let overrides = vec![flotilla_protocol::AgentOverride {
        capability: "governor".to_string(),
        adapter: "codex".to_string(),
        model: Some("fable".to_string()),
    }];
    let ensures = daemon.resource_backend().definitions::<ConvoyEnsure>("flotilla");
    ensures
        .create(
            &InputMeta::builder()
                .name("governor".to_string())
                .annotations(BTreeMap::from([
                    ("flotilla.work/source-repository".to_string(), front.key().to_string()),
                    ("flotilla.work/source-commit".to_string(), "abc123".to_string()),
                ]))
                .build(),
            &ConvoyEnsureSpec::builder()
                .project_ref("ghostty".to_string())
                .role("governor".to_string())
                .workflow_ref("governor".to_string())
                .repositories(vec![front.key(), ssh.key(), alias.key()])
                .agent_overrides(overrides.clone())
                .build(),
        )
        .await
        .expect("legacy ensure");
    let templates = daemon.resource_backend().definitions::<WorkflowTemplate>("flotilla");
    templates
        .create(
            &InputMeta::builder().name("governor".to_string()).build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![front.key(), ssh.key(), alias.key()])
                    .crew(vec![CrewSpec::builder()
                        .role("governor".to_string())
                        .source(CrewSource::Agent { selector: Selector::for_capability("governor"), prompt: None, brief_template: None })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("legacy workflow");
    templates
        .create(
            &InputMeta::builder().name("scoped-workflow".to_string()).build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .credential_scopes(BTreeMap::from([("landing".to_string(), BTreeSet::from([front.key(), ssh.key()]))]))
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("legacy scoped workflow");
    let grants = daemon.resource_backend().definitions::<CredentialGrant>("flotilla");
    grants
        .create(
            &InputMeta::builder().name("legacy-grant".to_string()).build(),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().repositories(BTreeSet::from([front.key(), ssh.key()])).build())
                .credentials(BTreeSet::new())
                .landing_credentials(BTreeMap::from([("landing".to_string(), LandingCredentialScope::Branch {
                    repository: front.key(),
                    branch: "main".to_string(),
                })]))
                .build(),
        )
        .await
        .expect("legacy grant");

    let expected = front.clone().on_forge(&forge).expect("canonical forge identity").key();
    let host_ref = "host-udder".to_string();
    create_test_host_direct_policy(&daemon.resource_backend(), "governor-host", &host_ref, 1, BTreeSet::from(["codex".to_string()])).await;
    let hosts = daemon.resource_backend().using::<ResourceHost>("flotilla");
    let host = hosts.get(&host_ref).await.expect("governor host");
    let mut status = host.status.expect("governor host status");
    status.fulfilment_facts.insert("governor-host".to_string(), FulfilmentFacts {
        harnesses: BTreeMap::from([("codex".to_string(), HarnessFacts {
            version: "1.0.0".to_string(),
            models: BTreeMap::from([("fable".to_string(), ModelFact { usable: true, source: ModelFactSource::Probe })]),
        })]),
        observed_at: chrono::Utc::now(),
        ..Default::default()
    });
    hosts.update_status(&host_ref, &host.metadata.resource_version, &status).await.expect("publish governor model facts");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("pre-sweep ensure admits");
    let first = admitted_convoy(&daemon.resource_backend(), "governor").await;
    assert!(first.spec.repositories.iter().any(|repository| repository.repo_ref == alias.key()));
    let convoys = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    convoys
        .update_status(&first.metadata.name, &first.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("first generation fails");
    daemon
        .resource_backend()
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &forge)
        .await
        .expect("declare forge after first admission");
    let forge_repository = daemon.inspect_repository_path(&repo, None).await.expect("resolve Forge repository");
    let stored = repositories.get(&forge_repository.key().to_string()).await.expect("Forge repository");
    let alias_first = stored.spec.update_remotes("https://forgejo-manchego/robert/ghostty-ops").expect("observe alias remote");
    repositories
        .update(&InputMeta::builder().name(stored.metadata.name.clone()).build(), &stored.metadata.resource_version, &alias_first)
        .await
        .expect("record alias first");
    daemon.reconcile_convoy_ensure_now("flotilla", "governor", daemon.as_ref()).await.expect("ensure re-admits after forge sweep");
    let renewed = ensures.get("governor").await.expect("migrated ensure");
    let successor = convoys.get(&renewed.status.expect("ensure status").convoy_ref.expect("successor")).await.expect("second generation");
    assert_eq!(successor.spec.generation, 2);
    assert_eq!(successor.spec.repositories.len(), 1);
    assert_eq!(successor.spec.repositories[0].repo_ref, expected);
    assert_eq!(successor.spec.repositories[0].url, "https://forgejo.lab.flotilla.work/robert/ghostty-ops");
    assert_eq!(
        successor.status.as_ref().expect("admitted status").placement_decision.as_ref().expect("placement").policy_name,
        "governor-host"
    );

    let inspected = daemon.inspect_repository_path(&repo, None).await.expect("resolve and sweep identities");
    assert!(matches!(inspected.spec.identity(), RepositoryIdentity::Forge { forge_ref, .. } if forge_ref == "flotilla-lab"));
    let merged = repositories.get(&inspected.key().to_string()).await.expect("merged Repository");
    assert!(merged.spec.allows_reviewless_workflows());
    assert_eq!(merged.spec.remotes().len(), 3);
    assert!(matches!(repositories.get(&front.key().to_string()).await, Err(ResourceError::NotFound { .. })));
    assert!(matches!(repositories.get(&ssh.key().to_string()).await, Err(ResourceError::NotFound { .. })));
    assert!(matches!(repositories.get(&alias.key().to_string()).await, Err(ResourceError::NotFound { .. })));
    let project = projects.get("ghostty").await.expect("migrated project");
    assert_eq!(project.spec.repositories.len(), 1);
    assert_eq!(project.spec.repositories[0].repo, inspected.key());
    assert_eq!(project.spec.repositories[0].default_branch.as_deref(), Some("main"));
    assert_eq!(project.spec.repositories[0].roles, [ProjectRepositoryRole::Code, ProjectRepositoryRole::Ops].into());
    let ensure = ensures.get("governor").await.expect("migrated ensure");
    assert_eq!(ensure.spec.repositories, vec![inspected.key()]);
    assert_eq!(ensure.spec.agent_overrides, overrides);
    assert_eq!(ensure.metadata.annotations["flotilla.work/source-repository"], inspected.key().to_string());
    let template = templates.get("governor").await.expect("migrated workflow");
    assert_eq!(template.spec.vessels[0].repository_refs, Some(vec![inspected.key()]));
    let scoped = templates.get("scoped-workflow").await.expect("migrated scoped workflow");
    assert_eq!(scoped.spec.vessels[0].credential_scopes["landing"], BTreeSet::from([inspected.key()]));
    let grant = grants.get("legacy-grant").await.expect("migrated grant");
    assert_eq!(grant.spec.selector.repositories, BTreeSet::from([inspected.key()]));
    assert_eq!(grant.spec.landing_credentials["landing"], LandingCredentialScope::Branch {
        repository: inspected.key(),
        branch: "main".to_string()
    });
    assert_eq!(successor.spec.repositories[0].repo_ref, inspected.key());

    // #2159: refuse the overlapping declaration before it can poison namespace
    // consumers, and retain the previously resolved repository identity.
    let mut overlap = lab_forge_spec();
    overlap.forge_id = "other-lab".to_string();
    let error = daemon
        .resource_backend()
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("other-lab".to_string()).build(), &overlap)
        .await
        .expect_err("overlapping Forge ownership must be refused at admission");
    assert!(error.to_string().contains("overlaps issue-service ownership"), "{error}");
    let unchanged = daemon.inspect_repository_path(&repo, None).await.expect("refused declaration leaves namespace usable");
    assert_eq!(unchanged.key(), inspected.key());
}

#[tokio::test]
async fn forge_identity_sweep_reports_conflicting_aliases_before_changing_repositories() {
    use flotilla_resources::{Forge, ProjectRepositoryRole};

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("ghostty-ops");
    std::fs::create_dir_all(&repo).expect("create repository path");
    let daemon =
        InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    daemon
        .set_repository_inspector(Arc::new(ForgeAliasInspector {
            path: repo.clone(),
            remote: "https://manchego.lab.flotilla.work/robert/ghostty-ops.git",
            host_ref: "host-test",
        }))
        .await;
    daemon
        .resource_backend()
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &lab_forge_spec())
        .await
        .expect("declare forge");
    let front = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops").expect("front");
    let ssh = RepositorySpec::remote("https://manchego.lab.flotilla.work/robert/ghostty-ops").expect("ssh");
    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    for spec in [&front, &ssh] {
        repositories.create(&InputMeta::builder().name(spec.key().to_string()).build(), spec).await.expect("legacy Repository");
    }
    daemon
        .resource_backend()
        .definitions::<Project>("flotilla")
        .create(&InputMeta::builder().name("ghostty".to_string()).build(), &ProjectSpec {
            platform_matrix: Vec::new(),
            role_needs: Default::default(),
            skills: BTreeMap::new(),
            display_name: "ghostty".to_string(),
            default_workflow_ref: "single-agent".to_string(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            dispatch_policy: None,
            repositories: vec![
                ProjectRepositorySpec {
                    repo: front.key(),
                    alias: Some("front".to_string()),
                    roles: [ProjectRepositoryRole::Code].into(),
                    subpath: None,
                    default_branch: None,
                },
                ProjectRepositorySpec {
                    repo: ssh.key(),
                    alias: Some("ssh".to_string()),
                    roles: [ProjectRepositoryRole::Ops].into(),
                    subpath: None,
                    default_branch: None,
                },
            ],
        })
        .await
        .expect("legacy project");

    let error = daemon.inspect_repository_path(&repo, None).await.expect_err("aliases need operator resolution");
    assert!(error.contains("aliases `front` and `ssh`") && error.contains("resolve the aliases"), "{error}");
    let target = ssh.clone().on_forge(&lab_forge_spec()).expect("forge identity").key();
    assert!(matches!(repositories.get(&target.to_string()).await, Err(ResourceError::NotFound { .. })));
    repositories.get(&front.key().to_string()).await.expect("front legacy Repository preserved");
    repositories.get(&ssh.key().to_string()).await.expect("ssh legacy Repository preserved");
}

#[tokio::test]
async fn forge_identity_sweep_merges_records_whose_bookkeeping_annotations_differ() {
    use flotilla_resources::Forge;

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("katzensteg-ops");
    std::fs::create_dir_all(&repo).expect("create repository path");
    let daemon =
        InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    daemon
        .set_repository_inspector(Arc::new(ForgeAliasInspector {
            path: repo.clone(),
            remote: "https://manchego.lab.flotilla.work/robert/katzensteg-ops.git",
            host_ref: "host-test",
        }))
        .await;
    daemon
        .resource_backend()
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &lab_forge_spec())
        .await
        .expect("declare forge");
    let front = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/katzensteg-ops").expect("front");
    let ssh = RepositorySpec::remote("https://manchego.lab.flotilla.work/robert/katzensteg-ops").expect("ssh");
    let target = ssh.clone().on_forge(&lab_forge_spec()).expect("forge identity").key();
    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    // Each record was bootstrapped and synced separately, so its bookkeeping annotations
    // differ (live incident 2026-10-01: feta governors refused re-admission after a reboot).
    for (spec, commit, synced) in [(&front, "98acd5b1", "2026-09-25T10:00:00Z"), (&ssh, "04d84fe0", "2026-09-24T09:00:00Z")] {
        repositories
            .create(
                &InputMeta::builder()
                    .name(spec.key().to_string())
                    .annotations(BTreeMap::from([
                        ("flotilla.work/project-bootstrap-commit".to_string(), commit.to_string()),
                        ("flotilla.work/last-synced-at".to_string(), synced.to_string()),
                    ]))
                    .build(),
                spec,
            )
            .await
            .expect("legacy Repository");
    }

    daemon.inspect_repository_path(&repo, None).await.expect("bookkeeping annotation differences must not block the sweep");
    let merged = repositories.get(&target.to_string()).await.expect("forge-identity Repository created");
    let commit = merged.metadata.annotations.get("flotilla.work/project-bootstrap-commit").expect("bootstrap commit carried");
    assert!(commit == "98acd5b1" || commit == "04d84fe0", "{commit}");
}

#[tokio::test]
async fn forge_identity_sweep_includes_replica_only_legacy_repository() {
    use flotilla_resources::{Forge, InMemoryBackend};

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("ghostty-ops");
    std::fs::create_dir_all(&repo).expect("create repository path");
    let source_root = NodeId::new("source-root");
    let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(source_root.clone());
    let target = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("target-root"));
    let old = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops")
        .expect("legacy Repository")
        .with_allow_reviewless_workflows(true);
    source
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(old.key().to_string()).build(), &old)
        .await
        .expect("author legacy Repository");
    let snapshot = source.using::<Repository>("flotilla").list().await.expect("source list");
    target
        .replica_writer::<Repository>(source_root, "flotilla")
        .replace(&snapshot, chrono::Utc::now())
        .await
        .expect("replicate Repository");
    target
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &lab_forge_spec())
        .await
        .expect("declare forge");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        test_config_store(temp.path().join("config")),
        fake_discovery(false),
        HostName::local(),
        target.clone(),
    )
    .await;
    daemon
        .set_repository_inspector(Arc::new(ForgeAliasInspector {
            path: repo.clone(),
            remote: "https://manchego.lab.flotilla.work/robert/ghostty-ops.git",
            host_ref: "host-test",
        }))
        .await;

    let inspected = daemon.inspect_repository_path(&repo, None).await.expect("resolve replica-only identity");
    let merged = target.using::<Repository>("flotilla").get(&inspected.key().to_string()).await.expect("local canonical Repository");
    assert!(merged.spec.allows_reviewless_workflows());
    assert_eq!(merged.spec.remotes().len(), 2);
}

#[tokio::test]
async fn associated_checkout_remote_move_updates_repository_in_place() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    let fresh = temp.path().join("fresh");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    std::fs::create_dir_all(&fresh).expect("create fresh dir");
    let state = FakeVcsState::builder(repo.clone()).branch("main", true).checkout("main").is_main(true).path(&repo).build().build();
    let remote = Arc::new(std::sync::RwLock::new("old-repo".to_string()));
    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(MutableRemoteHostDetector { owner: "owner", repo: Arc::clone(&remote) }));
    let daemon = InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::clone(&remote)).await;
    daemon.add_repo(&repo).await.expect("track old remote");

    let original = RepositorySpec::remote("https://github.com/owner/old-repo").expect("old spec");
    let original_key = original.key();
    *remote.write().expect("remote lock") = "new-repo".to_string();
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh moved remote");

    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    let items = repositories.list().await.expect("list repositories").items;
    assert_eq!(items.len(), 1, "remote move must not mint a sibling Repository");
    let updated = &items[0];
    assert_eq!(updated.metadata.name, original_key.to_string());
    assert_eq!(updated.spec.remotes(), ["https://github.com/owner/new-repo", "https://github.com/owner/old-repo"]);
    assert_eq!(updated.spec.forge().expect("live forge").repository, "owner/new-repo");

    let fresh_inspection = daemon.inspect_repository_path(&fresh, None).await.expect("inspect fresh moved checkout");
    assert_eq!(fresh_inspection.key(), original_key, "declared moved remote must resolve to the birth identity");
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ProjectAdd {
                    target: fresh.to_string_lossy().into_owned(),
                    name: Some("moved-project".to_string()),
                    display_name: None,
                    remote: None,
                })
                .build(),
        )
        .await
        .expect("project add moved checkout");
    assert!(matches!(recv_command_finished(&mut events, command_id).await, CommandValue::ProjectAdded { .. }));
    assert_eq!(repositories.list().await.expect("repositories after project add").items.len(), 1);
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>("flotilla").list().await.expect("observed checkouts");
    assert!(observed.items.iter().all(|checkout| checkout.spec.repo_ref() == &original_key));
}

#[tokio::test]
async fn associated_path_replaced_by_unrelated_repository_mints_new_identity() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("ghostty-ops");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let state = FakeVcsState::builder(repo.clone()).branch("main", true).checkout("main").is_main(true).path(&repo).build().build();
    let remote = Arc::new(std::sync::RwLock::new("ghostty".to_string()));
    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(MutableRemoteHostDetector { owner: "fork-issues", repo: Arc::clone(&remote) }));
    let daemon = InProcessDaemon::new(Vec::new(), test_config_store(temp.path().join("config")), discovery, HostName::local()).await;
    daemon
        .set_repository_inspector(Arc::new(TestRepositoryInspector {
            host_ref: daemon.local_host_id().expect("local Host").to_string(),
            repository: Arc::clone(&remote),
            fixed_repository_by_path: HashMap::new(),
            continuity: false,
            inspection_failure: None,
        }))
        .await;
    daemon.add_repo(&repo).await.expect("track original repository");

    let old_key = RepositorySpec::remote("https://github.com/owner/ghostty").expect("old spec").key();
    let unaffected_path = temp.path().join("ghostty-main");
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>("flotilla");
    observed
        .create(
            &InputMeta::builder()
                .name("unaffected-old-checkout".to_string())
                .labels(BTreeMap::from([(REPO_KEY_LABEL.to_string(), old_key.to_string())]))
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Observed),
            &ResourceCheckoutSpec::Observed(
                ObservedCheckoutSpec::builder()
                    .r#ref("main".to_string())
                    .path(unaffected_path.to_string_lossy().into_owned())
                    .repo_ref(old_key.clone())
                    .host_ref("other-host".to_string())
                    .is_main(true)
                    .build(),
            ),
        )
        .await
        .expect("create unaffected old checkout observation");
    *remote.write().expect("remote lock") = "ghostty-ops".to_string();
    let change = daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh replacement");

    let new_key = RepositorySpec::remote("https://github.com/owner/ghostty-ops").expect("new spec").key();
    assert!(change.is_some(), "replacement should report an identity change");
    assert_eq!(daemon.repository_key_for_path(&repo).await, Some(new_key.clone()));
    let repositories = daemon.resource_backend().using::<Repository>("flotilla").list().await.expect("list repositories");
    assert_eq!(repositories.items.len(), 2, "unrelated replacement must not be absorbed into the old Repository");
    let old = repositories.items.iter().find(|item| item.metadata.name == old_key.to_string()).expect("old Repository remains");
    assert_eq!(old.spec.remotes(), ["https://github.com/owner/ghostty"]);
    repositories.items.iter().find(|item| item.metadata.name == new_key.to_string()).expect("new Repository is materialized");
    observed.get("unaffected-old-checkout").await.expect("unaffected old checkout observation remains");
}

async fn recv_event(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>) -> DaemonEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.expect("timeout waiting for event").expect("recv error")
}

async fn recv_command_finished(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandValue {
    loop {
        match recv_event(rx).await {
            DaemonEvent::CommandFinished { command_id: finished_id, result, .. } if finished_id == command_id => return result,
            _ => {}
        }
    }
}

async fn wait_for_observed_checkout_count(observed: &TypedResolver<ResourceCheckout>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if observed.list().await.expect("observed checkout list should succeed").items.len() == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observed checkout count should converge");
}

#[tokio::test]
async fn execute_broadcasts_lifecycle_events() {
    let (_temp, repo, daemon, identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // Execute a command that goes through the spawned task path.
    // ArchiveSession with a non-existent ID returns immediately with
    // "session not found" — no external API calls, deterministic.
    // We only care about the lifecycle events, not the command result.
    let command = Command::builder()
        .action(CommandAction::ArchiveSession { session_id: "nonexistent-session".into() })
        .context_repo(RepoSelector::Identity(identity.clone()))
        .build();
    let command_id = daemon.execute(command).await.expect("execute should return a command id");

    // Collect CommandStarted and CommandFinished events, skipping any
    // Repo snapshot events that arrive from the background refresh loop.
    let timeout = std::time::Duration::from_secs(10);
    let mut got_started = false;
    let mut got_finished = false;
    let mut started_id = None;
    let mut finished_id = None;

    let result = tokio::time::timeout(timeout, async {
        while !got_started || !got_finished {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, node_id, repo_identity, repo: ref event_repo, .. }) => {
                    assert_eq!(node_id, *daemon.node_id(), "CommandStarted node should default to local node");
                    assert_eq!(repo_identity, identity, "CommandStarted repo identity should match executed repo");
                    assert_eq!(event_repo.as_deref(), Some(repo.as_path()), "CommandStarted repo should match executed repo");
                    started_id = Some(id);
                    got_started = true;
                }
                Ok(DaemonEvent::CommandFinished { command_id: id, node_id, repo_identity, repo: ref event_repo, .. }) => {
                    assert_eq!(node_id, *daemon.node_id(), "CommandFinished node should default to local node");
                    assert_eq!(repo_identity, identity, "CommandFinished repo identity should match executed repo");
                    assert_eq!(event_repo.as_deref(), Some(repo.as_path()), "CommandFinished repo should match executed repo");
                    finished_id = Some(id);
                    got_finished = true;
                }
                Ok(_) => {
                    // Skip snapshot and other events
                }
                Err(e) => panic!("unexpected recv error: {:?}", e),
            }
        }
    })
    .await;

    result.expect("timed out waiting for lifecycle events");

    // Both events must carry the same command ID returned by execute()
    assert_eq!(started_id, Some(command_id), "CommandStarted id should match the id returned by execute()");
    assert_eq!(finished_id, Some(command_id), "CommandFinished id should match the id returned by execute()");
}

#[tokio::test]
async fn fetch_checkout_status_accepts_identity_context_repo() {
    let (_temp, _repo, daemon, identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    let command = Command::builder()
        .action(CommandAction::FetchCheckoutStatus { branch: "main".into(), checkout_path: None, change_request_id: None })
        .context_repo(RepoSelector::Identity(identity.clone()))
        .build();

    let command_id = daemon.execute(command).await.expect("status command should resolve via identity context repo");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: finished_id, repo_identity, result, .. }) if finished_id == command_id => {
                    assert_eq!(repo_identity, identity, "finished event should preserve repo identity");
                    break result;
                }
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for checkout status command to finish");

    assert!(matches!(result, CommandValue::CheckoutStatus(_)), "expected checkout status result via identity context repo, got {result:?}");
}

#[tokio::test]
async fn add_and_remove_repo_updates_state_and_emits_events() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("new-repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);

    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("new-repo".to_string()))).await;
    let mut rx = daemon.subscribe();

    let add_id = daemon
        .execute(Command::builder().action(CommandAction::TrackRepoPath { path: repo.clone() }).build())
        .await
        .expect("add_repo command should return an id");

    let (started_add, finished_add, added) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut started = None;
        let mut finished = None;
        let mut added = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id, repo_identity, .. }) if command_id == add_id => started = Some(repo_identity),
                Ok(DaemonEvent::CommandFinished { command_id, repo_identity, result, .. }) if command_id == add_id => {
                    finished = Some((repo_identity, result));
                }
                Ok(DaemonEvent::RepoTracked(info)) => added = Some(*info),
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
            if let (Some(_), Some(_), Some(_)) = (&started, &finished, &added) {
                break (started.take().expect("started set"), finished.take().expect("finished set"), added.take().expect("added set"));
            }
        }
    })
    .await
    .expect("timeout waiting for add command events");
    let (finished_identity, finished_result) = finished_add;
    assert!(matches!(finished_result, CommandValue::RepoTracked { ref path, .. } if *path == repo));
    assert_eq!(finished_identity, added.identity, "CommandFinished should use the tracked repo identity");
    assert_eq!(started_add, added.identity, "CommandStarted should use the tracked repo identity");
    assert_eq!(added.path.as_deref(), Some(repo.as_path()));
    assert!(added.repository_key.is_some(), "RepoTracked should publish the queryable repository key");

    let repos = daemon.list_repos().await.expect("list_repos after add");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path.as_deref(), Some(repo.as_path()));
    assert_eq!(repos[0].repository_key, added.repository_key);

    let remove_id = daemon
        .execute(Command::builder().action(CommandAction::UntrackRepo { repo: RepoSelector::Query("new-repo".into()) }).build())
        .await
        .expect("remove_repo command should return an id");
    let (finished_remove, removed) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut finished = None;
        let mut removed = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == remove_id => finished = Some(result),
                Ok(DaemonEvent::RepoUntracked { path, .. }) => removed = Some(path),
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
            if let (Some(_), Some(_)) = (&finished, &removed) {
                break (finished.take().expect("finished set"), removed.take().expect("removed set"));
            }
        }
    })
    .await
    .expect("timeout waiting for remove command events");
    assert!(matches!(finished_remove, CommandValue::RepoUntracked { ref path } if *path == repo));
    assert_eq!(removed.as_deref(), Some(repo.as_path()));

    let repos = daemon.list_repos().await.expect("list_repos after remove");
    assert!(repos.is_empty());
}

#[tokio::test]
async fn remove_repo_persistence_failure_leaves_repo_tracked() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);

    let config_dir = temp.path().join("config");
    let config = test_config_store(config_dir.clone());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".to_string()))).await;
    daemon.add_repo(&repo).await.expect("track repo");

    let roots_file = config_dir.join("observation-roots.toml");
    std::fs::remove_file(&roots_file).expect("remove roots file");
    std::fs::create_dir(&roots_file).expect("replace roots file with directory");

    let error = daemon.remove_repo(&repo).await.expect_err("persistence failure should abort removal");
    assert!(error.contains("failed to read"), "unexpected error: {error}");
    assert!(daemon.tracked_repo_identity_for_path(&repo).await.is_some(), "repo should remain tracked");
    assert_eq!(daemon.list_repos().await.expect("list repos").len(), 1);
}

#[tokio::test]
async fn remove_repo_command_cleans_up_an_untracked_observation_root() {
    let temp = tempfile::tempdir().unwrap();
    let unavailable = temp.path().join("unavailable-repo");
    let config = test_config_store(temp.path().join("config"));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&unavailable)).expect("persist observation root");
    let daemon = InProcessDaemon::new(vec![], Arc::clone(&config), fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let command_id = daemon
        .execute(Command::builder().action(CommandAction::UntrackRepo { repo: RepoSelector::Query("unavailable-repo".into()) }).build())
        .await
        .expect("remove observation root command");
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(DaemonEvent::CommandFinished { command_id: finished_id, result, .. }) = rx.recv().await {
                if finished_id == command_id {
                    break result;
                }
            }
        }
    })
    .await
    .expect("timeout waiting for remove command");

    assert!(matches!(result, CommandValue::RepoUntracked { ref path } if *path == unavailable));
    assert!(config.load_observation_roots().expect("load observation roots").is_empty());
}

#[tokio::test]
async fn duplicate_local_roots_share_identity_but_remain_tracked() {
    let (_temp, repo_a, repo_b, daemon) = daemon_for_duplicate_fake_repos().await;

    let identity_a = daemon.tracked_repo_identity_for_path(&repo_a).await.expect("identity for first repo");
    let identity_b = daemon.tracked_repo_identity_for_path(&repo_b).await.expect("identity for second repo");
    assert_eq!(identity_a, identity_b, "same upstream repo should resolve to one repo identity");

    let tracked = daemon.tracked_repo_paths().await;
    assert!(tracked.contains(&repo_a));
    assert!(tracked.contains(&repo_b));

    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1, "list_repos should expose one logical repo per identity");
    assert_eq!(repos[0].identity, identity_a);
    assert_eq!(repos[0].path.as_deref(), Some(repo_a.as_path()), "first tracked root should remain the deterministic preferred path");

    daemon.remove_repo(&repo_a).await.expect("remove preferred root");
    let repos = daemon.list_repos().await.expect("list_repos after removing preferred root");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].identity, identity_b);
    assert_eq!(repos[0].path.as_deref(), Some(repo_b.as_path()), "remaining root should become the preferred path");
    assert!(daemon.tracked_repo_identity_for_path(&repo_a).await.is_none());
    assert_eq!(daemon.tracked_repo_identity_for_path(&repo_b).await, Some(identity_b));
}

// TODO(task-9): Migrate to fake VCS — this test depends on real git for two reasons:
// 1. `normalize_repo_path` uses `GitVcs` directly to canonicalize symlinked temp paths
//    (e.g. /var → /private/var on macOS), so `tracked_path == canonical_repo` requires
//    a real git process to resolve the canonical form.
// 2. The identity match relies on git reading the remote URL; `local_bare_remote_discovery`
//    uses a real git runner to detect `github.com/owner/repo` from the remote.
// Skipping fake migration until `normalize_repo_path` uses an injectable Vcs.
#[tokio::test]
async fn adding_local_clone_promotes_remote_only_identity_to_local_execution() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let local_repo = temp.path().join("repo");
    let remote = temp.path().join("origin.git");
    let _ = init_git_repo_with_local_bare_remote(&local_repo, &remote);
    let identity = test_repo_identity();
    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![], config, local_bare_remote_discovery(), HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".to_string()))).await;

    daemon
        .add_virtual_repo(identity.clone(), None, PathBuf::from("/remote/desktop/owner/repo"), vec![], 0)
        .await
        .expect("add virtual repo");
    let outcome = daemon.add_repo(&local_repo).await.expect("add local repo");
    let tracked_path = outcome.tracked_path;
    // Path may be canonicalized (e.g. /var -> /private/var on macOS)
    let canonical_repo = std::fs::canonicalize(&local_repo).unwrap_or_else(|_| local_repo.clone());

    let repos = daemon.list_repos().await.expect("list repos");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].identity, identity);
    assert_eq!(repos[0].path.as_deref(), Some(canonical_repo.as_path()), "local clone should become the preferred executable path");
    assert_eq!(tracked_path, canonical_repo);
    assert_eq!(daemon.preferred_local_path_for_identity(&identity).await, Some(canonical_repo.clone()));
    assert_eq!(daemon.tracked_repo_paths().await, vec![canonical_repo]);
}

#[tokio::test]
async fn refresh_unobserved_checkout_refuses_without_started_event() {
    let config_tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config_store(config_tmp.path().to_path_buf());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();
    let repo = std::path::PathBuf::from("/tmp/does-not-exist-for-daemon-test");

    let err = daemon
        .execute(Command::builder().action(CommandAction::Refresh { repo: Some(RepoSelector::Path(repo.clone())) }).build())
        .await
        .expect_err("untracked repo should fail");
    assert!(err.contains("no Repository matches"));

    let started = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { .. }) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert!(started.is_err() || !started.unwrap(), "should not emit CommandStarted for invalid repo");
}

#[tokio::test]
async fn stop_observing_missing_checkout_refuses_without_started_event() {
    let config_tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config_store(config_tmp.path().to_path_buf());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();
    let repo = std::path::PathBuf::from("/tmp/does-not-exist-for-daemon-test");

    let err = daemon
        .execute(Command::builder().action(CommandAction::UntrackRepo { repo: RepoSelector::Path(repo.clone()) }).build())
        .await
        .expect_err("untracked repo removal should fail");
    assert!(err.contains("no observed checkout at"));

    let started = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { .. }) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert!(started.is_err() || !started.unwrap(), "should not emit CommandStarted for missing repo removal");
}

#[tokio::test]
async fn refresh_all_command_refreshes_every_tracked_repo() {
    let temp = tempfile::tempdir().unwrap();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let config = test_config_store(temp.path().join("config"));
    let daemon = InProcessDaemon::new(vec![repo_a.clone(), repo_b.clone()], config, fake_discovery(false), HostName::local()).await;
    daemon
        .set_repository_inspector(Arc::new(TestRepositoryInspector {
            host_ref: daemon.local_host_id().expect("local Host").to_string(),
            repository: Arc::new(std::sync::RwLock::new("repo".into())),
            fixed_repository_by_path: HashMap::from([(repo_a.clone(), "repo-a".into()), (repo_b.clone(), "repo-b".into())]),
            continuity: true,
            inspection_failure: None,
        }))
        .await;
    daemon.add_repo(&repo_a).await.expect("adopt first checkout");
    daemon.add_repo(&repo_b).await.expect("adopt second checkout");
    let mut rx = daemon.subscribe();

    let refresh_id = daemon
        .execute(Command::builder().action(CommandAction::Refresh { repo: None }).build())
        .await
        .expect("refresh all should return an id");

    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == refresh_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for refresh all CommandFinished");

    assert!(matches!(finished, CommandValue::Refreshed { repos, .. } if repos.len() == 2));
}

#[tokio::test]
async fn remove_checkout_command_accepts_selector_queries() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let err = daemon
        .execute(
            Command::builder().action(CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("does-not-exist".into()) }).build(),
        )
        .await
        .expect_err("missing checkout should fail cleanly");

    assert!(
        err.contains("checkout") || err.contains("does-not-exist") || err.contains(repo.to_string_lossy().as_ref()),
        "expected checkout resolution error, got {err}"
    );
}

#[tokio::test]
async fn fetch_checkout_status_uses_context_repo_when_checkout_path_is_absent() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let command = Command::builder()
        .action(CommandAction::FetchCheckoutStatus { branch: "main".into(), checkout_path: None, change_request_id: None })
        .context_repo(RepoSelector::Path(repo.clone()))
        .build();

    let command_id = daemon.execute(command).await.expect("status command should resolve via context repo");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: finished_id, result, .. }) if finished_id == command_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for checkout status command to finish");

    assert!(matches!(result, CommandValue::CheckoutStatus(_)), "expected checkout status result via context repo, got {result:?}");
}

#[tokio::test]
async fn checkout_target_branch_and_fresh_branch_are_distinct_errors() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let branch_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::Checkout {
                    repo: RepoSelector::Path(repo.clone()),
                    target: CheckoutTarget::Branch("definitely-missing-branch".into()),
                    issue_ids: vec![],
                })
                .build(),
        )
        .await
        .expect("checking out a missing existing branch should return a command id");

    let fresh_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::Checkout {
                    repo: RepoSelector::Path(repo),
                    target: CheckoutTarget::FreshBranch("main".into()),
                    issue_ids: vec![],
                })
                .build(),
        )
        .await
        .expect("creating a fresh branch that already exists should return a command id");
    let mut branch_err = None;
    let mut fresh_err = None;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while branch_err.is_none() || fresh_err.is_none() {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == branch_id => match result {
                    CommandValue::Error { message } => branch_err = Some(message),
                    other => panic!("expected error for Branch checkout, got {other:?}"),
                },
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == fresh_id => match result {
                    CommandValue::Error { message } => fresh_err = Some(message),
                    other => panic!("expected error for FreshBranch checkout, got {other:?}"),
                },
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await;
    outcome.expect("timed out waiting for checkout failures");

    assert_ne!(branch_err, fresh_err, "Branch and FreshBranch should remain distinct intents");
}

#[tokio::test]
async fn missing_issue_provider_names_the_missing_capability_on_every_host() {
    let config_tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config_store(config_tmp.path().to_path_buf());
    let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };

    let leader = InProcessDaemon::new(vec![], config.clone(), fake_discovery(false), HostName::local()).await;
    let leader_error = leader.issue_provider_for_source(&source).await.err().expect("leader should have no provider in test environment");
    assert_eq!(leader_error, "no issue provider available for https://github.com flotilla-org/flotilla");

    let follower = InProcessDaemon::new(vec![], config, fake_discovery(true), HostName::local()).await;
    let follower_error = follower.issue_provider_for_source(&source).await.err().expect("follower should have no issue provider");
    assert_eq!(follower_error, "no issue provider available for https://github.com flotilla-org/flotilla");
}

#[tokio::test]
async fn add_virtual_repo_is_idempotent() {
    let config_tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config_store(config_tmp.path().to_path_buf());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    daemon.add_virtual_repo(identity.clone(), None, synthetic_path.clone(), vec![], 0).await.expect("first add should succeed");

    // Second add with same path should be a no-op
    daemon.add_virtual_repo(identity, None, synthetic_path.clone(), vec![], 0).await.expect("second add should succeed (idempotent)");

    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1, "should still have exactly one repo");
}

#[tokio::test]
async fn get_repo_providers_returns_structured_unmet_requirements_and_discovery() {
    let (_temp, repo, daemon) = daemon_for_plain_dir().await;

    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("repo".into()))).await;
    daemon.add_repo(&repo).await.expect("adopt test checkout into resources");
    let providers = daemon.get_repo_providers_internal(&RepoSelector::Path(repo.clone())).await.expect("get_repo_providers failed");

    assert_eq!(providers.path, Some(repo));
    assert!(
        providers.host_discovery.iter().any(|entry| entry.kind == "binary_available" && entry.detail.get("name") == Some(&"git".into())),
        "should include host discovery assertions"
    );
    assert!(
        providers
            .unmet_requirements
            .iter()
            .any(|req| { req.factory == "github" && req.kind == "missing_binary" && req.value.as_deref() == Some("gh") }),
        "should expose structured valued unmet requirements"
    );
    // The view now discovers checkout capabilities from resource facts rather
    // than retaining the tracked registry's historical no-checkout refusal.
    assert!(providers.providers.iter().any(|provider| provider.category == "vcs"));
}

#[tokio::test]
async fn add_repo_uses_manager_backed_local_environment_for_repo_identity() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = test_config_store(temp.path().join("config"));
    let daemon =
        InProcessDaemon::new(vec![], config, fake_discovery_with_provider_set(FakeDiscoveryProviders::new()), HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("manager-backed-repo".to_string()))).await;

    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::remote_host(
            "github.com",
            "owner",
            "manager-backed-repo",
            "origin",
        )))
        .expect("replace local environment bag");

    let outcome = daemon.add_repo(&repo).await.expect("add repo");
    let tracked_path = outcome.tracked_path;
    let resolved_from = outcome.resolved_from;

    assert_eq!(tracked_path, repo);
    assert_eq!(resolved_from, None);
    assert_eq!(
        daemon.tracked_repo_identity_for_path(&tracked_path).await,
        Some(RepoIdentity { authority: "github.com".into(), path: "owner/manager-backed-repo".into() })
    );
}

#[tokio::test]
async fn add_repo_uses_manager_backed_local_environment_for_provider_discovery() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = test_config_store(temp.path().join("config"));
    let terminal_pool: Arc<dyn TerminalPool> = Arc::new(FakeTerminalPool::new());
    let mut discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    discovery
        .factories
        .terminal_pools
        .push(Box::new(EnvGatedTerminalPoolFactory { required_env_var: "ENABLE_MANAGER_TERMINALS", pool: terminal_pool }));
    discovery.host_detectors.push(Box::new(FixedEnvVarHostDetector { key: "ENABLE_MANAGER_TERMINALS", value: "1" }));
    let daemon = InProcessDaemon::new(vec![], config, discovery, HostName::local()).await;
    install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("provider-discovery".to_string()))).await;

    daemon.add_repo(&repo).await.expect("add repo");

    let providers = daemon.get_repo_providers_internal(&RepoSelector::Path(repo.clone())).await.expect("get_repo_providers");

    assert!(
        providers
            .providers
            .iter()
            .any(|provider| { provider.category == ProviderCategory::TerminalPool.slug() && provider.name == "Managed Bag Terminals" }),
        "provider discovery should read the manager-backed local environment bag"
    );
}

#[tokio::test]
async fn selected_static_ssh_repo_discovery_uses_default_remote_host_detector_via_remote_runner() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("git", &["--version"], Ok("git version 2.43.0".into()))
            .on_run("env", &[], Ok("TERM=xterm-256color\n".into()))
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'env'",
                ],
                Ok("TERM=xterm-256color\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'rev-parse' '--is-inside-work-tree'", repo.display()).as_str(),
                ],
                Ok("true\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'rev-parse' '--path-format=absolute' '--git-dir'", repo.display()).as_str(),
                ],
                Ok("/remote/repo/.git\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'rev-parse' '--path-format=absolute' '--git-common-dir'", repo.display()).as_str(),
                ],
                Ok("/remote/repo/.git\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'rev-parse' '--abbrev-ref' '@{{upstream}}'", repo.display()).as_str(),
                ],
                Err("fatal: no upstream".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'remote'", repo.display()).as_str(),
                ],
                Ok("origin\n".into()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    format!("cd '{}' && exec 'git' 'remote' 'get-url' 'origin'", repo.display()).as_str(),
                ],
                Ok("git@github.com:owner/remote-repo.git\n".into()),
            )
            .build(),
    );

    let mut discovery = fake_discovery(false);
    discovery.runner = ssh_runner;
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(config_dir)), discovery, HostName::local()).await;

    let result = daemon
        .discover_repo_for_environment_for_test(&repo, &EnvironmentId::new("static-ssh-6275696c64626f78"))
        .await
        .expect("discover repo in remote direct environment");

    assert_eq!(
        result.host_repo_bag.repo_identity(),
        Some(RepoIdentity { authority: "github.com".into(), path: "owner/remote-repo".into() })
    );
}

#[tokio::test]
async fn provider_discovery_for_selected_static_ssh_environment_uses_its_environment_bag() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config_dir = temp.path().join("config");
    write_static_environment_config(
        &config_dir,
        r#"
[environments.buildbox]
hostname = "buildbox.example"
"#,
    );

    let ssh_runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'true'",
                ],
                Ok(String::new()),
            )
            .on_run(
                "ssh",
                &[
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/tmp/flotilla-ssh-%C",
                    "-o",
                    "ControlPersist=60",
                    "buildbox.example",
                    "sh",
                    "-lc",
                    "cd '/' && exec 'probe-env' 'ENABLE_REMOTE_TERMINALS'",
                ],
                Ok("1".into()),
            )
            .build(),
    );

    let terminal_pool: Arc<dyn TerminalPool> = Arc::new(FakeTerminalPool::new());
    let mut discovery = static_ssh_test_discovery_with_env_and_detectors(ssh_runner, Arc::new(TestEnvVars::default()), vec![Box::new(
        RunnerEchoHostDetector { probe: "ENABLE_REMOTE_TERMINALS", assertion_key: "ENABLE_REMOTE_TERMINALS" },
    )]);
    discovery
        .factories
        .terminal_pools
        .push(Box::new(EnvGatedTerminalPoolFactory { required_env_var: "ENABLE_REMOTE_TERMINALS", pool: terminal_pool }));
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(config_dir)), discovery, HostName::local()).await;

    let result = daemon
        .discover_repo_for_environment_for_test(&repo, &EnvironmentId::new("static-ssh-6275696c64626f78"))
        .await
        .expect("discover repo providers in remote direct environment");

    assert!(
        result
            .registry
            .provider_infos()
            .iter()
            .any(|(category, name)| category == ProviderCategory::TerminalPool.slug() && name == "Managed Bag Terminals"),
        "provider discovery should use the selected direct environment bag"
    );
}

#[tokio::test]
async fn cancel_nonexistent_command_returns_error() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let result = daemon.cancel(999).await;
    assert!(result.is_err(), "cancelling a non-existent command should fail");
    assert!(result.unwrap_err().contains("no matching active command"), "error should mention no matching active command");
}

#[tokio::test]
async fn handoff_uses_remote_session_origin_and_refuses_remote_only_anchor() {
    let authority_temp = tempfile::tempdir().expect("authority tempdir");
    let terminal_temp = tempfile::tempdir().expect("terminal tempdir");
    let authority =
        InProcessDaemon::new(vec![], test_config_store(authority_temp.path().join("config")), fake_discovery(false), HostName::new("feta"))
            .await;
    let terminal_host =
        InProcessDaemon::new(vec![], test_config_store(terminal_temp.path().join("config")), fake_discovery(false), HostName::new("kiwi"))
            .await;
    let snapshot = WorkflowSnapshot {
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(
                ["coder", "reviewer"]
                    .into_iter()
                    .map(|role| {
                        flotilla_resources::CrewSpec::builder()
                            .role(role.to_string())
                            .source(flotilla_resources::CrewSource::Agent {
                                selector: flotilla_resources::Selector::for_capability("coding"),
                                prompt: None,
                                brief_template: None,
                            })
                            .build()
                    })
                    .collect(),
            )
            .build()],
    };
    let initial_status = flotilla_resources::ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(snapshot),
        crew_work: BTreeMap::from([(
            "work".to_string(),
            BTreeMap::from([
                (
                    "coder".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                ),
                (
                    "reviewer".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Pending).build(),
                ),
            ]),
        )]),
        ..Default::default()
    };
    let convoys = authority.resource_backend().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("handoff-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys.update_status("handoff-convoy", &created.metadata.resource_version, &initial_status).await.expect("set crew status");
    authority
        .resource_backend()
        .using::<flotilla_resources::Vessel>("flotilla")
        .create(&InputMeta::builder().name("work-vessel".to_string()).build(), &flotilla_resources::VesselSpec {
            convoy_ref: "handoff-convoy".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "host-direct".to_string(),
            adopted_checkout_refs: Default::default(),
        })
        .await
        .expect("create vessel");
    let session_name = |role: &str, index| {
        flotilla_resources::TerminalSessionIdentity::builder()
            .vessel_ref("work-vessel".to_string())
            .convoy("handoff-convoy".to_string())
            .vessel("work".to_string())
            .role(role.to_string())
            .vessel_index(0)
            .crew_index(index)
            .labels(BTreeMap::new())
            .build()
            .name()
    };
    let sessions = terminal_host.resource_backend().using::<TerminalSession>("flotilla");
    let create_session = |role: &str, index| {
        let name = session_name(role, index);
        let labels = BTreeMap::from([
            (CONVOY_LABEL.to_string(), "handoff-convoy".to_string()),
            (VESSEL_LABEL.to_string(), "work".to_string()),
            (ROLE_LABEL.to_string(), role.to_string()),
            (flotilla_resources::VESSEL_REF_LABEL.to_string(), "work-vessel".to_string()),
        ]);
        let spec = TerminalSessionSpec::builder()
            .env_ref("remote-environment".to_string())
            .role(role.to_string())
            .source(TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    artifact_digest: None,
                    path: "brief".to_string(),
                    content: "Initial".to_string(),
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".to_string(),
                    convoy: "handoff-convoy".to_string(),
                    vessel_ref: "work-vessel".to_string(),
                }),
                message: None,
            })
            .cwd("/workspace".to_string())
            .pool("fake-terminals".to_string())
            .build();
        (name, labels, spec)
    };
    let (reviewer_name, labels, spec) = create_session("reviewer", 1);
    sessions.create(&InputMeta::builder().name(reviewer_name.clone()).labels(labels).build(), &spec).await.expect("remote reviewer");
    authority
        .resource_backend()
        .replica_writer::<TerminalSession>(terminal_host.node_id().clone(), "flotilla")
        .replace(&sessions.list().await.expect("reviewer snapshot"), chrono::Utc::now())
        .await
        .expect("replicate reviewer");
    let context = flotilla_protocol::CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("handoff-convoy".to_string()),
        vessel_ref: Some("work-vessel".to_string()),
        role: Some("coder".to_string()),
    };
    authority.crew_handoff_internal(&context, "reviewer", "Review this").await.expect("queue remote handoff");
    let convoy = convoys.get("handoff-convoy").await.expect("convoy after handoff");
    assert!(convoy.status.expect("convoy status").turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
    terminal_host
        .resource_backend()
        .replica_writer::<ResourceConvoy>(authority.node_id().clone(), "flotilla")
        .replace(&convoys.list().await.expect("convoy snapshot"), chrono::Utc::now())
        .await
        .expect("replicate handoff");
    terminal_host.reconcile_pending_supervisor_turns_once("flotilla").await.expect("deliver handoff at session origin");
    let reviewer = sessions.get(&reviewer_name).await.expect("reviewer after delivery");
    let TerminalSessionSource::Agent { message, .. } = reviewer.spec.source else { panic!("agent session expected") };
    assert!(message.expect("queued handoff").text.contains("Review this"));

    sessions.delete(&reviewer_name).await.expect("remove target session");
    let (coder_name, labels, spec) = create_session("coder", 0);
    sessions.create(&InputMeta::builder().name(coder_name).labels(labels).build(), &spec).await.expect("remote coder anchor");
    authority
        .resource_backend()
        .replica_writer::<TerminalSession>(terminal_host.node_id().clone(), "flotilla")
        .replace(&sessions.list().await.expect("coder snapshot"), chrono::Utc::now())
        .await
        .expect("replicate coder anchor");
    let current = convoys.get("handoff-convoy").await.expect("convoy before refusal");
    convoys.update_status("handoff-convoy", &current.metadata.resource_version, &initial_status).await.expect("reset crew status");
    let error = authority
        .crew_handoff_internal(&context, "reviewer", "Review again")
        .await
        .expect_err("remote anchor cannot create a local target");
    assert!(error.contains(terminal_host.node_id().as_str()), "refusal must name the anchor origin: {error}");
    assert_eq!(convoys.get("handoff-convoy").await.expect("convoy after refusal").status, Some(initial_status));
}

#[tokio::test]
async fn convoy_resume_finds_a_terminal_session_on_another_host() {
    let authority_temp = tempfile::tempdir().expect("authority tempdir");
    let terminal_temp = tempfile::tempdir().expect("terminal tempdir");
    let authority =
        InProcessDaemon::new(vec![], test_config_store(authority_temp.path().join("config")), fake_discovery(false), HostName::new("feta"))
            .await;
    let terminal_host =
        InProcessDaemon::new(vec![], test_config_store(terminal_temp.path().join("config")), fake_discovery(false), HostName::new("kiwi"))
            .await;
    let convoys = authority.resource_backend().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("split-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("wf".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status("split-convoy", &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Active,
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([(
                    "coder".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Stalled).build(),
                )]),
            )]),
            ..Default::default()
        })
        .await
        .expect("mark stalled crew work");
    let sessions = terminal_host.resource_backend().using::<TerminalSession>("flotilla");
    sessions
        .create(
            &InputMeta::builder()
                .name("split-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "split-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &TerminalSessionSpec::builder()
                .env_ref("remote-environment".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief".to_string(),
                        content: "Initial".to_string(),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "split-convoy".to_string(),
                        vessel_ref: "work-vessel".to_string(),
                    }),
                    message: None,
                })
                .cwd("/workspace".to_string())
                .pool("fake-terminals".to_string())
                .build(),
        )
        .await
        .expect("create terminal at placement host");
    authority
        .resource_backend()
        .replica_writer::<TerminalSession>(terminal_host.node_id().clone(), "flotilla")
        .replace(&sessions.list().await.expect("session snapshot"), chrono::Utc::now())
        .await
        .expect("replicate terminal session");

    let outcome = authority
        .convoy_resume_internal("flotilla", "split-convoy", "Continue", Some("work"), Some("coder"))
        .await
        .expect("resume remote crew session");
    assert!(matches!(outcome, flotilla_core::in_process::ConvoyResumeOutcome::Queued { .. }));
    let convoy = convoys.get("split-convoy").await.expect("convoy after resume");
    assert!(convoy.status.expect("convoy status").turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
    assert!(authority.resource_backend().using::<TerminalSession>("flotilla").list().await.expect("local sessions").items.is_empty());
    terminal_host
        .resource_backend()
        .replica_writer::<ResourceConvoy>(authority.node_id().clone(), "flotilla")
        .replace(&convoys.list().await.expect("convoy snapshot"), chrono::Utc::now())
        .await
        .expect("replicate pending turn");
    terminal_host.reconcile_pending_supervisor_turns_once("flotilla").await.expect("deliver at session origin");
    let session = sessions.get("split-session").await.expect("session after delivery");
    let TerminalSessionSource::Agent { message, .. } = session.spec.source else { panic!("agent session expected") };
    let message = message.expect("queued remote turn");
    assert!(message.text.contains("Continue"));
    assert!(matches!(message.sender, flotilla_resources::CrewMessageSender::OperatorResume { .. }));
}

#[tokio::test]
async fn convoy_resume_queues_a_brief_while_crew_is_working() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("busy-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Active,
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([(
                    "coder".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                )]),
            )]),
            ..Default::default()
        })
        .await
        .expect("mark crew working");

    daemon
        .convoy_resume_internal("flotilla", "busy-convoy", "Check the edge case", Some("work"), Some("coder"))
        .await
        .expect("queue brief for busy crew");

    let convoy = convoys.get("busy-convoy").await.expect("read convoy");
    let status = serde_json::to_value(convoy.status.expect("convoy status")).expect("serialize convoy status");
    assert_eq!(status["turn_deliveries"]["operator"]["pending_brief"]["content"], "Check the edge case");
    assert_eq!(status["turn_deliveries"]["operator"]["pending_brief"]["vessel"], "work");
    assert_eq!(status["turn_deliveries"]["operator"]["pending_brief"]["role"], "coder");
    assert_eq!(status["turn_deliveries"]["operator"]["pending_brief"]["sender"]["kind"], "operator-resume");

    let outcome = daemon
        .convoy_resume_internal("flotilla", "busy-convoy", "Use the newer instruction", Some("work"), Some("coder"))
        .await
        .expect("replace pending brief");
    assert_eq!(outcome, flotilla_core::in_process::ConvoyResumeOutcome::Queued { displaced: Some("Check the edge case".to_string()) });
    let convoy = convoys.get("busy-convoy").await.expect("read updated convoy");
    let status = serde_json::to_value(convoy.status.expect("convoy status")).expect("serialize convoy status");
    assert_eq!(status["turn_deliveries"]["operator"]["pending_brief"]["content"], "Use the newer instruction");

    let withdrawn = daemon.convoy_withdraw_pending_brief_internal("flotilla", "busy-convoy").await.expect("withdraw pending brief");
    assert_eq!(withdrawn.as_deref(), Some("Use the newer instruction"));
    assert!(convoys.get("busy-convoy").await.expect("read withdrawn convoy").status.expect("convoy status").pending_brief().is_none());

    apply_status_patch(&convoys, "busy-convoy", &flotilla_resources::ConvoyStatusPatch::RollUpPhase {
        phase: ConvoyPhase::Landed,
        started_at: None,
        finished_at: Some(chrono::Utc::now()),
    })
    .await
    .expect("mark convoy terminal");
    let error = daemon
        .convoy_resume_internal("flotilla", "busy-convoy", "too late", Some("work"), Some("coder"))
        .await
        .expect_err("terminal convoy should refuse a brief");
    assert!(error.contains("terminal phase `Landed`"), "unexpected refusal: {error}");
}

#[tokio::test]
async fn convoy_resume_queues_confirmed_delivery_when_working_crew_is_already_idle() {
    let terminal_pool = Arc::new(FakeTerminalPool::new());
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_terminal_pool(Arc::clone(&terminal_pool) as Arc<dyn TerminalPool>),
    );
    let (_temp, _repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    let backend = daemon.resource_backend();
    let local_host_ref = daemon.local_host_id().expect("local host identity").to_string();
    backend
        .clone()
        .using::<ResourceHost>("flotilla")
        .create(&InputMeta::builder().name(local_host_ref.clone()).build(), &HostSpec {
            display_name: daemon.host_name().to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("create local host resource");
    backend
        .clone()
        .using::<flotilla_resources::Environment>("flotilla")
        .create(&InputMeta::builder().name("idle-environment".to_string()).build(), &flotilla_resources::EnvironmentSpec {
            host_direct: Some(flotilla_resources::HostDirectEnvironmentSpec {
                host_ref: local_host_ref,
                repo_default_dir: "/workspace".to_string(),
            }),
            docker: None,
        })
        .await
        .expect("create idle crew environment");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("idle-convoy".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Active,
            crew_work: BTreeMap::from([
                (
                    "work".to_string(),
                    BTreeMap::from([(
                        "coder".to_string(),
                        flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                    )]),
                ),
                (
                    "review".to_string(),
                    BTreeMap::from([(
                        "qa".to_string(),
                        flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Done).build(),
                    )]),
                ),
            ]),
            ..Default::default()
        })
        .await
        .expect("mark crew working");
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("idle-coder-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "idle-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &TerminalSessionSpec::builder()
                .env_ref("idle-environment".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: ".flotilla/briefs/coder.md".to_string(),
                        content: "Initial turn".to_string(),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "idle-convoy".to_string(),
                        vessel_ref: "work-vessel".to_string(),
                    }),
                    message: None,
                })
                .cwd("/workspace".to_string())
                .pool("fake-terminals".to_string())
                .build(),
        )
        .await
        .expect("create idle crew session");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("idle-coder".to_string()),
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                as_of: chrono::Utc::now() - chrono::Duration::minutes(2),
                source: TerminalAttentionSource::Screen,
            }),
            ..Default::default()
        })
        .await
        .expect("observe working crew session");
    let review_session = sessions
        .create(
            &InputMeta::builder()
                .name("idle-review-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "idle-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "review".to_string()),
                    (ROLE_LABEL.to_string(), "qa".to_string()),
                ]))
                .build(),
            &TerminalSessionSpec::builder()
                .env_ref("idle-environment".to_string())
                .role("qa".to_string())
                .source(TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("review"),
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: ".flotilla/briefs/qa.md".to_string(),
                        content: "Initial turn".to_string(),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "idle-convoy".to_string(),
                        vessel_ref: "review-vessel".to_string(),
                    }),
                    message: None,
                })
                .cwd("/workspace".to_string())
                .pool("fake-terminals".to_string())
                .build(),
        )
        .await
        .expect("create review crew session");
    sessions
        .update_status(&review_session.metadata.name, &review_session.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("idle-review".to_string()),
            ..Default::default()
        })
        .await
        .expect("mark review crew session running");

    let queued = daemon
        .convoy_resume_internal("flotilla", "idle-convoy", "Finish the current turn", Some("work"), Some("coder"))
        .await
        .expect("queue brief while crew is working");
    assert_eq!(queued, flotilla_core::in_process::ConvoyResumeOutcome::Queued { displaced: None });
    let unrelated = daemon
        .convoy_resume_internal("flotilla", "idle-convoy", "Start the review", Some("review"), Some("qa"))
        .await
        .expect("resume unrelated crew");
    assert_eq!(unrelated, flotilla_core::in_process::ConvoyResumeOutcome::Queued { displaced: None });
    assert_eq!(
        convoys
            .get("idle-convoy")
            .await
            .expect("read convoy with queued brief")
            .status
            .expect("convoy status")
            .pending_brief()
            .map(|brief| brief.content.as_str()),
        Some("Finish the current turn")
    );
    apply_status_patch(&sessions, "idle-coder-session", &TerminalSessionStatusPatch::ObserveAttention {
        attention: TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: chrono::Utc::now() - chrono::Duration::minutes(1),
            source: TerminalAttentionSource::Screen,
        },
    })
    .await
    .expect("observe idle crew session");

    let outcome = daemon
        .convoy_resume_internal("flotilla", "idle-convoy", "Start the next turn", Some("work"), Some("coder"))
        .await
        .expect("deliver brief to idle crew");

    assert_eq!(outcome, flotilla_core::in_process::ConvoyResumeOutcome::Queued { displaced: Some("Finish the current turn".to_string()) });
    assert!(terminal_pool.delivered.lock().await.is_empty(), "agent messages should await reconciled delivery confirmation");
    let review_session = sessions.get("idle-review-session").await.expect("read queued review session");
    let TerminalSessionSource::Agent { message: review_message, .. } = review_session.spec.source else {
        panic!("review session should remain agent-backed")
    };
    let review_message = review_message.expect("queued review delivery");
    assert!(matches!(review_message.sender, flotilla_resources::CrewMessageSender::OperatorResume { .. }));
    assert_eq!(review_message.text, "[operator (unattributed) · via convoy resume]\n\nStart the review");
    let coder_session = sessions.get("idle-coder-session").await.expect("read queued coder session");
    let TerminalSessionSource::Agent { message: coder_message, .. } = coder_session.spec.source else {
        panic!("coder session should remain agent-backed")
    };
    assert_eq!(coder_message.expect("queued coder delivery").text, "[operator (unattributed) · via convoy resume]\n\nStart the next turn");
    let status = convoys.get("idle-convoy").await.expect("read resumed convoy").status.expect("convoy status");
    assert!(status.pending_brief().is_none());
    assert_eq!(status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["coder"].message.as_deref(), Some("Start the next turn"));
}

#[tokio::test]
async fn crew_completion_delivers_the_pending_brief_as_the_next_turn() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("turn-boundary".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Active,
            work: BTreeMap::from([("work".to_string(), flotilla_resources::WorkState::builder().phase(WorkPhase::Running).build())]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([(
                    "coder".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                )]),
            )]),
            ..Default::default()
        })
        .await
        .expect("mark crew working");
    backend
        .clone()
        .using::<flotilla_resources::Vessel>("flotilla")
        .create(&InputMeta::builder().name("work-vessel".to_string()).build(), &flotilla_resources::VesselSpec {
            convoy_ref: "turn-boundary".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "test".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("create vessel");
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    sessions
        .create(
            &InputMeta::builder().name("coder-session".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("test-env".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: ".flotilla/briefs/coder.md".to_string(),
                        content: "Initial turn".to_string(),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "turn-boundary".to_string(),
                        vessel_ref: "work-vessel".to_string(),
                    }),
                    message: None,
                })
                .cwd("/workspace".to_string())
                .pool("cleat".to_string())
                .build(),
        )
        .await
        .expect("create crew session");
    daemon
        .convoy_resume_internal("flotilla", "turn-boundary", "Begin the follow-up turn", Some("work"), Some("coder"))
        .await
        .expect("queue pending brief");

    let context = flotilla_protocol::CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("turn-boundary".to_string()),
        vessel_ref: Some("work-vessel".to_string()),
        role: Some("coder".to_string()),
    };
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: context.clone(),
                    message: Some("first turn complete".to_string()),
                    disposition: Some("satisfied".to_string()),
                    decision_ledger_ref: Some("https://example.test/pull/1#decision-ledger".to_string()),
                    force: false,
                })
                .build(),
        )
        .await
        .expect("dispatch first completion");
    assert_eq!(recv_command_finished(&mut events, command_id).await, CommandValue::CrewFollowUpDelivered);

    let convoy = convoys.get("turn-boundary").await.expect("read convoy");
    let status = convoy.status.expect("convoy status");
    assert!(status.pending_brief().is_none());
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["coder"].superseded_claims[0].disposition.as_deref(), Some("satisfied"));
    let session = sessions.get("coder-session").await.expect("read crew session");
    let TerminalSessionSource::Agent { message, .. } = session.spec.source else { panic!("crew session should be agent-backed") };
    let message = message.expect("next turn message");
    assert!(matches!(message.sender, flotilla_resources::CrewMessageSender::OperatorFollowUp { .. }));
    let message = message.text;
    assert!(message.starts_with("[operator (unattributed) · follow-up brief · reply by running `crew complete`]"), "{message}");
    assert!(message.contains("run `flotilla crew complete` again"), "{message}");
    assert!(message.ends_with("Begin the follow-up turn"), "{message}");
    assert_eq!(status.crew_work["work"]["coder"].message.as_deref(), Some("Begin the follow-up turn"));
    assert_eq!(status.crew_work["work"]["coder"].superseded_claims[0].message.as_deref(), Some("first turn complete"));
    assert_eq!(
        status.crew_work["work"]["coder"].superseded_claims[0].decision_ledger_ref.as_deref(),
        Some("https://example.test/pull/1#decision-ledger")
    );
    let explanation = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".to_string()), name: "turn-boundary".to_string() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain convoy");
    let CommandValue::ConvoyExplanation(explanation) = explanation else { panic!("expected convoy explanation") };
    assert!(explanation.decision_ledgers.iter().any(|ledger| ledger.superseded
        && ledger.comment_url.as_deref() == Some("https://example.test/pull/1#decision-ledger")
        && ledger.message.as_deref() == Some("first turn complete")));
    let second_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context,
                    message: Some("second turn complete".to_string()),
                    disposition: None,
                    decision_ledger_ref: Some("https://example.test/pull/1#second-ledger".to_string()),
                    force: false,
                })
                .build(),
        )
        .await
        .expect("dispatch second completion");
    assert_eq!(recv_command_finished(&mut events, second_id).await, CommandValue::Ok);
    let status = convoys.get("turn-boundary").await.expect("read settled convoy").status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Landing);
    assert_eq!(status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Done);
    let stall_error = daemon
        .crew_stall_internal(
            &flotilla_protocol::CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".to_string()),
                convoy: Some("turn-boundary".to_string()),
                vessel_ref: Some("work-vessel".to_string()),
                role: Some("coder".to_string()),
            },
            flotilla_protocol::StallReason::Scope,
            None,
            "settlement waits for the change request to merge".to_string(),
        )
        .await
        .expect_err("completed crew cannot stall during landing");
    assert!(stall_error.contains("already complete") && stall_error.contains("change request"), "{stall_error}");
    assert_eq!(convoys.get("turn-boundary").await.expect("read convoy after stall").status.expect("status after stall"), status);
}

#[tokio::test]
async fn convoy_explain_reports_completed_work_without_a_crew_claim() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let convoys = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("unclaimed-work".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Active,
            work: BTreeMap::from([("work".to_string(), flotilla_resources::WorkState::builder().phase(WorkPhase::Complete).build())]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([(
                    "coder".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                )]),
            )]),
            ..Default::default()
        })
        .await
        .expect("record unclaimed work");
    let result = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".to_string()), name: "unclaimed-work".to_string() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain convoy");
    let CommandValue::ConvoyExplanation(explanation) = result else { panic!("expected convoy explanation") };
    assert_eq!(explanation.unclaimed_work.len(), 1);
    assert_eq!(explanation.unclaimed_work[0].evidence, "work_complete");
    assert!(explanation.decision_ledgers.is_empty());
}

#[tokio::test]
async fn crew_completion_without_a_decision_ledger_is_refused() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("missing-ledger".to_string()).build(),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ConvoyStatus {
            phase: ConvoyPhase::Active,
            work: BTreeMap::from([("work".to_string(), flotilla_resources::WorkState::builder().phase(WorkPhase::Running).build())]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([(
                    "coder".to_string(),
                    flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                )]),
            )]),
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: flotilla_resources::interactive_single_workflow_spec().vessels,
            }),
            ..Default::default()
        })
        .await
        .expect("mark crew working");
    backend
        .clone()
        .using::<flotilla_resources::Vessel>("flotilla")
        .create(&InputMeta::builder().name("missing-ledger-vessel".to_string()).build(), &flotilla_resources::VesselSpec {
            convoy_ref: "missing-ledger".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "test".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("create vessel");

    let before = convoys.get("missing-ledger").await.expect("read convoy before refusal").status.expect("convoy status before refusal");
    let error = daemon
        .crew_complete_with_disposition_internal(
            &flotilla_protocol::CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".to_string()),
                convoy: Some("missing-ledger".to_string()),
                vessel_ref: Some("missing-ledger-vessel".to_string()),
                role: Some("coder".to_string()),
            },
            Some("done".to_string()),
            None,
            None,
        )
        .await
        .expect_err("refuse completion without a ledger");
    assert!(error.contains("decision-ledger") && error.contains(".exists"), "{error}");

    let status = convoys.get("missing-ledger").await.expect("read convoy").status.expect("convoy status");
    assert_eq!(status.phase, before.phase);
    assert_eq!(status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
    assert!(status.crew_work["work"]["coder"]
        .completion_refusal
        .as_ref()
        .is_some_and(|refusal| { refusal.consecutive_count == 1 && refusal.expectation.contains("decision-ledger") }));

    let operator = flotilla_protocol::PrincipalRef { namespace: "flotilla".to_string(), name: "operator".to_string() };
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute_for_principal(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: flotilla_protocol::CrewCommandContext {
                        crew_id: None,
                        namespace: Some("flotilla".to_string()),
                        convoy: Some("missing-ledger".to_string()),
                        vessel_ref: Some("missing-ledger-vessel".to_string()),
                        role: Some("coder".to_string()),
                    },
                    message: Some("operator accepted the ledger-less claim".to_string()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: true,
                })
                .build(),
            Some(operator.clone()),
        )
        .await
        .expect("dispatch forced completion");
    assert_eq!(recv_command_finished(&mut events, command_id).await, CommandValue::Ok);

    let forced = convoys.get("missing-ledger").await.expect("read forced convoy").status.expect("forced status");
    assert_eq!(forced.phase, ConvoyPhase::Landing);
    assert_eq!(forced.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Done);
    assert_eq!(forced.crew_work["work"]["coder"].completion_override.as_ref().map(|override_| &override_.principal), Some(&operator));

    daemon
        .crew_complete_with_disposition_internal(
            &flotilla_protocol::CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".to_string()),
                convoy: Some("missing-ledger".to_string()),
                vessel_ref: Some("missing-ledger-vessel".to_string()),
                role: Some("coder".to_string()),
            },
            Some("stray duplicate".to_string()),
            Some("must not replace the admitted claim".to_string()),
            None,
        )
        .await
        .expect("already-admitted completion is idempotent");
    let after_duplicate = convoys.get("missing-ledger").await.expect("read convoy after duplicate").status.expect("status after duplicate");
    assert_eq!(after_duplicate, forced);
}

async fn run_identity_command(daemon: &InProcessDaemon, action: CommandAction, selector: RepoSelector) -> CommandValue {
    let mut events = daemon.subscribe();
    let id = daemon.execute(Command::builder().action(action).context_repo(selector).build()).await.expect("admit identity command");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("command event") {
                if command_id == id {
                    return result;
                }
            }
        }
    })
    .await
    .expect("identity command finishes")
}

// #2500: Alias, slug and cwd address the same Repository without any tracked
// roots. Linking and forge mutations must preserve this across operation sequences.
#[hegel::test]
fn repository_operations_without_tracked_roots(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Every selector, no-checkout/observed-checkout, duplicate/empty issue lists,
    // and repeated links or closes are included in the operation space.
    let checkout = tc.draw(gs::booleans());
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let operations = (0..steps)
        .map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(2)), tc.draw(gs::integers::<usize>().min_value(0).max_value(4))))
        .collect::<Vec<_>>();
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let temp = tempfile::tempdir().expect("test config");
        let provider = Arc::new(FakeChangeRequest::new());
        provider
            .add_change_requests(vec![("55".into(), ChangeRequest {
                title: "Identity".into(),
                branch: "feature".into(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: Some("Existing body".into()),
                provider_name: "fake".into(),
                provider_display_name: "Fake".into(),
            })])
            .await;
        let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_change_request(provider.clone()));
        let daemon = InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), discovery, HostName::local()).await;
        let spec = RepositorySpec::remote("https://github.com/owner/identity-repo").expect("Repository");
        let key = spec.key();
        daemon
            .resource_backend()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
            .await
            .expect("declare Repository");
        daemon
            .resource_backend()
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("identity-project".into()).build(),
                &ProjectSpec::builder()
                    .display_name("Identity".into())
                    .default_workflow_ref("workflow".into())
                    .repositories(vec![ProjectRepositorySpec::builder().repo(key.clone()).alias("app".into()).build()])
                    .build(),
            )
            .await
            .expect("declare alias");
        let path = temp.path().join("checkout");
        if checkout {
            daemon
                .observed_resource_backend()
                .using::<ResourceCheckout>("flotilla")
                .create(
                    &InputMeta::builder().name("identity-checkout".into()).build(),
                    &ResourceCheckoutSpec::Observed(ObservedCheckoutSpec {
                        repo_ref: key.clone(),
                        host_ref: daemon.local_host_id().expect("Host").to_string(),
                        path: path.to_string_lossy().into_owned(),
                        r#ref: "main".into(),
                        is_main: true,
                    }),
                )
                .await
                .expect("checkout fact");
            install_test_repository_inspector(&daemon, Arc::new(std::sync::RwLock::new("identity-repo".into()))).await;
        }
        let mut linked = false;
        for (selector, operation) in operations {
            let selector = match selector {
                0 => RepoSelector::Query("app".into()),
                1 => RepoSelector::Query("owner/identity-repo".into()),
                _ if checkout => RepoSelector::Path(path.join("src")),
                _ => RepoSelector::Repository(key.clone()),
            };
            let response = daemon.get_repo_providers_internal(&selector).await.expect("identity provider view");
            assert_eq!(response.repository, key);
            assert_eq!(response.path, checkout.then(|| path.clone()));
            assert!(response.providers.iter().any(|provider| provider.category == ProviderCategory::ChangeRequest.slug()));
            let action = match operation {
                0 => CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".into(), issue_ids: vec!["10".into(), "10".into()] },
                1 => CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".into(), issue_ids: vec![] },
                2 => CommandAction::CloseChangeRequest { id: "55".into() },
                3 => CommandAction::Refresh { repo: Some(selector.clone()) },
                _ => CommandAction::Refresh { repo: None },
            };
            let result = run_identity_command(&daemon, action, selector).await;
            assert!(matches!(result, CommandValue::Ok | CommandValue::Refreshed { .. }), "{result:?}");
            if let CommandValue::Refreshed { repository_count, .. } = result {
                assert_eq!(repository_count, 1);
            }
            linked |= operation == 0;
            let (_, request) = provider.get_change_request("55").await.expect("request");
            assert_eq!(request.body.as_deref(), Some(if linked { "Existing body\n\nFixes #10" } else { "Existing body" }));
            assert!(daemon.tracked_repo_paths().await.is_empty());
        }
    });
}

// #2500: GitHub provider execution must select the declared Forge and Repository,
// even when its resolved transport names a Forge alias and no checkout exists.
#[tokio::test]
async fn repository_github_execution_uses_forge_identity_without_checkout() {
    use flotilla_resources::{Forge, ForgeKind, ForgeSpec};
    // Boundary double: exact gh subprocess arguments and response headers.
    let runner = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run(
                "gh",
                &["api", "--include", "repos/team/widget/pulls/55"],
                Ok(format!(
                    "HTTP/2.0 200 OK\n\n{}",
                    serde_json::json!({"number": 55, "title": "PR", "head": {"ref": "feature"}, "body": "Notes"})
                )),
            )
            .on_run("gh", &["pr", "edit", "55", "--repo", "team/widget", "--body", "Notes\n\nFixes #10"], Ok(String::new()))
            .on_run("gh", &["pr", "close", "55", "--repo", "team/widget"], Ok(String::new()))
            .build(),
    );
    let temp = tempfile::tempdir().expect("config");
    let daemon = InProcessDaemon::new(
        vec![],
        test_config_store(temp.path().join("config")),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::local(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(
            EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/bin/gh")).with(EnvironmentAssertion::remote_host(
                "github.com",
                "wrong",
                "target",
                "origin",
            )),
        )
        .expect("gh capability");
    let forge = ForgeSpec::builder()
        .forge_id("github".into())
        .kind(ForgeKind::Github)
        .hosts(BTreeSet::from(["github-alias".into(), "github.com".into()]))
        .https_url("https://github.com".into())
        .git_ssh_host("github-alias".into())
        .build();
    daemon
        .resource_backend()
        .using::<Forge>("flotilla")
        .create(&InputMeta::builder().name("github".into()).build(), &forge)
        .await
        .expect("Forge");
    let spec = RepositorySpec::remote("https://github-alias/team/widget").expect("remote").on_forge(&forge).expect("Forge identity");
    let key = spec.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("Repository");
    for action in [
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".into(), issue_ids: vec!["10".into()] },
        CommandAction::CloseChangeRequest { id: "55".into() },
    ] {
        assert_eq!(run_identity_command(&daemon, action, RepoSelector::Repository(key.clone())).await, CommandValue::Ok);
    }
    assert!(daemon.tracked_repo_paths().await.is_empty());
}

// #2500 preserves confirmation, fork policy and provider errors with no tracked root.
#[tokio::test]
async fn repository_forge_refusals_without_tracked_roots() {
    let temp = tempfile::tempdir().expect("config");
    let provider = Arc::new(FakeChangeRequest::new());
    let daemon = InProcessDaemon::new(
        vec![],
        test_config_store(temp.path().join("config")),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_change_request(provider)),
        HostName::local(),
    )
    .await;
    let spec = RepositorySpec::remote("https://github.com/owner/refusals").expect("Repository");
    let key = spec.key();
    let repositories = daemon.resource_backend().using::<Repository>("flotilla");
    let stored = repositories.create(&InputMeta::builder().name(key.to_string()).build(), &spec).await.expect("Repository");
    let cases = [
        (CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false }, "requires explicit confirmation"),
        (CommandAction::CloseChangeRequest { id: "missing".into() }, "not found"),
        (CommandAction::OpenIssue { id: "42".into() }, "no issue provider"),
    ];
    for (action, expected) in cases {
        let result = run_identity_command(&daemon, action, RepoSelector::Repository(key.clone())).await;
        assert!(matches!(result, CommandValue::Error { message } if message.contains(expected)));
    }
    let fork = spec.with_upstream("https://github.com/upstream/refusals", RepositoryRelation::Fork).expect("fork");
    repositories.update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, &fork).await.expect("fork policy");
    let result = run_identity_command(
        &daemon,
        CommandAction::MergeChangeRequest { id: "42".into(), confirmed: true },
        RepoSelector::Repository(key),
    )
    .await;
    assert!(matches!(result, CommandValue::Error { message } if message.contains("human-only")));
}

// #2500: observation seeds must publish resource context before Repository-based
// consumers run after startup, while observing a root does not mint a Project.
#[tokio::test]
async fn startup_observation_publishes_repository_context_without_project_minting() {
    let temp = tempfile::tempdir().expect("checkout fixture");
    let path = temp.path().join("repo");
    init_git_repo_with_remote(&path, "https://github.com/owner/startup");
    let daemon = InProcessDaemon::new(
        vec![path.clone()],
        test_config_store(temp.path().join("config")),
        git_process_discovery(false),
        HostName::local(),
    )
    .await;
    let response = daemon.get_repo_providers_internal(&RepoSelector::Path(path.join("src"))).await.expect("startup cwd context");
    assert_eq!(response.repository, RepositorySpec::remote("https://github.com/owner/startup").expect("Repository").key());
    let tracked_identity = daemon.tracked_repo_identity_for_path(&path).await.expect("repo-info identity");
    let mut events = daemon.subscribe();
    let _ = run_identity_command(
        &daemon,
        CommandAction::Refresh { repo: Some(RepoSelector::Repository(response.repository.clone())) },
        RepoSelector::Repository(response.repository.clone()),
    )
    .await;
    let mut matched = false;
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::CommandStarted { repo_identity, .. } = event {
            assert_eq!(repo_identity, tracked_identity);
            matched = true;
        }
    }
    assert!(matched, "refresh publishes the repo-info event identity");
    assert_eq!(response.path, Some(path));
    assert!(daemon.resource_backend().using::<Project>("flotilla").list().await.expect("Projects").items.is_empty());
}

// Background discovery failure must not turn checkout-less refresh into a
// command failure; the explicit strict CLI refresh still reports it.
#[tokio::test]
async fn checkoutless_refresh_honors_failure_policy() {
    use flotilla_resources::{ForgeKind, ForgeSpec};
    let temp = tempfile::tempdir().expect("config");
    let daemon =
        InProcessDaemon::new(vec![], test_config_store(temp.path().join("config")), fake_discovery(false), HostName::local()).await;
    let forge = ForgeSpec::builder()
        .forge_id("unavailable".into())
        .kind(ForgeKind::Github)
        .hosts(BTreeSet::from(["github.com".into()]))
        .https_url("https://github.com".into())
        .git_ssh_host("github.com".into())
        .build();
    let spec = RepositorySpec::remote("https://github.com/team/repo").expect("remote").on_forge(&forge).expect("identity");
    let key = spec.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("Repository");
    let selector = RepoSelector::Repository(key);
    assert_eq!(daemon.refresh(&selector).await.expect("best effort"), None);
    assert!(daemon.refresh_strict(&selector).await.is_err(), "strict refresh reports the missing Forge");
}
