//! Shared scenario fixtures and stand-ins for credential-controller and forge I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::HostName;
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CredentialExpiry, CrewWorkPhase, CrewWorkState, Environment as ResourceEnvironment,
    EnvironmentSpec as ResourceEnvironmentSpec, Host as ResourceHost, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, HostSpec, HostStatus, InMemoryBackend, InputMeta, PlacementPolicy, PlacementPolicySpec, ResourceBackend,
    ResourceObject, Selector, TerminalAttention, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, VesselRequirement, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY,
    CONVOY_LABEL, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};

use crate::config::ConfigStore;
use crate::in_process::dispatch_board::tests::board;
use crate::in_process::{input_meta_from_resource, InProcessDaemon, WorkCredentialReconciler};
use crate::providers::change_request::ChangeRequestTracker;
use crate::providers::discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement};
use crate::providers::issue_tracker::IssueProvider;
use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};
use crate::testkits::discovery::fake_discovery;
use flotilla_paths::path_context::ExecutionEnvironmentPath;

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
    pub(super) session: String,
    pub(super) environment: String,
    pub(super) fail_next: std::sync::atomic::AtomicBool,
    pub(super) invalidate_next: std::sync::atomic::AtomicBool,
    pub(super) staged: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl WorkCredentialReconciler for SessionStagingProbe {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        assert_eq!(environment_ref, self.environment);
        let sessions = self.backend.clone().using::<ResourceTerminalSession>(namespace);
        let session = sessions.get(&self.session).await.map_err(|error| error.to_string())?;
        let TerminalSessionSource::Agent { message, .. } = &session.spec.source else { return Err("expected agent session".to_string()) };
        assert!(message.is_none(), "message became deliverable before credential staging");
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("credential staging failed".to_string());
        }
        self.staged.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.invalidate_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let mut changed_spec = session.spec.clone();
            let TerminalSessionSource::Agent { brief, .. } = &mut changed_spec.source else { unreachable!("checked above") };
            brief.content.push_str(" (concurrent edit)");
            sessions
                .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &changed_spec)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

