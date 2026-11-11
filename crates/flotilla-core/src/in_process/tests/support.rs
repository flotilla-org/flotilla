//! Shared scenario fixtures and stand-ins for credential-controller and forge I/O.

use flotilla_store_memory::InMemoryBackend;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_change_request::ChangeRequestTracker;
use flotilla_config::ConfigStore;
use flotilla_discovery_api::provider_config;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_protocol::HostName;
use flotilla_resources::{
    terminal_records::{TerminalLaunchRequest, TerminalLaunchSource, TerminalRecords},
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CredentialExpiry, CrewWorkPhase, EnvironmentSpec as ResourceEnvironmentSpec,
    FulfilmentFacts, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, Host as ResourceHost,
    HostDirectEnvironmentSpec, HostSpec, HostStatus, InputMeta, ResourceObject, TerminalAttention,
    TerminalSession as ResourceTerminalSession, VesselRequirement, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY, GENERATION_LABEL,
    PROJECT_LABEL, ROLE_LABEL,
};
use flotilla_store::ResourceBackend;

use crate::in_process::dispatch_board_tests::tests::board;
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;
use flotilla_crew::ports::WorkCredentialReconciler;
use flotilla_issue_tracker::IssueProvider;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;

pub(super) struct RecordingWorkCredentials {
    pub(super) backend: ResourceBackend,
    pub(super) delivered: tokio::sync::Mutex<BTreeSet<String>>,
    pub(super) fail_next: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl WorkCredentialReconciler for RecordingWorkCredentials {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        assert_eq!(environment_ref, "credential-env");
        let convoy =
            self.backend.clone().using::<ResourceConvoy>(namespace).get("turn-credential-work").await.map_err(|error| error.to_string())?;
        let status = convoy.status.ok_or_else(|| "missing convoy status".to_string())?;
        if status.phase != flotilla_resources::ConvoyPhase::Active
            || status.work.get("work").is_none_or(|work| work.phase != flotilla_resources::WorkPhase::Running)
            || status.crew_work.get("work").and_then(|crew| crew.get("coder")).is_none_or(|crew| crew.phase != CrewWorkPhase::Working)
        {
            return Err("credentials reconciled before work reopened".to_string());
        }
        let refs = status
            .workflow_snapshot
            .ok_or_else(|| "missing workflow snapshot".to_string())?
            .vessels
            .into_iter()
            .find(|vessel| vessel.name == "work")
            .ok_or_else(|| "missing work vessel".to_string())?
            .credential_refs;
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("credential staging failed".to_string());
        }
        self.delivered.lock().await.extend(refs);
        Ok(())
    }
}

pub(super) struct SessionStagingProbe {
    pub(super) backend: ResourceBackend,
    /// The launch of `resume-staging` the session works for.
    pub(super) convoy: String,
    pub(super) session: String,
    pub(super) environment: String,
    pub(super) fail_next: std::sync::atomic::AtomicBool,
    pub(super) staged: std::sync::atomic::AtomicUsize,
    /// The engine whose launches the daemon reads: the session's launches are stepped here.
    pub(super) engine: TestEngine,
}

#[async_trait]
impl WorkCredentialReconciler for SessionStagingProbe {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        assert_eq!(environment_ref, self.environment);
        self.backend.clone().using::<ResourceTerminalSession>(namespace).get(&self.session).await.map_err(|error| error.to_string())?;
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("credential staging failed".to_string());
        }
        self.staged.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

pub(super) async fn resume_staging_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    resume_staging_fixture_with_backend(ResourceBackend::new(InMemoryBackend::default())).await
}

pub(super) async fn resume_staging_fixture_with_backend(
    backend: ResourceBackend,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    resume_staging_fixture_with_clock(backend, Arc::new(flotilla_resources::SystemClock)).await
}

pub(super) async fn resume_staging_fixture_with_clock(
    backend: ResourceBackend,
    clock: Arc<dyn flotilla_resources::Clock>,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"resume-staging-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        clock,
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let engine = test_engine(&daemon);
    // Its coder owes a decision ledger, kept by an operator's verdict: the convoy is Landing,
    // its work Complete and its crew Done.
    let mut coder = claim_crew("coder");
    coder.promises = vec![flotilla_resources::TemplatePromise::decision_ledger("coder")];
    let launch = admit(
        &daemon,
        "resume-staging",
        ConvoySpec::builder().workflow_ref("implement-review".to_string()).role("resume-staging".to_string()).build(),
        ConvoyStatus {
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![coder]).build()],
            }),
            ..Default::default()
        },
    )
    .await;
    let convoy = backend.clone().using::<ResourceConvoy>("flotilla").get(&launch).await.expect("convoy");
    let scope = flotilla_resources::convoy::engine::VerdictScope { vessel: "work".to_string(), role: Some("coder".to_string()) };
    convoy_engine(&daemon).keep_by_verdict(&convoy, &scope, "operator", "work done").await.expect("work complete");
    let vessel = ensure_vessel(&engine, &launch, "work");
    engine.terminals.request(&TerminalLaunchRequest {
        session: "resume-staging-session".to_string(),
        environment: "resume-env".to_string(),
        vessel: Some(vessel),
        role: "coder".to_string(),
        pool: "passthrough".to_string(),
        cwd: "/repo".to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "code".to_string(),
            adapter: None,
            model: None,
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: "brief.md".to_string(),
                content: "original".to_string(),
                copies: vec![],
            },
        },
    });
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        convoy: launch,
        session: "resume-staging-session".to_string(),
        environment: "resume-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        staged: std::sync::atomic::AtomicUsize::new(0),
        engine,
    });
    daemon.set_work_credential_reconciler(probe.clone()).await;
    (daemon, backend, probe)
}

/// The launch serving `work` of convoy `convoy`: what a crew's sessions are held for, whose
/// convoy and work their readers see. A live launch is reused.
pub(in crate::in_process) fn ensure_vessel(engine: &flotilla_provisioning_testkit::EngineFixture, convoy: &str, work: &str) -> String {
    engine.vessels.request(convoy, work, "local")
}

/// Records `attention` as what this home observes of launch `launch`, as its hooks or screen reads do.
pub(in crate::in_process) fn observe_attention(
    engine: &flotilla_provisioning_testkit::EngineFixture,
    launch: &str,
    attention: TerminalAttention,
) {
    engine.terminals.terminals.observations().update(launch, |observation| observation.attention = Some(attention));
}

pub(super) struct ForgeAwareTestChangeRequestFactory(pub(super) Arc<dyn ChangeRequestTracker>);

#[async_trait]
impl Factory for ForgeAwareTestChangeRequestFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> Self::Descriptor {
        ProviderDescriptor::named(ProviderCategory::ChangeRequest, "test-forgejo")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn provider_config::ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        assert_eq!(env.find_origin_forge().map(|forge| forge.kind), Some(flotilla_resources::ForgeKind::Forgejo));
        assert!(env.find_auth_path("forgejo").is_some());
        Ok(Arc::clone(&self.0))
    }
}