pub(super) async fn resume_staging_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    resume_staging_fixture_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await
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
    )
    .await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &test_meta("resume-staging"),
            &ConvoySpec::builder().workflow_ref("implement-review".to_string()).role("resume-staging".to_string()).build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![claim_crew("coder")]).build()],
                }),
                work: BTreeMap::from([(
                    "work".to_string(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status");
    backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("resume-staging-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "resume-staging".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "resume-env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief.md".to_string(),
                        content: "original".to_string(),
                        copies: vec![],
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "resume-staging".to_string(),
                        vessel_ref: "resume-vessel".to_string(),
                    }),
                    message: None,
                },
                cwd: "/repo".to_string(),
                env: Default::default(),
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("session");
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        session: "resume-staging-session".to_string(),
        environment: "resume-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        invalidate_next: std::sync::atomic::AtomicBool::new(false),
        staged: std::sync::atomic::AtomicUsize::new(0),
    });
    daemon.set_work_credential_reconciler(probe.clone()).await;
    (daemon, backend, probe)
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
        _config: &ConfigStore,
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
    ) -> Result<crate::providers::CommandOutput, String> {
        if cmd == "gh" && args.first() == Some(&"api") && args.get(1) == Some(&"--include") {
            let endpoint = args.get(2).ok_or("missing REST endpoint")?;
            let number = endpoint.rsplit('/').next().ok_or("missing PR number")?.parse::<u64>().map_err(|e| e.to_string())?;
            let flags = [
                self.rate_limit_two.load(Ordering::SeqCst),
                self.hard_error_one.load(Ordering::SeqCst),
                self.rate_limit_all.load(Ordering::SeqCst),
                self.mixed_history_errors.load(Ordering::SeqCst),
                self.conflicting.load(Ordering::SeqCst),
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
            return self.run(cmd, args, cwd, label).await.map(|stdout| crate::providers::CommandOutput {
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
            return Ok(crate::providers::CommandOutput {
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
            return Ok(crate::providers::CommandOutput {
                stdout: "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1893456000\r\nRetry-After: 60\r\n\r\n{\"message\":\"You have exceeded a secondary rate limit\"}".into(),
                stderr: String::new(), exit_code: Some(1),
            });
        }
        if query.contains("name:\"one\"") && self.hard_error_one.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("rate limited diagnostics unavailable: access denied".into());
        }
        if query.contains("name:\"two\"") && self.rate_limit_two.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(crate::providers::CommandOutput {
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
                        "state": "OPEN", "isDraft": false, "headRefOid": "abc", "reviewDecision": null,
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
        Ok(crate::providers::CommandOutput {
            stdout: format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": requests}})),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

pub(in crate::in_process) async fn create_identity_convoy(backend: &ResourceBackend, record: &str, role: &str, project: Option<&str>) {
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

pub(in crate::in_process) fn test_meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

pub(super) async fn placement_policy(backend: &ResourceBackend, name: &str, host_ref: &str) -> ResourceObject<PlacementPolicy> {
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta(name),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: host_ref.to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("placement policy")
}

pub(super) async fn create_host_direct_placement(
    backend: &ResourceBackend,
    policy_name: &str,
    host_ref: &str,
    agent_adapters: BTreeSet<String>,
) {
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(host_ref),
            &HostSpec { display_name: host_ref.to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("host create");
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(agent_adapters))].into_iter().collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                ..HostStatus::default()
            },
        )
        .await
        .expect("host status update");
    placement_policy(backend, policy_name, host_ref).await;
}

pub(super) fn trusted_codex_workflow() -> WorkflowTemplateSpec {
    flotilla_resources::single_agent_workflow_spec()
}

pub(in crate::in_process) async fn create_test_environment(daemon: &InProcessDaemon, name: &str, host_ref: &str) -> String {
    daemon
        .resource_backend()
        .using::<ResourceEnvironment>("flotilla")
        .create(
            &test_meta(name),
            &ResourceEnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_ref.to_string(), repo_default_dir: "/tmp".to_string() }),
                docker: None,
            },
        )
        .await
        .expect("environment");
    name.to_string()
}

pub(in crate::in_process) async fn create_running_session(daemon: &InProcessDaemon, env_ref: &str, name: &str, convoy: &str, role: &str) {
    let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
    let created = terminals
        .create(
            &InputMeta::builder()
                .name(name.to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (VESSEL_REF_LABEL.to_string(), format!("{convoy}-work")),
                    (ROLE_LABEL.to_string(), role.to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: env_ref.to_string(),
                role: role.to_string(),
                source: TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                env: Default::default(),
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("terminal session");
    terminals
        .update_status(
            name,
            &created.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                session_id: Some(format!("session-{name}")),
                ..Default::default()
            },
        )
        .await
        .expect("running session");
}

pub(super) async fn create_docker_placement(
    backend: &ResourceBackend,
    policy_name: &str,
    host_ref: &str,
    held_credentials: BTreeSet<String>,
) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(host_ref),
            &HostSpec { display_name: host_ref.to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("host create");
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [
                    (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(held_credentials)),
                    ("docker".to_string(), serde_json::json!(true)),
                    ("os".to_string(), serde_json::json!("linux")),
                ]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            },
        )
        .await
        .expect("host status update");
    backend
        .clone()
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta(policy_name),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: host_ref.to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["codex".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("placement create");
}

pub(super) async fn set_host_credential_expiry(backend: &ResourceBackend, host_ref: &str, expiry: BTreeMap<String, CredentialExpiry>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get(host_ref).await.expect("host resource");
    let mut status = host.status.expect("host status");
    status.capabilities.insert(flotilla_resources::CREDENTIAL_EXPIRY_CAPABILITY.to_string(), serde_json::json!(expiry));
    hosts.update_status(host_ref, &host.metadata.resource_version, &status).await.expect("update host status");
}

pub(super) async fn stall_test_daemon() -> (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"stall-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let (sender, mut receiver) = flotilla_resources::controller::WorkQueueSender::channel();
    let watch = daemon.reconciler_wake_watch();
    let watch_backend = backend.clone();
    let task = tokio::spawn(async move {
        let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        watch.spawn(watch_backend, "flotilla".to_string(), sender).await.expect("stall watch");
        drain.abort();
    });
    (daemon, backend, temp, task)
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

pub(super) async fn stall_test_session(backend: &ResourceBackend, convoy: &str, name: &str, role: &str, attention: TerminalAttention) {
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name(name.into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), convoy.into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), role.into()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".into(),
                role: role.into(),
                source: TerminalSessionSource::Tool { command: "test".into() },
                cwd: "/tmp".into(),
                env: Default::default(),
                pool: "test".into(),
            },
        )
        .await
        .expect("session");
    sessions
        .update_status(
            name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(attention),
                ..Default::default()
            },
        )
        .await
        .expect("attention");
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