pub(super) struct BatchedObservationRunner {
    pub(super) calls: std::sync::Mutex<Vec<String>>,
    pub(super) rate_limit_two: std::sync::atomic::AtomicBool,
    pub(super) hard_error_one: std::sync::atomic::AtomicBool,
    pub(super) rate_limit_all: std::sync::atomic::AtomicBool,
    pub(super) mixed_history_errors: std::sync::atomic::AtomicBool,
    pub(super) block_one: std::sync::atomic::AtomicBool,
    pub(super) one_started: tokio::sync::Notify,
    pub(super) release_one: tokio::sync::Notify,
    pub(super) conflicting: std::sync::atomic::AtomicBool,
    pub(super) merged: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl CommandRunner for BatchedObservationRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        match (cmd, args) {
            ("gh", ["--version"]) => Ok("gh version 2.49.0\n".into()),
            ("git", ["--version"]) => Ok("git version 2.43.0\n".into()),
            _ => Err(format!("unexpected command: {cmd} {args:?}")),
        }
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        if cmd == "gh" && args.first() == Some(&"api") && args.get(1) == Some(&"--include") {
            let endpoint = args.get(2).ok_or("missing REST endpoint")?;
            let number = endpoint.rsplit('/').next().ok_or("missing PR number")?.parse::<u64>().map_err(|e| e.to_string())?;
            let flags = [
                self.rate_limit_two.load(Ordering::SeqCst),
                self.hard_error_one.load(Ordering::SeqCst),
                self.rate_limit_all.load(Ordering::SeqCst),
                self.mixed_history_errors.load(Ordering::SeqCst),
                self.conflicting.load(Ordering::SeqCst),
                self.merged.load(Ordering::SeqCst),
            ];
            let etag = format!("{flags:?}");
            let unchanged = args.iter().any(|arg| *arg == format!("If-None-Match: {etag}"));
            return Ok(CommandOutput {
                stdout: if unchanged {
                    "HTTP/2 304 Not Modified\r\n\r\n".into()
                } else {
                    format!(
                        "HTTP/2 200 OK\r\nETag: {etag}\r\n\r\n{}",
                        serde_json::json!({"number":number,"updated_at":"2026-10-07T00:00:00Z"})
                    )
                },
                stderr: String::new(),
                exit_code: Some(0),
            });
        }
        if cmd != "gh" || args.first() != Some(&"api") || args.get(1) != Some(&"graphql") {
            return self.run(cmd, args, cwd, label).await.map(|stdout| flotilla_provider_runtime::CommandOutput {
                stdout,
                stderr: String::new(),
                exit_code: Some(0),
            });
        }
        let query = args.iter().find_map(|arg| arg.strip_prefix("query=")).ok_or("missing GraphQL query")?;
        self.calls.lock().expect("calls").push(query.to_string());
        if query.contains("name:\"one\"") && self.block_one.load(std::sync::atomic::Ordering::SeqCst) {
            self.one_started.notify_one();
            self.release_one.notified().await;
        }
        // This fake stands in for the GitHub subprocess/network boundary.
        if self.mixed_history_errors.load(std::sync::atomic::Ordering::SeqCst) && query.contains("before:") {
            let limited = query.contains("number:2201)");
            return Ok(flotilla_provider_runtime::CommandOutput {
                stdout: if limited {
                    "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 4989\r\nRetry-After: 60\r\n\r\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"secondary rate limit\"}]}".into()
                } else {
                    "HTTP/2 200 OK\r\n\r\n{\"errors\":[{\"type\":\"FORBIDDEN\",\"message\":\"history access denied\"}]}".into()
                },
                stderr: String::new(),
                exit_code: Some(if !limited { 0 } else { 1 }),
            });
        }
        if self.rate_limit_all.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(flotilla_provider_runtime::CommandOutput {
                stdout: "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1893456000\r\nRetry-After: 60\r\n\r\n{\"message\":\"You have exceeded a secondary rate limit\"}".into(),
                stderr: String::new(), exit_code: Some(1),
            });
        }
        if query.contains("name:\"one\"") && self.hard_error_one.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("rate limited diagnostics unavailable: access denied".into());
        }
        if query.contains("name:\"two\"") && self.rate_limit_two.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(flotilla_provider_runtime::CommandOutput {
                stdout: "HTTP/2 403 Forbidden\r\nX-RateLimit-Reset: 1893456000\r\nX-RateLimit-Remaining: 0\r\n\r\n{\"message\":\"API rate limit exceeded\"}".into(),
                stderr: "gh: API rate limit exceeded".into(),
                exit_code: Some(1),
            });
        }
        let requests = query
            .split("pullRequest(number:")
            .skip(1)
            .filter_map(|tail| {
                let number = tail.split(')').next()?.parse::<u64>().ok()?;
                Some((
                    format!("pr{number}"),
                    serde_json::json!({
                        "state": if self.merged.load(Ordering::SeqCst) { "MERGED" } else { "OPEN" }, "isDraft": false, "headRefOid": "abc", "reviewDecision": null,
                        "mergeable": if self.conflicting.load(std::sync::atomic::Ordering::SeqCst) { "CONFLICTING" } else { "MERGEABLE" },
                        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": []}}}}]},
                        "comments": {"nodes": [], "pageInfo": {
                            "hasPreviousPage": self.mixed_history_errors.load(std::sync::atomic::Ordering::SeqCst),
                            "startCursor": "older",
                        }}
                    }),
                ))
            })
            .collect::<serde_json::Map<_, _>>();
        Ok(flotilla_provider_runtime::CommandOutput {
            stdout: format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": requests}})),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

/// Old Convoy `record` in a plain store, generation 1 of standing `role` in `project`: what a
/// store without the engine's Convoy path holds.
pub(in crate::in_process) async fn create_identity_record(backend: &ResourceBackend, record: &str, role: &str, project: Option<&str>) {
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), project.unwrap_or_default().to_string()),
        (ROLE_LABEL.to_string(), role.to_string()),
        (GENERATION_LABEL.to_string(), "1".to_string()),
    ]);
    let mut spec = ConvoySpec::builder().workflow_ref("review".to_string()).role(role.to_string()).generation(1).build();
    spec.project_ref = project.map(str::to_string);
    backend
        .clone()
        .using::<ResourceConvoy>("flotilla")
        .create(&InputMeta::builder().name(record.to_string()).labels(labels).build(), &spec)
        .await
        .expect("convoy");
}

/// Admits a launch of `record`, generation 1 of standing `role` in `project`, on `daemon`'s
/// engine, its work crewed by an agent for `role`. Returns the launch's name.
pub(in crate::in_process) async fn create_identity_convoy(
    daemon: &InProcessDaemon,
    record: &str,
    role: &str,
    project: Option<&str>,
) -> String {
    let mut spec = ConvoySpec::builder().workflow_ref("review".to_string()).role(role.to_string()).generation(1).build();
    spec.project_ref = project.map(str::to_string);
    identity_launch(daemon, record, spec).await
}

/// Admits a launch of `record` with identity `spec` on `daemon`'s engine, labelled as admission
/// labels a standing role's generation; see [`create_identity_convoy`].
pub(in crate::in_process) async fn identity_launch(daemon: &InProcessDaemon, record: &str, spec: ConvoySpec) -> String {
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), spec.project_ref.clone().unwrap_or_default()),
        (ROLE_LABEL.to_string(), spec.role.clone()),
        (GENERATION_LABEL.to_string(), spec.generation.to_string()),
    ]);
    let status = flotilla_engine_convoys_testkit::single_agent(&spec.role);
    let mut convoy = flotilla_engine_convoys_testkit::convoy("flotilla", record, spec, status);
    convoy.metadata.labels = labels;
    admit_convoy(daemon, convoy).await
}

pub(in crate::in_process) fn test_meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

/// Capacity a placement host publishes so the admission free-space floor passes.
const PLACEMENT_DISK_FREE_BYTES: u64 = 100 * 1024 * 1024 * 1024;
const PLACEMENT_FREE_SPACE_FLOOR_BYTES: u64 = 20 * 1024 * 1024 * 1024;

pub(super) fn host_direct_kind_spec(host_ref: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder()
        .host_ref(host_ref.to_string())
        .pool("passthrough".to_string())
        .grants(BTreeSet::from([
            FulfilmentGrant::platform("linux".to_string()),
            FulfilmentGrant::host_account_reach(),
            FulfilmentGrant::network("host".to_string()),
        ]))
        .realisation(FulfilmentRealisation::HostDirect)
        .build()
}

pub(super) async fn host_direct_kind(backend: &ResourceBackend, name: &str, host_ref: &str) -> ResourceObject<FulfilmentKind> {
    backend.using::<FulfilmentKind>("flotilla").create(&test_meta(name), &host_direct_kind_spec(host_ref)).await.expect("host-direct kind")
}

/// Where a test writes Hosts: a plain store holds any, and a daemon's store only those homed at
/// that daemon (ADR 0064); another host's are written at its own home and replicated in.
#[derive(Clone, Copy)]
pub(super) enum HostsAt<'a> {
    Store(&'a ResourceBackend),
    Daemon(&'a InProcessDaemon),
}

impl<'a> From<&'a ResourceBackend> for HostsAt<'a> {
    fn from(backend: &'a ResourceBackend) -> Self {
        Self::Store(backend)
    }
}

impl<'a> From<&'a InProcessDaemon> for HostsAt<'a> {
    fn from(daemon: &'a InProcessDaemon) -> Self {
        Self::Daemon(daemon)
    }
}

impl<'a> From<&'a Arc<InProcessDaemon>> for HostsAt<'a> {
    fn from(daemon: &'a Arc<InProcessDaemon>) -> Self {
        Self::Daemon(daemon)
    }
}

impl HostsAt<'_> {
    pub(super) fn backend(self) -> ResourceBackend {
        match self {
            Self::Store(backend) => backend.clone(),
            Self::Daemon(daemon) => daemon.resource_backend(),
        }
    }

    /// Whether Host `name` with `spec` is written through [`Self::backend`].
    fn homes(self, name: &str, spec: &HostSpec) -> bool {
        let Self::Daemon(daemon) = self else { return true };
        let home_host = match &spec.connection {
            flotilla_resources::HostConnection::Daemon => name,
            flotilla_resources::HostConnection::SshHost { owning_daemon, .. } => owning_daemon.as_str(),
        };
        daemon.local_host_id().is_some_and(|own| own.to_string() == home_host)
    }
}

/// Writes Host `name`, with `status` as its home observes it, where it is homed.
pub(super) async fn put_host<'a>(at: impl Into<HostsAt<'a>>, name: &str, spec: &HostSpec, status: &HostStatus) {
    let at = at.into();
    if !at.homes(name, spec) {
        let HostsAt::Daemon(daemon) = at else { unreachable!("a plain store homes every host") };
        let target = daemon.decided_store().expect("decided store");
        flotilla_provisioning_testkit::put_host_at_its_home(&target, name, spec, Some(status)).await;
        return;
    }
    let hosts = at.backend().using::<ResourceHost>("flotilla");
    let version = match hosts.get(name).await {
        Ok(held) => held.metadata.resource_version,
        Err(_) => hosts.create(&test_meta(name), spec).await.expect("host create").metadata.resource_version,
    };
    hosts.update_status(name, &version, status).await.expect("host status update");
}

/// Host `name` as read where it is held, from whichever home wrote it.
pub(super) async fn read_host<'a>(at: impl Into<HostsAt<'a>>, name: &str) -> ResourceObject<ResourceHost> {
    at.into().backend().including_replicas::<ResourceHost>("flotilla").get(name).await.expect("host").object
}

pub(super) async fn create_host_direct_placement<'a>(
    at: impl Into<HostsAt<'a>>,
    kind_name: &str,
    host_ref: &str,
    agent_adapters: BTreeSet<String>,
) {
    let at = at.into();
    create_host_direct_host(at, host_ref, agent_adapters).await;
    host_direct_kind(&at.backend(), kind_name, host_ref).await;
}

/// A ready host for host-direct placements, offering `agent_adapters`.
pub(super) async fn create_host_direct_host<'a>(at: impl Into<HostsAt<'a>>, host_ref: &str, agent_adapters: BTreeSet<String>) {
    put_host(
        at,
        host_ref,
        &HostSpec { display_name: host_ref.to_string(), connection: Default::default(), ..HostSpec::default() },
        &HostStatus {
            capabilities: [
                (
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                ),
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(agent_adapters)),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            disk_free_bytes: Some(PLACEMENT_DISK_FREE_BYTES),
            admission_free_space_floor_bytes: Some(PLACEMENT_FREE_SPACE_FLOOR_BYTES),
            ..HostStatus::default()
        },
    )
    .await;
}

pub(super) fn trusted_codex_workflow() -> WorkflowTemplateSpec {
    flotilla_resources::single_agent_workflow_spec()
}

/// The engine a test daemon reads: its environments, clones, checkouts, vessels and terminal
/// sessions on the daemon's decided store, as the daemon's runtime has them.
pub(in crate::in_process) type TestEngine = Arc<flotilla_provisioning_testkit::EngineFixture>;

/// The engines installed into test daemons, by the decided store they are on.
static TEST_ENGINES: std::sync::LazyLock<std::sync::Mutex<Vec<(std::sync::Weak<flotilla_schema_sqlite::stream::Service>, TestEngine)>>> =
    std::sync::LazyLock::new(Default::default);

/// [`TestEngine`] over `daemon`'s decided store, installed as its lifecycle reads on first use
/// and the same engine after, as a composed daemon has one. Its readers and watchers follow the
/// store when a runtime is running, as the runtime's do.
pub(in crate::in_process) fn test_engine(daemon: &InProcessDaemon) -> TestEngine {
    let service = daemon.decided_store().expect("decided store");
    let mut engines = TEST_ENGINES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    engines.retain(|(store, _)| store.strong_count() > 0);
    if let Some((_, engine)) = engines.iter().find(|(store, _)| store.as_ptr() == Arc::as_ptr(&service)) {
        return Arc::clone(engine);
    }
    let engine = Arc::new(flotilla_provisioning_testkit::EngineFixture::over_service(&service));
    if tokio::runtime::Handle::try_current().is_ok() {
        drop(engine.terminals.terminals.follow(Arc::clone(&service)).expect("follow the terminal sessions"));
        drop(engine.vessels.vessels.follow(Arc::clone(&service)).expect("follow the vessels"));
        drop(engine.environments.environments.follow(Arc::clone(&service)).expect("follow the environments"));
        drop(engine.checkouts.checkouts.follow(Arc::clone(&service)).expect("follow the checkouts"));
    }
    daemon.install_lifecycle_reads(engine.reads()).expect("engine reads");
    engines.push((Arc::downgrade(&service), Arc::clone(&engine)));
    engine
}

/// Admits `convoy` on `daemon`'s engine Convoy path as the next launch of what it is a launch of,
/// as admission does, with its status's workflow snapshot frozen. Returns the launch's name.
pub(in crate::in_process) async fn admit_convoy(daemon: &InProcessDaemon, convoy: ResourceObject<ResourceConvoy>) -> String {
    test_engine(daemon);
    flotilla_engine_convoys_testkit::launch(&daemon.convoy_engine_path(), convoy).await
}

/// `daemon`'s engine Convoy writer, for the edges a test drives: verdicts, links, failure.
pub(in crate::in_process) fn convoy_engine(daemon: &InProcessDaemon) -> Arc<dyn flotilla_resources::convoy::engine::EngineConvoys> {
    test_engine(daemon);
    Arc::clone(daemon.convoy_engine_path().get().expect("the Convoy path is installed"))
}

/// Admits `convoy` with `spec` and `status` in namespace `flotilla` on `daemon`; see [`admit_convoy`].
pub(in crate::in_process) async fn admit(daemon: &InProcessDaemon, convoy: &str, spec: ConvoySpec, status: ConvoyStatus) -> String {
    admit_convoy(daemon, flotilla_engine_convoys_testkit::convoy("flotilla", convoy, spec, status)).await
}

/// An engine not installed into any daemon: each fixture's `reads` serve all of them.
pub(in crate::in_process) fn engine_fixture() -> flotilla_provisioning_testkit::EngineFixture {
    flotilla_provisioning_testkit::EngineFixture::default()
}

/// `name`, a ready host-direct environment of `host_ref` on the engine.
pub(in crate::in_process) fn create_test_environment(
    environments: &flotilla_provisioning_testkit::EnvironmentFixture,
    name: &str,
    host_ref: &str,
) -> String {
    let spec = ResourceEnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_ref.to_string(), repo_default_dir: "/tmp".to_string() }),
        ..Default::default()
    };
    environments.put_ready(name, &spec, None, "/tmp", Default::default());
    name.to_string()
}

/// Session `name`, a tool running in environment `env_ref` for convoy `convoy`'s `work` vessel
/// as `role`, under pool session `session-<name>`. Its launch.
pub(in crate::in_process) async fn create_running_session(
    engine: &flotilla_provisioning_testkit::EngineFixture,
    env_ref: &str,
    name: &str,
    convoy: &str,
    role: &str,
) -> String {
    let vessel_ref = ensure_vessel(engine, convoy, "work");
    let launch = engine.terminals.request(&TerminalLaunchRequest {
        session: name.to_string(),
        environment: env_ref.to_string(),
        vessel: Some(vessel_ref),
        role: role.to_string(),
        pool: "passthrough".to_string(),
        cwd: "/repo".to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Tool { command: "bash".to_string() },
    });
    engine.terminals.ready(&launch, &format!("session-{name}"), &Default::default());
    launch
}

/// A ready Docker host and a literal-image Docker kind on it whose image the
/// host probed for the Codex harness.
pub(super) async fn create_docker_placement<'a>(
    at: impl Into<HostsAt<'a>>,
    kind_name: &str,
    host_ref: &str,
    held_credentials: BTreeSet<String>,
) {
    let at = at.into();
    put_host(
        at,
        host_ref,
        &HostSpec { display_name: host_ref.to_string(), connection: Default::default(), ..HostSpec::default() },
        &HostStatus {
            capabilities: [
                (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(held_credentials)),
                (
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                ),
                ("docker".to_string(), serde_json::json!(true)),
                ("os".to_string(), serde_json::json!("linux")),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            disk_free_bytes: Some(PLACEMENT_DISK_FREE_BYTES),
            admission_free_space_floor_bytes: Some(PLACEMENT_FREE_SPACE_FLOOR_BYTES),
            fulfilment_facts: BTreeMap::from([(
                kind_name.to_string(),
                FulfilmentFacts {
                    harnesses: BTreeMap::from([(
                        "codex".to_string(),
                        HarnessFacts { version: "0.160.0".to_string(), models: BTreeMap::new() },
                    )]),
                    observed_at: Utc::now(),
                    ..Default::default()
                },
            )]),
            ..HostStatus::default()
        },
    )
    .await;
    at.backend()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta(kind_name),
            &FulfilmentKindSpec::builder()
                .host_ref(host_ref.to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::platform("linux".to_string()), FulfilmentGrant::network("scoped".to_string())]))
                .realisation(FulfilmentRealisation::DockerPerVessel {
                    image: Some(flotilla_resources::PlacementImageSource::Literal("crew:latest".to_string())),
                })
                .build(),
        )
        .await
        .expect("Docker kind");
}

pub(super) async fn set_host_credential_expiry<'a>(at: impl Into<HostsAt<'a>>, host_ref: &str, expiry: BTreeMap<String, CredentialExpiry>) {
    let at = at.into();
    let host = read_host(at, host_ref).await;
    let mut status = host.status.expect("host status");
    status.capabilities.insert(flotilla_resources::CREDENTIAL_EXPIRY_CAPABILITY.to_string(), serde_json::json!(expiry));
    put_host(at, host_ref, &host.spec, &status).await;
}

pub(super) type StallTestDaemon = (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir, tokio::task::JoinHandle<()>, TestEngine);

pub(super) async fn stall_test_daemon() -> StallTestDaemon {
    stall_test_daemon_with_clock(Arc::new(flotilla_resources::SystemClock)).await
}

pub(super) async fn stall_test_daemon_with_clock(clock: Arc<dyn flotilla_resources::Clock>) -> StallTestDaemon {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"stall-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        clock,
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let engine = test_engine(&daemon);
    let (sender, mut receiver) = flotilla_store::controller::WorkQueueSender::channel();
    let watch = daemon.reconciler_wake_watch();
    let watch_backend = backend.clone();
    let task = tokio::spawn(async move {
        let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        watch.spawn(watch_backend, "flotilla".to_string(), sender).await.expect("stall watch");
        drain.abort();
    });
    (daemon, backend, temp, task, engine)
}

/// The daemon's AgentSessions, where its harness hooks and screen reads record a crew's activity
/// beside the TerminalSession's attention; stall judgement reads them (#3108).
pub(super) fn agent_sessions(daemon: &InProcessDaemon) -> Arc<flotilla_engine_messages::Sessions> {
    let service = daemon.decided_store().expect("decided store");
    let primitives = flotilla_engine_messages::MessagePrimitives::open(
        daemon.node_id().as_str(),
        service,
        daemon.resource_backend(),
        daemon.clock_for_scenarios(),
    )
    .expect("open the AgentSessions");
    Arc::clone(primitives.sessions())
}

/// `at` as the engine records it.
pub(super) fn engine_at(at: chrono::DateTime<Utc>) -> flotilla_schema::Timestamp {
    flotilla_schema::Timestamp::from_datetime(&at)
}

pub(super) async fn wait_for_stall(backend: &ResourceBackend, name: &str, expected: bool) -> ConvoyStatus {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let status = backend.clone().using::<ResourceConvoy>("flotilla").get(name).await.expect("convoy").status.expect("status");
            if status.stalled.is_some() == expected {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("stall judgement timed out")
}

pub(super) fn stall_workflow_snapshot(crew: Vec<flotilla_resources::CrewSpec>) -> flotilla_resources::WorkflowSnapshot {
    flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![flotilla_resources::VesselRequirement::builder().name("work".into()).crew(crew).build()],
    }
}

pub(super) fn claim_crew(role: &str) -> flotilla_resources::CrewSpec {
    flotilla_resources::CrewSpec::builder()
        .role(role.to_string())
        .source(flotilla_resources::CrewSource::Tool { command: "test".into() })
        .completion_conditions(vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
            role,
            "decision-ledger",
            flotilla_resources::ArtifactSubjectBinding::Convoy,
        )])
        .build()
}

/// Session `name`, a tool running for convoy `convoy`'s `work` vessel as `role`, whose home
/// observes `attention`. Its launch.
pub(super) async fn stall_test_session(
    engine: &flotilla_provisioning_testkit::EngineFixture,
    convoy: &str,
    name: &str,
    role: &str,
    attention: TerminalAttention,
) -> String {
    let vessel_ref = ensure_vessel(engine, convoy, "work");
    let launch = engine.terminals.request(&TerminalLaunchRequest {
        session: name.into(),
        environment: "env".into(),
        vessel: Some(vessel_ref),
        role: role.into(),
        pool: "test".into(),
        cwd: "/tmp".into(),
        env: Default::default(),
        source: TerminalLaunchSource::Tool { command: "test".into() },
    });
    engine.terminals.ready(&launch, &format!("session-{name}"), &Default::default());
    observe_attention(engine, &launch, attention);
    launch
}

pub(super) struct SuspendedBoardProvider {
    pub(super) calls: AtomicUsize,
    pub(super) issue_calls: AtomicUsize,
    pub(super) release: tokio::sync::Semaphore,
}

impl SuspendedBoardProvider {
    fn issue(source: &flotilla_protocol::IssueSource, id: &str) -> flotilla_protocol::Issue {
        flotilla_protocol::Issue::builder()
            .reference(flotilla_protocol::IssueRef { source: source.clone(), id: id.into() })
            .title("Shared issue".into())
            .labels(vec!["ready".into()])
            .state(flotilla_protocol::IssueState::Open)
            .as_of(Utc::now())
            .provider_name("test".into())
            .provider_display_name("Test".into())
            .build()
    }
}

#[async_trait]
impl IssueProvider for SuspendedBoardProvider {
    fn board_includes_mission_fields(&self) -> bool {
        true
    }
    async fn mission_fields(&self, _: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::MissionFields, String> {
        panic!("complete bulk boards must not cause per-ticket field requests")
    }

    fn supports(&self, _: &flotilla_protocol::IssueSource) -> bool {
        true
    }
    async fn query(
        &self,
        source: &flotilla_protocol::IssueSource,
        _: &flotilla_protocol::issue_query::IssueQuery,
        _: u32,
        _: usize,
    ) -> Result<flotilla_protocol::issue_query::IssueResultPage, String> {
        self.issue_calls.fetch_add(1, Ordering::SeqCst);
        Ok(flotilla_protocol::issue_query::IssueResultPage { items: vec![Self::issue(source, "1")], total: Some(1), has_more: false })
    }
    async fn fetch_by_id(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        self.issue_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Self::issue(&reference.source, &reference.id))
    }
    async fn list_changed_since(
        &self,
        source: &flotilla_protocol::IssueSource,
        _: &str,
        _: usize,
    ) -> Result<flotilla_protocol::IssueChangeset, String> {
        self.issue_calls.fetch_add(1, Ordering::SeqCst);
        Ok(flotilla_protocol::IssueChangeset { updated: vec![Self::issue(source, "1")], closed: vec![], has_more: false })
    }
    async fn open_in_browser(&self, _: &flotilla_protocol::IssueRef) -> Result<(), String> {
        unreachable!("no browser")
    }
    async fn dispatch_board(&self, source: &flotilla_protocol::IssueSource) -> Result<flotilla_protocol::DispatchBoardRepository, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.release.acquire().await.expect("release observation").forget();
        Ok(board(source, 400))
    }
}

// #2842: realistic Project status and 400 serving convoys must not turn a
// board read into per-issue forge calls or wait on an unfinished bulk fetch.

/// The engine's Message path over `service`, its attentions installed on `backend`, as the
/// runtime opens it (#3108).
pub(in crate::in_process) fn engine_messages(
    service: Arc<flotilla_schema_sqlite::stream::Service>,
    backend: &ResourceBackend,
) -> Arc<flotilla_engine_messages::MessagePrimitives> {
    let home = service.home().to_owned();
    let primitives = Arc::new(
        flotilla_engine_messages::MessagePrimitives::open(&home, service, backend.clone(), Arc::new(flotilla_resources::SystemClock))
            .expect("message path"),
    );
    backend.install_attentions(Arc::clone(&primitives) as Arc<dyn flotilla_resources::attentions::Attentions>);
    primitives
}

/// `daemon`'s Message path over its decided store, installed as its runtime installs it.
pub(in crate::in_process) fn install_engine_messages(
    daemon: &crate::in_process::InProcessDaemon,
) -> Arc<flotilla_engine_messages::MessagePrimitives> {
    daemon.open_message_primitives().expect("message path")
}

/// Declare and designate the fleet whose roles the engine's Message delivery reads.
pub(in crate::in_process) async fn designate_fleet(backend: &ResourceBackend) {
    flotilla_engine_messages_testkit::designate_fleet(backend, "flotilla").await;
}
