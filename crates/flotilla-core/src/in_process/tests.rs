use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    result_set::{ConvoyPhase as WireConvoyPhase, ConvoyRow, IndependentRow, QueryId, ResultSet, Rows, SessionPhase},
    AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, Command, CommandAction, CommandValue, CrewCommandContext, DaemonEvent,
    EnvironmentId, EnvironmentStatus, HostEnvironment, HostPath, HostSummary, ImageId, Issue, QueryCursor, RepoSelector, ResourceRef,
    SystemInfo, ToolInventory, TopologyRoute,
};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec,
    CheckoutStatus as ResourceCheckoutStatus, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase,
    CrewWorkState, Environment as ResourceEnvironment, EnvironmentSpec as ResourceEnvironmentSpec, HostDirectEnvironmentSpec, InputMeta,
    LifecycleAuthority, ObservedCheckoutSpec as ResourceObservedCheckoutSpec, Selector, TerminalBrief, TerminalCrewContext,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource,
    TerminalSessionSpec as ResourceTerminalSessionSpec, TerminalSessionStatus as ResourceTerminalSessionStatus, Vessel, VesselPhase,
    VesselRequirement, VesselSpec, VesselStatus, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot, CONVOY_LABEL,
    CREW_ORDINAL_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};

use super::*;
use crate::{
    agents::shared_in_memory_agent_state_store,
    attachable::shared_in_memory_attachable_store,
    config::ConfigStore,
    environment_manager::EnvironmentManager,
    model::RepoModel,
    providers::{
        discovery::{
            test_support::{
                fake_discovery, fake_discovery_with_provider_set, git_process_discovery, init_git_repo_with_remote, DiscoveryMockRunner,
                FakeDiscoveryProviders, FakeTerminalPool,
            },
            EnvironmentAssertion, EnvironmentBag,
        },
        environment::{EnvironmentHandle, ProvisionedEnvironment, ProvisionedMount},
        ChannelLabel, CommandOutput, CommandRunner,
    },
};

#[test]
fn project_target_syntax_disambiguates_paths_and_qualified_slugs() {
    assert_eq!(project_target_syntax("/srv/repos/example"), ProjectTargetSyntax::ExplicitPath);
    assert_eq!(project_target_syntax("./org/repo"), ProjectTargetSyntax::ExplicitPath);
    assert_eq!(project_target_syntax("org/repo"), ProjectTargetSyntax::QualifiedSlug);
    assert_eq!(project_target_syntax("repo"), ProjectTargetSyntax::Ambiguous);
}

fn convoy_row(namespace: &str, name: &str, phase: WireConvoyPhase, message: Option<&str>) -> ConvoyRow {
    ConvoyRow::builder()
        .resource(ResourceRef::new("flotilla.work/v1", "Convoy", namespace, name))
        .name(name)
        .workflow_ref("scratch")
        .phase(phase)
        .initializing(true)
        .maybe_message(message.map(str::to_string))
        .build()
}

fn convoy_result_set(seq: u64, rows: Vec<ConvoyRow>) -> ResultSet {
    ResultSet { seq, rows: Rows::Convoys(rows) }
}

async fn set_local_convoy_rows(daemon: &InProcessDaemon, seq: u64, rows: Vec<ConvoyRow>) {
    let state = daemon.aggregator_projection_state().await;
    let mut view = state.write().await;
    view.local_rows = rows.into_iter().map(|row| (row.resource.clone(), row)).collect();
    view.seq = seq;
}

fn node(name: &str) -> NodeInfo {
    NodeInfo::new(NodeId::new(format!("{name}-node")), name)
}

fn local_node_id() -> NodeId {
    NodeId::new("local-node")
}

fn test_environment_manager() -> &'static EnvironmentManager {
    static MANAGER: OnceLock<EnvironmentManager> = OnceLock::new();
    MANAGER.get_or_init(|| {
        EnvironmentManager::from_local_state(
            EnvironmentId::new("test-local-env"),
            HostId::new("test-local-host"),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
        )
    })
}

fn empty_input_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
    }
}

fn input_meta_with_labels(name: &str, labels: BTreeMap<String, String>) -> InputMeta {
    InputMeta { labels, ..empty_input_meta(name) }
}

async fn wait_for_command_result(events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandValue {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(err) => panic!("unexpected event error: {err}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result")
}

async fn force_complete_work(daemon: &InProcessDaemon, events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>) -> CommandValue {
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyWorkForceComplete {
                convoy: "convoy-a".to_string(),
                work: "implement".to_string(),
                message: Some("done".to_string()),
            },
        })
        .await
        .expect("execute should return a command id");
    wait_for_command_result(events, command_id).await
}

async fn new_attach_test_daemon(config_base: &Path) -> Arc<InProcessDaemon> {
    new_attach_test_daemon_with_pool(config_base).await.0
}

async fn new_attach_test_daemon_with_pool(config_base: &Path) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
    let terminal_pool = Arc::new(FakeTerminalPool::new());
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_terminal_pool(Arc::clone(&terminal_pool) as Arc<dyn crate::providers::terminal::TerminalPool>),
    );
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(config_base)), discovery, HostName::local()).await;
    (daemon, terminal_pool)
}

async fn create_local_attach_environment(daemon: &InProcessDaemon) -> String {
    let host_id = daemon.local_host_id().expect("daemon should have a local host id");
    let env_name = format!("host-direct-{host_id}");
    daemon
        .resource_backend()
        .using::<ResourceEnvironment>("flotilla")
        .create(&empty_input_meta(&env_name), &ResourceEnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_id.to_string(), repo_default_dir: "/tmp".to_string() }),
            docker: None,
        })
        .await
        .expect("environment should be created");
    env_name
}

async fn create_remote_attach_environment(daemon: &InProcessDaemon, host: &str) -> String {
    let env_name = format!("host-direct-{host}");
    daemon
        .resource_backend()
        .using::<ResourceEnvironment>("flotilla")
        .create(&empty_input_meta(&env_name), &ResourceEnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: host.to_string(), repo_default_dir: "/tmp".to_string() }),
            docker: None,
        })
        .await
        .expect("remote environment should be created");
    env_name
}

fn write_attach_hosts_config(config_base: &Path, hosts: &[(&str, &str, Option<&str>)]) {
    let mut toml = "[ssh]\nmultiplex = false\n".to_string();
    for (label, hostname, user) in hosts {
        toml.push_str(&format!(
            "\n[hosts.{label}]\nhostname = \"{hostname}\"\nexpected_host_name = \"{label}\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n"
        ));
        if let Some(user) = user {
            toml.push_str(&format!("user = \"{user}\"\n"));
        }
    }
    std::fs::write(config_base.join("hosts.toml"), toml).expect("write hosts config");
}

async fn publish_attach_host_summary(daemon: &InProcessDaemon, node_name: &str, host_name: &str) {
    daemon
        .host_registry
        .publish_peer_summary(
            HostSummary {
                environment_id: EnvironmentId::host(HostId::new(format!("{node_name}-{host_name}-host"))),
                host_name: Some(HostName::new(host_name)),
                node: node(node_name),
                system: SystemInfo {
                    home_dir: Some(PathBuf::from("/home/test")),
                    os: Some("linux".to_string()),
                    arch: Some("aarch64".to_string()),
                    cpu_count: Some(4),
                    memory_total_mb: Some(8192),
                    environment: HostEnvironment::BareMetal,
                },
                inventory: ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
            &|_| {},
        )
        .await;
}

async fn create_running_attach_session(
    daemon: &InProcessDaemon,
    env_ref: &str,
    name: &str,
    session_id: &str,
    convoy: &str,
    task: &str,
    role: &str,
) {
    create_running_attach_session_with_pool(daemon, env_ref, name, session_id, convoy, task, role, "fake-terminals").await;
}

#[allow(clippy::too_many_arguments)]
async fn create_running_attach_session_with_pool(
    daemon: &InProcessDaemon,
    env_ref: &str,
    name: &str,
    session_id: &str,
    convoy: &str,
    task: &str,
    role: &str,
    pool: &str,
) {
    let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
    let created = terminals
        .create(
            &input_meta_with_labels(
                name,
                BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.to_string()),
                    (VESSEL_LABEL.to_string(), task.to_string()),
                    (VESSEL_REF_LABEL.to_string(), format!("{convoy}-{task}")),
                    (ROLE_LABEL.to_string(), role.to_string()),
                ]),
            ),
            &ResourceTerminalSessionSpec {
                env_ref: env_ref.to_string(),
                role: role.to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                pool: pool.to_string(),
            },
        )
        .await
        .expect("terminal session should be created");
    terminals
        .update_status(name, &created.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        })
        .await
        .expect("terminal session should be running");
}

async fn create_adopted_checkout_for_convoy(daemon: &InProcessDaemon, convoy: &str) {
    let checkouts = daemon.resource_backend().using::<ResourceCheckout>("flotilla");
    let checkout_name = format!("adopted-checkout-{convoy}");
    let created = checkouts
        .create(
            &InputMeta::builder()
                .name(checkout_name.clone())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.to_string())]))
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Adopted),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "main".to_string(),
                path: "/repo".to_string(),
                repo_ref: flotilla_resources::RepositoryKey("repo".to_string()),
                host_ref: "host-01".to_string(),
                is_main: true,
            }),
        )
        .await
        .expect("adopted checkout should be created");
    checkouts
        .update_status(&checkout_name, &created.metadata.resource_version, &ResourceCheckoutStatus {
            phase: ResourceCheckoutPhase::Ready,
            path: Some("/repo".to_string()),
            commit: None,
            message: None,
        })
        .await
        .expect("adopted checkout should be ready");
}

async fn create_two_agent_crew(daemon: &InProcessDaemon, env_ref: &str) {
    let convoys = daemon.resource_backend().using::<Convoy>("flotilla");
    let convoy = convoys
        .create(&empty_input_meta("demo"), &ConvoySpec {
            workflow_ref: "coding-review".into(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repository: None,
            r#ref: None,
            project_ref: None,
            adopted_checkout_ref: None,
        })
        .await
        .expect("create convoy");
    let processes = vec![
        CrewSpec::builder()
            .role("coder".to_string())
            .source(CrewSource::Agent { selector: Selector { capability: "coding".into() }, prompt: Some("Implement the change.".into()) })
            .build(),
        CrewSpec::builder()
            .role("reviewer".to_string())
            .source(CrewSource::Agent { selector: Selector { capability: "review".into() }, prompt: Some("Review the change.".into()) })
            .build(),
    ];
    convoys
        .update_status("demo", &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![
                    VesselRequirement { name: "prepare".into(), stance: Default::default(), depends_on: Vec::new(), crew: Vec::new() },
                    VesselRequirement { name: "implement".into(), stance: Default::default(), depends_on: Vec::new(), crew: processes },
                ],
            }),
            work: BTreeMap::from([("implement".to_string(), WorkState {
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })]),
            crew_work: BTreeMap::from([(
                "implement".to_string(),
                BTreeMap::from([
                    ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ("reviewer".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
                ]),
            )]),
            ..Default::default()
        })
        .await
        .expect("update convoy status");

    let workspaces = daemon.resource_backend().using::<Vessel>("flotilla");
    let workspace = workspaces
        .create(
            &input_meta_with_labels(
                "demo-implement",
                BTreeMap::from([(CONVOY_LABEL.into(), "demo".into()), (VESSEL_LABEL.into(), "implement".into())]),
            ),
            &VesselSpec {
                convoy_ref: "demo".into(),
                vessel_name: "implement".into(),
                placement_policy_ref: "host-direct".into(),
                adopted_checkout_ref: None,
            },
        )
        .await
        .expect("create workspace");
    workspaces
        .update_status("demo-implement", &workspace.metadata.resource_version, &VesselStatus {
            phase: VesselPhase::Ready,
            environment_ref: Some(env_ref.into()),
            terminal_session_refs: vec!["terminal-demo-implement-coder".into()],
            ..Default::default()
        })
        .await
        .expect("update workspace status");

    let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
    let coder = terminals
        .create(
            &input_meta_with_labels(
                "terminal-demo-implement-coder",
                BTreeMap::from([
                    (CONVOY_LABEL.into(), "demo".into()),
                    (VESSEL_LABEL.into(), "implement".into()),
                    (VESSEL_REF_LABEL.into(), "demo-implement".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]),
            ),
            &ResourceTerminalSessionSpec {
                env_ref: env_ref.into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "coding".into() },
                    brief: TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "coder brief".into() },
                    context: TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "demo".into(),
                        vessel_ref: "demo-implement".into(),
                    },
                    message: None,
                },
                cwd: "/repo".into(),
                pool: "fake-terminals".into(),
            },
        )
        .await
        .expect("create coder session");
    terminals
        .update_status("terminal-demo-implement-coder", &coder.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            session_id: Some("session-coder".into()),
            crew: Some(flotilla_resources::CrewSessionStatus {
                id: "crew-coder".into(),
                adapter: "codex".into(),
                model: None,
                stance: "trusted-implicit".into(),
            }),
            launch_command: Some("codex".into()),
            ..Default::default()
        })
        .await
        .expect("run coder session");
}

#[tokio::test]
async fn crew_complete_uses_ambient_identity_to_complete_callers_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let (daemon, _) = new_attach_test_daemon_with_pool(temp.path()).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_two_agent_crew(&daemon, &env_ref).await;

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewComplete {
                context: CrewCommandContext { crew_id: Some("crew-coder".into()), ..Default::default() },
                message: Some("ready for review".into()),
            },
        })
        .await
        .expect("crew complete command");

    assert_eq!(wait_for_command_result(&mut events, command_id).await, CommandValue::Ok);
    let convoy = daemon.resource_backend().using::<Convoy>("flotilla").get("demo").await.expect("convoy");
    let coder = &convoy.status.expect("convoy status").crew_work["implement"]["coder"];
    assert_eq!(coder.phase, CrewWorkPhase::Done);
    assert_eq!(coder.message.as_deref(), Some("ready for review"));
}

#[tokio::test]
async fn crew_fail_uses_ambient_identity_to_fail_callers_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let (daemon, _) = new_attach_test_daemon_with_pool(temp.path()).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_two_agent_crew(&daemon, &env_ref).await;

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewFail {
                context: CrewCommandContext { crew_id: Some("crew-coder".into()), ..Default::default() },
                message: "blocked by credentials".into(),
            },
        })
        .await
        .expect("crew fail command");

    assert_eq!(wait_for_command_result(&mut events, command_id).await, CommandValue::Ok);
    let convoy = daemon.resource_backend().using::<Convoy>("flotilla").get("demo").await.expect("convoy");
    let coder = &convoy.status.expect("convoy status").crew_work["implement"]["coder"];
    assert_eq!(coder.phase, CrewWorkPhase::Failed);
    assert_eq!(coder.message.as_deref(), Some("blocked by credentials"));
}

#[tokio::test]
async fn crew_complete_rejects_role_without_agent_work_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let (daemon, _) = new_attach_test_daemon_with_pool(temp.path()).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_two_agent_crew(&daemon, &env_ref).await;

    let error = daemon
        .crew_complete_internal(
            &CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".into()),
                convoy: Some("demo".into()),
                vessel_ref: Some("demo-implement".into()),
                role: Some("build".into()),
            },
            None,
        )
        .await
        .expect_err("role without agent work state should be rejected");

    assert_eq!(error, "crew work for role `build` is not defined on vessel `implement`");
}

#[tokio::test]
async fn handoff_rejects_failed_target_instead_of_succeeding_without_state_change() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let (daemon, _) = new_attach_test_daemon_with_pool(temp.path()).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_two_agent_crew(&daemon, &env_ref).await;
    let reviewer_context = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".into()),
        convoy: Some("demo".into()),
        vessel_ref: Some("demo-implement".into()),
        role: Some("reviewer".into()),
    };
    daemon.crew_fail_internal(&reviewer_context, "review failed".into()).await.expect("reviewer failure should be recorded");

    let error = daemon
        .crew_handoff_internal(
            &CrewCommandContext { crew_id: Some("crew-coder".into()), ..Default::default() },
            "reviewer",
            "retry the review",
        )
        .await
        .expect_err("failed target should reject handoff");

    assert_eq!(error, "crew target `reviewer` has failed work and cannot receive a handoff");
}

#[tokio::test]
async fn crew_list_includes_defined_latent_members_and_handoff_activates_one() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let (daemon, terminal_pool) = new_attach_test_daemon_with_pool(temp.path()).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_two_agent_crew(&daemon, &env_ref).await;
    let context = CrewCommandContext { crew_id: Some("crew-coder".into()), ..Default::default() };

    let response = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryCrewList { context: context.clone() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("crew list query");
    let CommandValue::CrewList(response) = response else { panic!("expected crew list") };
    assert_eq!(response.members.iter().map(|member| (member.role.as_str(), member.state.as_str())).collect::<Vec<_>>(), vec![
        ("coder", "active"),
        ("reviewer", "latent")
    ]);

    let mut events = daemon.subscribe();
    let complete_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewComplete { context: context.clone(), message: Some("implementation ready".into()) },
        })
        .await
        .expect("complete coder work");
    assert_eq!(wait_for_command_result(&mut events, complete_id).await, CommandValue::Ok);

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewHandoff { context, target: "reviewer".into(), message: "Review commit abc123".into() },
        })
        .await
        .expect("handoff command");
    assert_eq!(wait_for_command_result(&mut events, command_id).await, CommandValue::Ok);
    let reviewer = daemon
        .resource_backend()
        .using::<ResourceTerminalSession>("flotilla")
        .get("terminal-demo-implement-reviewer")
        .await
        .expect("reviewer session should be defined");
    assert!(
        matches!(reviewer.spec.source, TerminalSessionSource::Agent { message: Some(ref message), .. } if message.text == "Review commit abc123")
    );
    assert_eq!(reviewer.metadata.labels.get(VESSEL_ORDINAL_LABEL).map(String::as_str), Some("001"));
    assert_eq!(reviewer.metadata.labels.get(CREW_ORDINAL_LABEL).map(String::as_str), Some("001"));

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewHandoff {
                context: CrewCommandContext { crew_id: Some("crew-coder".into()), ..Default::default() },
                target: "reviewer".into(),
                message: "Use the amended commit".into(),
            },
        })
        .await
        .expect("handoff while reviewer is starting");
    assert_eq!(wait_for_command_result(&mut events, command_id).await, CommandValue::Ok);
    let reviewer = daemon
        .resource_backend()
        .using::<ResourceTerminalSession>("flotilla")
        .get("terminal-demo-implement-reviewer")
        .await
        .expect("reviewer session should still exist");
    assert!(matches!(
        reviewer.spec.source,
        TerminalSessionSource::Agent { message: Some(ref message), .. } if message.text == "Use the amended commit"
    ));

    let explicit_context = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".into()),
        convoy: Some("demo".into()),
        vessel_ref: Some("demo-implement".into()),
        role: Some("reviewer".into()),
    };
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewHandoff {
                context: explicit_context.clone(),
                target: "coder".into(),
                message: "Address the review findings".into(),
            },
        })
        .await
        .expect("handoff to active coder");
    assert_eq!(wait_for_command_result(&mut events, command_id).await, CommandValue::Ok);
    assert_eq!(terminal_pool.delivered.lock().await.as_slice(), &[(
        "session-coder".to_string(),
        "Address the review findings".to_string(),
        true
    )]);
    let convoy = daemon.resource_backend().using::<Convoy>("flotilla").get("demo").await.expect("convoy");
    let crew_work = &convoy.status.expect("convoy status").crew_work["implement"];
    assert_eq!(crew_work["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(crew_work["reviewer"].phase, CrewWorkPhase::HandedBack);

    let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
    let coder = terminals.get("terminal-demo-implement-coder").await.expect("coder");
    terminals
        .update_status("terminal-demo-implement-coder", &coder.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Stopped,
            session_id: Some("session-coder".into()),
            crew: coder.status.as_ref().and_then(|status| status.crew.clone()),
            ..Default::default()
        })
        .await
        .expect("stop coder");
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CrewHandoff { context: explicit_context, target: "coder".into(), message: "Resume after review".into() },
        })
        .await
        .expect("revive coder");
    assert_eq!(wait_for_command_result(&mut events, command_id).await, CommandValue::Ok);
    let coder = terminals.get("terminal-demo-implement-coder").await.expect("restarting coder");
    assert_eq!(coder.status.expect("coder status").phase, ResourceTerminalSessionPhase::Starting);
    assert!(
        matches!(coder.spec.source, TerminalSessionSource::Agent { message: Some(ref message), .. } if message.text == "Resume after review")
    );
}

#[test]
fn fleet_replica_ssh_args_wraps_snapshot_command_in_remote_login_shell() {
    let remote = crate::config::RemoteHostConfig {
        hostname: "feta.local".to_string(),
        expected_host_name: "feta".to_string(),
        expected_node_id: None,
        user: Some("alice".to_string()),
        daemon_socket: "/tmp/flotilla.sock".to_string(),
        ssh_multiplex: None,
    };

    let args = fleet_replica_ssh_args(&remote, false);

    assert_eq!(&args[..5], ["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=2"]);
    assert_eq!(args[5], "-o");
    assert_eq!(args[6], "ConnectionAttempts=1");
    assert_eq!(args[7], "alice@feta.local");
    assert_eq!(args.len(), 9);

    let remote_command = args.last().expect("remote command arg");
    assert!(remote_command.starts_with("${SHELL:-/bin/sh} -l -c "), "remote command should start with login shell: {remote_command}");
    assert!(remote_command.contains("exec flotilla --socket"), "remote command should execute flotilla: {remote_command}");
    assert!(remote_command.contains("/tmp/flotilla.sock"), "remote command should include socket: {remote_command}");
    assert!(remote_command.contains("replica-snapshot"), "remote command should include hidden subcommand: {remote_command}");
    assert!(!args.iter().any(|arg| arg == "&&"), "shell operators must not be separate SSH argv elements: {args:?}");
}

#[test]
fn fleet_replica_ssh_args_preserves_multiplex_options() {
    let remote = crate::config::RemoteHostConfig {
        hostname: "feta.local".to_string(),
        expected_host_name: "feta".to_string(),
        expected_node_id: None,
        user: None,
        daemon_socket: "/tmp/flotilla.sock".to_string(),
        ssh_multiplex: None,
    };

    let args = fleet_replica_ssh_args(&remote, true);

    assert!(args.windows(2).any(|window| window == ["-o", "ControlMaster=auto"]));
    assert!(args.windows(2).any(|window| window == ["-o", "ControlPath=/tmp/flotilla-ssh-%C"]));
    assert!(args.windows(2).any(|window| window == ["-o", "ControlPersist=60"]));
    assert_eq!(args[13], "feta.local");
    assert!(args[14].starts_with("${SHELL:-/bin/sh} -l -c "));
}

struct QueuedOutputRunner {
    outputs: Mutex<VecDeque<CommandOutput>>,
}

impl QueuedOutputRunner {
    fn new(outputs: Vec<CommandOutput>) -> Self {
        Self { outputs: Mutex::new(outputs.into()) }
    }
}

#[async_trait]
impl CommandRunner for QueuedOutputRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        if cmd == "git" && args == ["--version"] {
            Ok("git version 2.43.0".to_string())
        } else {
            Err(format!("QueuedOutputRunner: no run response for {cmd} {}", args.join(" ")))
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "ssh");
        assert!(args.contains(&"ConnectTimeout=2"), "replica fetch should bound ssh connection time: {args:?}");
        assert!(
            args.last().is_some_and(|arg| arg.starts_with("${SHELL:-/bin/sh} -l -c ") && arg.contains("exec flotilla")),
            "replica fetch should pass one remote command through the remote login shell: {args:?}"
        );
        self.outputs.lock().expect("outputs mutex").pop_front().ok_or_else(|| "no queued output".to_string())
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

#[tokio::test]
async fn fleet_list_reports_store_backed_local_sessions_with_authority() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_adopted_checkout_for_convoy(&daemon, "convoy-a").await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;

    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");

    assert!(response.replicas.is_empty());
    assert_eq!(response.rows.len(), 1);
    let row = &response.rows[0];
    assert_eq!(row.convoy, "convoy-a");
    assert_eq!(row.vessel, env_ref);
    assert_eq!(row.authority.as_deref(), Some("adopted"));
    assert_eq!(row.crew, "implement/coder");
    assert_eq!(row.crew_state, "running");
    assert_eq!(row.host, daemon.host_name);
    assert_eq!(row.staleness, FleetStaleness::Local);

    let snapshot = daemon.fleet_replica_snapshot_internal().await.expect("fleet replica snapshot should succeed");
    assert_eq!(snapshot.host, daemon.host_name);
    assert_eq!(snapshot.rows, response.rows);
}

#[tokio::test]
async fn fleet_list_reports_local_crewless_failed_convoys() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    set_local_convoy_rows(&daemon, 1, vec![
        convoy_row("flotilla", "convoy-failed", WireConvoyPhase::Failed, Some("missing input 'topic'")),
        convoy_row("other", "other-failed", WireConvoyPhase::Failed, Some("wrong namespace")),
    ])
    .await;

    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");

    assert_eq!(response.rows.len(), 1);
    let row = &response.rows[0];
    assert_eq!(row.convoy, "convoy-failed");
    assert_eq!(row.vessel, "-");
    assert_eq!(row.crew, "-");
    assert_eq!(row.crew_state, "failed: missing input 'topic'");
    assert_eq!(row.host, daemon.host_name);
    assert_eq!(row.staleness, FleetStaleness::Local);
}

#[tokio::test]
async fn fleet_list_does_not_add_crewless_row_when_convoy_has_crew() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;

    set_local_convoy_rows(&daemon, 1, vec![convoy_row("flotilla", "convoy-a", WireConvoyPhase::Active, None)]).await;

    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");

    assert_eq!(response.rows.len(), 1);
    assert_eq!(response.rows[0].convoy, "convoy-a");
    assert_eq!(response.rows[0].crew, "implement/coder");
    assert_eq!(response.rows[0].crew_state, "running");
}

#[tokio::test]
async fn fleet_list_preserves_stale_rows_when_replica_is_unreachable() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let daemon = new_attach_test_daemon(&config_base).await;
    let last_sync = Utc::now() - chrono::Duration::seconds(FLEET_REPLICA_FRESH_SECS + 1);
    daemon.fleet_replica_cache.write().await.insert(HostName::new("feta"), FleetReplicaCacheEntry {
        rows: vec![FleetListRow::builder()
            .convoy("convoy-remote".to_string())
            .vessel("remote-env".to_string())
            .crew("implement/coder".to_string())
            .crew_state("running".to_string())
            .host(HostName::new("feta"))
            .namespace("dev")
            .staleness(FleetStaleness::Local)
            .build()],
        result_sets: vec![],
        last_sync: Some(last_sync),
        generation: Some("gen-1".to_string()),
        last_error: Some("connection refused".to_string()),
    });

    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");

    assert_eq!(response.rows.len(), 1);
    assert!(matches!(
        &response.rows[0].staleness,
        FleetStaleness::Unreachable { last_sync: Some(sync), ref message } if *sync == last_sync && message == "connection refused"
    ));
    assert_eq!(response.replicas.len(), 1);
    assert_eq!(response.replicas[0].host, HostName::new("feta"));
    assert!(!response.replicas[0].reachable);
    assert_eq!(response.replicas[0].last_sync, Some(last_sync));
    assert_eq!(response.replicas[0].generation.as_deref(), Some("gen-1"));
    assert_eq!(response.replicas[0].message.as_deref(), Some("connection refused"));
}

#[tokio::test]
async fn replica_refresh_replaces_rows_when_generation_changes() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let first = FleetReplicaSnapshot {
        host: HostName::new("feta"),
        generation: Some("gen-1".to_string()),
        rows: vec![FleetListRow::builder()
            .convoy("old-convoy".to_string())
            .vessel("old-env".to_string())
            .crew("implement/coder".to_string())
            .crew_state("running".to_string())
            .host(HostName::new("feta"))
            .namespace("dev")
            .staleness(FleetStaleness::Local)
            .build()],
        result_sets: vec![],
    };
    let second = FleetReplicaSnapshot {
        host: HostName::new("feta"),
        generation: Some("gen-2".to_string()),
        rows: vec![FleetListRow::builder()
            .convoy("new-convoy".to_string())
            .vessel("new-env".to_string())
            .maybe_authority(Some("adopted".to_string()))
            .crew("reviewer".to_string())
            .crew_state("stopped".to_string())
            .host(HostName::new("feta"))
            .namespace("dev")
            .staleness(FleetStaleness::Local)
            .build()],
        result_sets: vec![],
    };
    let runner = Arc::new(QueuedOutputRunner::new(vec![
        CommandOutput { stdout: serde_json::to_string(&first).expect("serialize first snapshot"), stderr: String::new(), success: true },
        CommandOutput { stdout: serde_json::to_string(&second).expect("serialize second snapshot"), stderr: String::new(), success: true },
    ]));
    let mut discovery =
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(Arc::new(FakeTerminalPool::new())));
    discovery.runner = runner;
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), discovery, HostName::local()).await;

    daemon.refresh_fleet_replicas_once().await.expect("first refresh should succeed");
    daemon.refresh_fleet_replicas_once().await.expect("second refresh should succeed");

    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");
    assert_eq!(response.rows.len(), 1);
    assert_eq!(response.rows[0].convoy, "new-convoy");
    assert_eq!(response.rows[0].authority.as_deref(), Some("adopted"));
    assert!(matches!(&response.rows[0].staleness, FleetStaleness::Fresh { .. }));
    assert_eq!(response.replicas.len(), 1);
    assert!(response.replicas[0].reachable);
    assert_eq!(response.replicas[0].generation.as_deref(), Some("gen-2"));
}

#[tokio::test]
async fn replica_refresh_reports_crewless_convoys_from_panel_snapshots() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let snapshot = FleetReplicaSnapshot {
        host: HostName::new("feta"),
        generation: Some("gen-1".to_string()),
        rows: vec![],
        result_sets: vec![convoy_result_set(3, vec![
            convoy_row("flotilla", "remote-failed", WireConvoyPhase::Failed, Some("missing input 'topic'")),
            convoy_row("other", "other-failed", WireConvoyPhase::Failed, Some("wrong namespace")),
        ])],
    };
    let runner = Arc::new(QueuedOutputRunner::new(vec![CommandOutput {
        stdout: serde_json::to_string(&snapshot).expect("serialize snapshot"),
        stderr: String::new(),
        success: true,
    }]));
    let mut discovery =
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(Arc::new(FakeTerminalPool::new())));
    discovery.runner = runner;
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), discovery, HostName::local()).await;

    daemon.refresh_fleet_replicas_once().await.expect("refresh should succeed");
    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");

    assert_eq!(response.rows.len(), 1);
    let row = &response.rows[0];
    assert_eq!(row.convoy, "remote-failed");
    assert_eq!(row.vessel, "-");
    assert_eq!(row.crew, "-");
    assert_eq!(row.crew_state, "failed: missing input 'topic'");
    assert_eq!(row.host, HostName::new("feta"));
    assert!(matches!(row.staleness, FleetStaleness::Fresh { .. }));
}

#[tokio::test]
async fn replica_refresh_dedupes_crewless_rows_already_present_in_snapshot_rows() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let snapshot = FleetReplicaSnapshot {
        host: HostName::new("feta"),
        generation: Some("gen-1".to_string()),
        rows: vec![FleetListRow::builder()
            .convoy("remote-failed".to_string())
            .vessel("-".to_string())
            .crew("-".to_string())
            .crew_state("failed: missing input 'topic'".to_string())
            .host(HostName::new("feta"))
            .namespace("dev")
            .staleness(FleetStaleness::Local)
            .build()],
        result_sets: vec![convoy_result_set(3, vec![convoy_row(
            "flotilla",
            "remote-failed",
            WireConvoyPhase::Failed,
            Some("missing input 'topic'"),
        )])],
    };
    let runner = Arc::new(QueuedOutputRunner::new(vec![CommandOutput {
        stdout: serde_json::to_string(&snapshot).expect("serialize snapshot"),
        stderr: String::new(),
        success: true,
    }]));
    let mut discovery =
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(Arc::new(FakeTerminalPool::new())));
    discovery.runner = runner;
    let daemon = InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), discovery, HostName::local()).await;

    daemon.refresh_fleet_replicas_once().await.expect("refresh should succeed");
    let response = daemon.fleet_list_internal().await.expect("fleet list should succeed");

    assert_eq!(response.rows.len(), 1);
    assert_eq!(response.rows[0].convoy, "remote-failed");
    assert_eq!(response.rows[0].crew, "-");
}

#[test]
fn choose_event_uses_delta_for_non_initial_changes() {
    let repo = PathBuf::from("/tmp/repo");
    let snapshot = RepoSnapshot {
        seq: 2,
        repo_identity: fallback_repo_identity(&repo),
        repo: Some(repo.clone()),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    };

    let initial = DeltaEntry { seq: 1, prev_seq: 0, changes: vec![] };
    assert!(matches!(choose_event(snapshot.clone(), initial), DaemonEvent::RepoSnapshot(_)));

    let non_empty = DeltaEntry {
        seq: 2,
        prev_seq: 1,
        changes: vec![flotilla_protocol::Change::Branch { key: "feature/x".into(), op: flotilla_protocol::EntryOp::Removed }],
    };
    assert!(matches!(choose_event(snapshot, non_empty), DaemonEvent::RepoDelta(_)));
}

#[tokio::test]
async fn attach_query_resolves_running_terminal_session_by_convoy_task_role() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-a-implement-coder",
        "cleat-session-1",
        "convoy-a",
        "implement",
        "coder",
    )
    .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, binding } = result else {
        panic!("expected attach command, got {result:?}");
    };
    assert_eq!(command, "attach cleat-session-1");
    let binding = binding.expect("local resolution carries the structured binding");
    assert_eq!(binding.session.as_deref(), Some("terminal-convoy-a-implement-coder"));
    assert_eq!(binding.convoy.as_deref(), Some("convoy-a"));
    assert_eq!(binding.vessel.as_deref(), Some("implement"));
    assert_eq!(binding.role.as_deref(), Some("coder"));
}

#[tokio::test]
async fn attach_query_rejects_a_running_agent_without_a_recorded_launch_command() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    let sessions = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
    let created = sessions
        .create(
            &input_meta_with_labels(
                "terminal-convoy-a-implement-coder",
                BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
                    (VESSEL_LABEL.to_string(), "implement".to_string()),
                    (VESSEL_REF_LABEL.to_string(), "convoy-a-implement".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]),
            ),
            &ResourceTerminalSessionSpec {
                env_ref,
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "coding".to_string() },
                    brief: TerminalBrief { path: ".flotilla/briefs/coder.md".into(), content: "brief".into() },
                    context: TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "convoy-a".into(),
                        vessel_ref: "convoy-a-implement".into(),
                    },
                    message: None,
                },
                cwd: "/repo".to_string(),
                pool: "fake-terminals".to_string(),
            },
        )
        .await
        .expect("starting agent session");
    sessions
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            session_id: Some("agent-session".to_string()),
            launch_command: None,
            ..Default::default()
        })
        .await
        .expect("malformed running status");

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    assert_eq!(result, CommandValue::Error {
        message: "agent terminal session terminal-convoy-a-implement-coder has no recorded launch command".to_string()
    });
}

#[tokio::test]
async fn attach_query_resolves_remote_session_as_one_recursive_hop() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_remote_attach_environment(&daemon, "feta").await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-a-implement-coder",
        "remote-provider-session",
        "convoy-a",
        "implement",
        "coder",
    )
    .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, .. } = result else {
        panic!("expected attach command, got {result:?}");
    };
    assert!(command.starts_with("ssh -t 'alice@feta.local' "), "command should target the next host over SSH: {command}");
    assert!(command.contains("${SHELL:-/bin/sh} -l -c"), "command should run through a remote login shell: {command}");
    assert!(command.contains("flotilla attach"), "command should recursively invoke flotilla attach: {command}");
    assert!(command.contains("convoy-a/implement/coder"), "command should preserve the original reference: {command}");
    assert!(!command.contains("remote-provider-session"), "remote hop must not include terminal-provider attach args: {command}");
    assert_eq!(command.matches("flotilla attach").count(), 1, "command should contain exactly one recursive attach invocation: {command}");
}

#[tokio::test]
async fn attach_query_resolves_fleet_replica_session_as_one_recursive_hop() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let daemon = new_attach_test_daemon(&config_base).await;
    daemon.fleet_replica_cache.write().await.insert(HostName::new("feta"), FleetReplicaCacheEntry {
        rows: vec![FleetListRow::builder()
            .convoy("convoy-a".to_string())
            .vessel("remote-env".to_string())
            .crew("implement/coder".to_string())
            .crew_state("running".to_string())
            .host(HostName::new("feta"))
            .namespace("dev")
            .session("terminal-remote-coder")
            .staleness(FleetStaleness::Stale { last_sync: Utc::now() - chrono::Duration::seconds(FLEET_REPLICA_FRESH_SECS + 1) })
            .build()],
        result_sets: vec![],
        last_sync: Some(Utc::now() - chrono::Duration::seconds(FLEET_REPLICA_FRESH_SECS + 1)),
        generation: Some("gen-1".to_string()),
        last_error: Some("connection refused".to_string()),
    });

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, binding } = result else {
        panic!("expected attach command, got {result:?}");
    };
    let binding = binding.expect("replica resolution carries the structured binding");
    assert_eq!(binding.host.as_str(), "feta");
    assert_eq!(binding.namespace, "dev");
    assert_eq!(binding.session.as_deref(), Some("terminal-remote-coder"), "cross-host panes stamp the full join key");
    assert_eq!(binding.convoy.as_deref(), Some("convoy-a"));
    assert_eq!(binding.vessel.as_deref(), Some("implement"));
    assert_eq!(binding.role.as_deref(), Some("coder"));
    assert!(command.starts_with("ssh -t 'alice@feta.local' "), "command should target the replica host over SSH: {command}");
    assert!(command.contains("${SHELL:-/bin/sh} -l -c"), "command should run through a remote login shell: {command}");
    assert!(command.contains("flotilla attach"), "command should recursively invoke flotilla attach: {command}");
    assert!(command.contains("convoy-a/implement/coder"), "command should preserve the original reference: {command}");

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, .. } = result else {
        panic!("expected attach command, got {result:?}");
    };
    assert!(command.starts_with("ssh -t 'alice@feta.local' "), "bare role should resolve through the replica host: {command}");
    assert!(command.contains("flotilla attach"), "bare role should recursively invoke flotilla attach: {command}");
    assert!(command.contains("coder"), "command should preserve the original bare role reference: {command}");

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "terminal-remote-coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, .. } = result else {
        panic!("expected attach command, got {result:?}");
    };
    assert!(command.starts_with("ssh -t 'alice@feta.local' "), "session name should resolve through the replica host: {command}");
    assert!(command.contains("terminal-remote-coder"), "command should preserve the independent row's attach reference: {command}");
}

#[tokio::test]
async fn transient_attach_selects_the_displayed_host_for_result_set_only_independents() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice")), ("gouda", "gouda.local", None)]);

    let daemon = new_attach_test_daemon(&config_base).await;
    for host in ["feta", "gouda"] {
        let host_name = HostName::new(host);
        let row = IndependentRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "TerminalSession", "dev", "terminal-scratch").on_host(host_name.clone()))
            .name("terminal-scratch")
            .host(host_name.clone())
            .attach("terminal-scratch")
            .phase(SessionPhase::Running)
            .build();
        let fleet_rows = (host == "feta")
            .then(|| {
                FleetListRow::builder()
                    .convoy("-")
                    .vessel("remote-environment")
                    .crew("shell")
                    .crew_state("running")
                    .host(HostName::new("environment-host"))
                    .namespace("dev")
                    .session("terminal-scratch")
                    .staleness(FleetStaleness::Local)
                    .build()
            })
            .into_iter()
            .collect();
        daemon.fleet_replica_cache.write().await.insert(host_name, FleetReplicaCacheEntry {
            rows: fleet_rows,
            result_sets: vec![ResultSet { seq: 1, rows: Rows::Independents(vec![row]) }],
            last_sync: Some(Utc::now()),
            generation: Some(format!("gen-{host}")),
            last_error: None,
        });
    }

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::AttachTransient { reference: "terminal-scratch".to_string(), host: Some(HostName::new("feta")) },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("transient attach query should execute");

    let CommandValue::AttachCommandResolved { command, binding } = result else {
        panic!("expected attach command, got {result:?}");
    };
    let binding = binding.expect("replica resolution carries a structured binding");
    assert_eq!(binding.host, HostName::new("feta"));
    assert_eq!(binding.session.as_deref(), Some("terminal-scratch"));
    assert_eq!(binding.convoy, None);
    assert_eq!(binding.role, None);
    assert!(command.starts_with("ssh -t 'alice@feta.local' "), "selected row should route through feta: {command}");
    assert!(command.contains("--transient"), "recursive attach must preserve the no-stamp mode: {command}");
    assert!(command.contains("--host"), "recursive attach must preserve the owning host: {command}");
    assert!(!command.contains("gouda.local"), "same-named row on another host must not make selection ambiguous: {command}");
}

#[tokio::test]
async fn attach_query_ignores_fleet_replica_hosts_that_are_not_configured() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let daemon = new_attach_test_daemon(&config_base).await;
    daemon.fleet_replica_cache.write().await.insert(HostName::new("removed"), FleetReplicaCacheEntry {
        rows: vec![FleetListRow::builder()
            .convoy("convoy-a".to_string())
            .vessel("removed-env".to_string())
            .crew("implement/coder".to_string())
            .crew_state("running".to_string())
            .host(HostName::new("removed"))
            .namespace("dev")
            .staleness(FleetStaleness::Fresh { last_sync: Utc::now() })
            .build()],
        result_sets: vec![],
        last_sync: Some(Utc::now()),
        generation: Some("gen-1".to_string()),
        last_error: None,
    });

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "removed-env".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::Error { message } = result else {
        panic!("expected attach error, got {result:?}");
    };
    assert_eq!(message, "no attach target matching 'removed-env'");
}

#[tokio::test]
async fn attach_query_uses_topology_next_hop_for_multi_hop_route_shape() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let daemon = new_attach_test_daemon(&config_base).await;
    publish_attach_host_summary(&daemon, "feta", "feta").await;
    publish_attach_host_summary(&daemon, "gouda", "gouda").await;
    daemon
        .set_topology_routes(vec![TopologyRoute {
            target: node("gouda"),
            next_hop: node("feta"),
            direct: false,
            connected: true,
            fallbacks: vec![],
        }])
        .await;

    let env_ref = create_remote_attach_environment(&daemon, "gouda").await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-a-implement-coder",
        "gouda-provider-session",
        "convoy-a",
        "implement",
        "coder",
    )
    .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, .. } = result else {
        panic!("expected attach command, got {result:?}");
    };
    assert!(command.starts_with("ssh -t 'alice@feta.local' "), "command should target the routed next hop: {command}");
    assert!(command.contains("${SHELL:-/bin/sh} -l -c"), "command should run through a remote login shell: {command}");
    assert!(command.contains("flotilla attach"), "command should recursively invoke flotilla attach on the next hop: {command}");
    assert!(!command.contains("gouda.local"), "command should not try to jump directly to the final host: {command}");
    assert!(!command.contains("gouda-provider-session"), "command should not embed final terminal-provider attach args: {command}");
}

#[tokio::test]
async fn attach_query_prefers_exact_reference_over_prefix_matches() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-a-implement-coder",
        "session-exact",
        "convoy-a",
        "implement",
        "coder",
    )
    .await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-alpha-implement-coder",
        "session-prefix",
        "convoy-alpha",
        "implement",
        "coder",
    )
    .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::AttachCommandResolved { command, binding } = result else {
        panic!("expected attach command, got {result:?}");
    };
    assert_eq!(command, "attach session-exact");
    assert_eq!(binding.expect("binding present").session.as_deref(), Some("terminal-convoy-a-implement-coder"));
}

#[tokio::test]
async fn batch_attach_capabilities_return_only_resolvable_references() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-b-review-reviewer", "session-b", "convoy-b", "review", "reviewer")
        .await;

    let references =
        vec!["terminal-convoy-a-implement-coder".to_string(), "terminal-convoy-b-review-reviewer".to_string(), "missing".to_string()];
    let resolved =
        daemon.resolvable_attach_references_internal(&references).await.expect("batch attach capability resolution should succeed");

    assert_eq!(
        resolved,
        HashSet::from(["terminal-convoy-a-implement-coder".to_string(), "terminal-convoy-b-review-reviewer".to_string(),])
    );
}

#[tokio::test]
async fn attach_query_rejects_ambiguous_prefix() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-alpha-implement-coder",
        "session-alpha",
        "convoy-alpha",
        "implement",
        "coder",
    )
    .await;
    create_running_attach_session(
        &daemon,
        &env_ref,
        "terminal-convoy-amber-implement-coder",
        "session-amber",
        "convoy-amber",
        "implement",
        "coder",
    )
    .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::Error { message } = result else {
        panic!("expected ambiguous attach error, got {result:?}");
    };
    assert!(message.contains("ambiguous"), "message should explain ambiguity: {message}");
    assert!(message.contains("convoy-alpha/implement/coder"), "message should include first candidate: {message}");
    assert!(message.contains("convoy-amber/implement/coder"), "message should include second candidate: {message}");
}

#[tokio::test]
async fn attach_query_reports_no_matching_reference() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "missing".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    assert_eq!(result, CommandValue::Error { message: "no attach target matching 'missing'".to_string() });
}

#[tokio::test]
async fn attach_query_reports_unreachable_next_hop_for_remote_session_without_host_config() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_remote_attach_environment(&daemon, "missing-host").await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    let CommandValue::Error { message } = result else {
        panic!("expected unreachable next-hop error, got {result:?}");
    };
    assert!(message.contains("unreachable next hop 'missing-host'"), "message should identify the unreachable next hop: {message}");
    assert!(message.contains("unknown remote host"), "message should include the host config lookup failure: {message}");
}

#[tokio::test]
async fn attach_query_reports_route_that_points_back_to_local_host() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    write_attach_hosts_config(&config_base, &[("feta", "feta.local", Some("alice"))]);

    let daemon = new_attach_test_daemon(&config_base).await;
    publish_attach_host_summary(&daemon, "feta", "feta").await;
    daemon
        .set_topology_routes(vec![TopologyRoute {
            target: node("feta"),
            next_hop: NodeInfo::new(daemon.node_id().clone(), "local"),
            direct: false,
            connected: true,
            fallbacks: vec![],
        }])
        .await;

    let env_ref = create_remote_attach_environment(&daemon, "feta").await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    assert_eq!(result, CommandValue::Error {
        message: "unreachable next hop for host 'feta': route points back to local host".to_string()
    });
}

#[tokio::test]
async fn attach_query_reports_ambiguous_routed_host_name() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    publish_attach_host_summary(&daemon, "feta-a", "feta").await;
    publish_attach_host_summary(&daemon, "feta-b", "feta").await;

    let env_ref = create_remote_attach_environment(&daemon, "feta").await;
    create_running_attach_session(&daemon, &env_ref, "terminal-convoy-a-implement-coder", "session-a", "convoy-a", "implement", "coder")
        .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    assert_eq!(result, CommandValue::Error { message: "host name 'feta' matches multiple routed nodes".to_string() });
}

#[tokio::test]
async fn attach_query_rejects_empty_reference() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    assert_eq!(result, CommandValue::Error { message: "attach reference is required".to_string() });
}

#[tokio::test]
async fn attach_query_errors_when_recorded_terminal_pool_is_unavailable() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon = new_attach_test_daemon(&config_base).await;
    let env_ref = create_local_attach_environment(&daemon).await;
    create_running_attach_session_with_pool(
        &daemon,
        &env_ref,
        "terminal-convoy-a-implement-coder",
        "session-a",
        "convoy-a",
        "implement",
        "coder",
        "missing-terminals",
    )
    .await;

    let result = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a/implement/coder".to_string() },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("attach query should execute");

    assert_eq!(result, CommandValue::Error { message: format!("terminal pool missing-terminals unavailable for environment {env_ref}") });
}

#[test]
fn choose_event_falls_back_to_full_when_delta_is_larger() {
    let snapshot = RepoSnapshot {
        seq: 3,
        repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
        repo: Some(PathBuf::from("/tmp/repo")),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    };

    let delta = DeltaEntry {
        seq: 3,
        prev_seq: 2,
        changes: vec![flotilla_protocol::Change::Branch { key: "feature/".repeat(128), op: flotilla_protocol::EntryOp::Removed }],
    };

    assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
}

#[test]
fn build_repo_snapshot_basic() {
    let default_snap = RefreshSnapshot::default();
    let snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &default_snap.providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &HostName::local(),
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        7,
        None,
    );
    assert_eq!(snap.seq, 7);
}

// --- choose_event edge case: empty changes with prev_seq > 0 ---

#[test]
fn choose_event_sends_full_when_delta_has_empty_changes() {
    let snapshot = RepoSnapshot {
        seq: 2,
        repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
        repo: Some(PathBuf::from("/tmp/repo")),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    };

    // prev_seq > 0 but changes is empty — should still send full
    let delta = DeltaEntry { seq: 2, prev_seq: 1, changes: vec![] };
    assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
}

// --- build_repo_snapshot_with_peers ---

#[test]
fn build_repo_snapshot_with_peers_merges_peer_data() {
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    // Create peer provider data with a checkout owned by host_b
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(flotilla_protocol::HostPath::new(host_b.clone(), PathBuf::from("/remote/repo")).into(), Checkout {
        branch: "remote-feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });

    let peers = vec![(node(host_b.as_str()), peer_data)];
    let default_snap = RefreshSnapshot::default();
    let snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &default_snap.providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &host_a,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        1,
        Some(&peers),
    );

    // The snapshot should contain the merged peer checkout
    assert!(!snap.providers.checkouts.is_empty(), "peer checkout should be merged");
    assert_eq!(snap.providers.checkouts.len(), 1);
}

/// Regression test: when `base` already contains merged peer data (as happens
/// after poll_snapshots stores `re_snapshot` in `last_snapshot`), calling
/// `build_repo_snapshot_with_peers` again must not re-attribute peer checkouts
/// to the local host via `normalize_local_provider_hosts`.
#[test]
fn build_repo_snapshot_with_peers_does_not_duplicate_from_merged_base() {
    let local_host = HostName::new("feta");
    let peer_host = HostName::new("kiwi");

    // Simulate local checkout
    let mut local_providers = ProviderData::default();
    local_providers.checkouts.insert(
        flotilla_protocol::HostPath::new(local_host.clone(), PathBuf::from("/home/dev/repo")).into(),
        Checkout {
            branch: "main".into(),
            is_main: true,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            host_name: None,
            environment_id: None,
        },
    );

    // Create peer data
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(flotilla_protocol::HostPath::new(peer_host.clone(), PathBuf::from("/srv/kiwi/repo")).into(), Checkout {
        branch: "peer-feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });
    let peers = vec![(node(peer_host.as_str()), peer_data.clone())];
    let default_snap = RefreshSnapshot::default();

    // First call — simulates the initial build (local-only base).
    // This produces a merged result containing both local + peer checkouts.
    let first_snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/home/dev/repo")),
            path: Path::new("/home/dev/repo"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &local_host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        1,
        Some(&peers),
    );
    assert_eq!(first_snap.providers.checkouts.len(), 2, "first build should have local + peer checkout");

    // Simulate poll_snapshots storing the merged result as last_snapshot
    // while last_local_providers retains only local data.
    // The bug was: passing merged providers as the base to a second call
    // would re-stamp peer checkouts as local via normalize_local_provider_hosts.
    // With the fix, callers always pass local_providers, never merged data.

    // Second call — uses local-only providers (the fix), not merged data.
    let second_snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/home/dev/repo")),
            path: Path::new("/home/dev/repo"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &local_host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        2,
        Some(&peers),
    );

    // The peer checkout must appear exactly once under kiwi
    let kiwi_count = second_snap.providers.checkouts.keys().filter(|hp| hp.host_name() == Some(&peer_host)).count();
    assert_eq!(kiwi_count, 1, "peer checkout should appear once under kiwi, got {kiwi_count}");

    // No ghost checkout — kiwi's path must not appear under the local host
    let ghost = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(&local_host, PathBuf::from("/srv/kiwi/repo"));
    assert!(
        !second_snap.providers.checkouts.contains_key(&ghost),
        "peer checkout at /srv/kiwi/repo must not be re-stamped as local host checkout"
    );

    // Total checkout count should remain 2 (1 local + 1 peer)
    assert_eq!(
        second_snap.providers.checkouts.len(),
        2,
        "should have exactly 2 checkouts (1 local + 1 peer), got {}",
        second_snap.providers.checkouts.len()
    );
}

#[test]
fn build_repo_snapshot_with_peers_preserves_remote_attachable_set_for_local_workspace_binding() {
    let local_host = HostName::new("kiwi");
    let remote_host = HostName::new("feta");
    let remote_checkout = HostPath::new(remote_host.clone(), PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"));
    let set_id = flotilla_protocol::AttachableSetId::new("set-remote");

    let mut local_providers = ProviderData::default();
    local_providers.workspaces.insert("workspace:9".into(), flotilla_protocol::Workspace {
        name: "attachable-correlation@feta".into(),
        correlation_keys: vec![],
        attachable_set_id: Some(set_id.clone()),
    });
    local_providers.attachable_sets.insert(set_id.clone(), flotilla_protocol::AttachableSet {
        id: set_id.clone(),
        host_affinity: Some(remote_host.clone()),
        checkout: Some(remote_checkout.clone().into()),
        template_identity: None,
        environment_id: None,
        members: vec![],
    });

    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(remote_checkout.clone().into(), Checkout {
        branch: "attachable-correlation".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![
            CorrelationKey::Branch("attachable-correlation".into()),
            CorrelationKey::CheckoutPath(remote_checkout.clone().into()),
        ],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });

    let peers = vec![(node(remote_host.as_str()), peer_data)];
    let default_snap = RefreshSnapshot::default();
    let snapshot = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/Users/robert/dev/flotilla")),
            path: Path::new("/Users/robert/dev/flotilla"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &local_host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        1,
        Some(&peers),
    );

    let set = snapshot.providers.attachable_sets.get(&set_id).expect("attachable set should remain projected");
    assert_eq!(set.host_affinity.as_ref(), Some(&remote_host), "remote attachable set host affinity should stay on feta");
    assert_eq!(set.checkout.as_ref(), Some(&remote_checkout.clone().into()), "remote attachable set checkout should stay on feta");

    let set_item =
        snapshot.work_items.iter().find(|item| item.attachable_set_id.as_ref() == Some(&set_id)).expect("work item for attachable set");
    assert_eq!(set_item.node_id, node(remote_host.as_str()).node_id, "correlated work item should be anchored to feta");
    assert_eq!(
        set_item.checkout.as_ref().and_then(|checkout| checkout.host_path()),
        Some(&remote_checkout),
        "correlated work item should point at the remote checkout"
    );
    assert_eq!(set_item.workspace_refs, vec!["workspace:9".to_string()]);

    let ghost_checkout = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(
        &local_host,
        PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"),
    );
    assert!(
        !snapshot.providers.checkouts.contains_key(&ghost_checkout),
        "remote checkout path must not be duplicated under the local host"
    );
}

// --- collect_linked_issue_ids ---

#[test]
fn collect_linked_issue_ids_from_change_requests() {
    let mut providers = ProviderData::default();
    providers.change_requests.insert("PR-1".into(), ChangeRequest {
        title: "Fix bug".into(),
        branch: "fix/bug".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![],
        association_keys: vec![
            AssociationKey::IssueRef("github".into(), "42".into()),
            AssociationKey::IssueRef("github".into(), "99".into()),
        ],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });

    let mut ids = collect_linked_issue_ids(&providers);
    ids.sort();
    assert_eq!(ids, vec!["42", "99"]);
}

#[test]
fn collect_linked_issue_ids_from_checkouts() {
    let mut providers = ProviderData::default();
    providers.checkouts.insert(HostPath::new(HostName::new("host"), PathBuf::from("/tmp/co")).into(), Checkout {
        branch: "feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "7".into())],
        host_name: None,
        environment_id: None,
    });

    let ids = collect_linked_issue_ids(&providers);
    assert_eq!(ids, vec!["7"]);
}

#[test]
fn collect_linked_issue_ids_deduplicates() {
    let mut providers = ProviderData::default();
    // Same issue referenced from both a change request and a checkout
    providers.change_requests.insert("PR-1".into(), ChangeRequest {
        title: "Fix".into(),
        branch: "fix".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });
    providers.checkouts.insert(HostPath::new(HostName::new("host"), PathBuf::from("/tmp/co")).into(), Checkout {
        branch: "fix".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        host_name: None,
        environment_id: None,
    });

    let ids = collect_linked_issue_ids(&providers);
    assert_eq!(ids.len(), 1, "duplicate issue refs should be deduplicated");
    assert_eq!(ids[0], "42");
}

#[test]
fn collect_linked_issue_ids_empty_when_no_associations() {
    let providers = ProviderData::default();
    let ids = collect_linked_issue_ids(&providers);
    assert!(ids.is_empty());
}

/// When `ProviderData.issues` is populated (as it would be after
/// `fetch_missing_linked_issues`), correlation picks up the issue
/// references and includes them in the snapshot's work items.
#[test]
fn snapshot_includes_linked_issues_when_populated() {
    let host = HostName::new("test-host");
    let checkout_path = HostPath::new(host.clone(), PathBuf::from("/tmp/repo"));

    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone().into(), Checkout {
        branch: "fix/42".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::Branch("fix/42".into()), CorrelationKey::CheckoutPath(checkout_path.into())],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        host_name: None,
        environment_id: None,
    });
    providers.change_requests.insert("PR-100".into(), ChangeRequest {
        title: "Fix issue #42".into(),
        branch: "fix/42".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![CorrelationKey::Branch("fix/42".into()), CorrelationKey::ChangeRequestRef("github".into(), "100".into())],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });
    // Simulate fetch_missing_linked_issues having populated the issue
    providers.issues.insert("42".into(), Issue {
        title: "Something is broken".into(),
        labels: vec!["bug".into()],
        association_keys: vec![],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });

    let default_snap = RefreshSnapshot::default();
    let snapshot = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        1,
        None,
    );

    // The snapshot should have the issue in its provider data
    assert!(snapshot.providers.issues.contains_key("42"), "issue 42 should be present in snapshot providers");

    // Find the work item that correlates checkout + change request
    let work_item =
        snapshot.work_items.iter().find(|wi| wi.branch.as_deref() == Some("fix/42")).expect("should have a work item for fix/42");

    // The work item should reference issue 42
    assert!(
        work_item.issue_keys.contains(&"42".to_string()),
        "work item should reference linked issue 42, got: {:?}",
        work_item.issue_keys
    );
}

#[tokio::test]
async fn get_repo_providers_uses_preferred_root_environment_host_discovery_for_non_local_direct_repo() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::env_var("LOCAL_MARKER", "local")))
        .expect("replace local environment bag");

    let remote_environment_id = EnvironmentId::new("remote-direct-env");
    daemon
        .register_direct_environment_for_test(
            remote_environment_id.clone(),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("REMOTE_MARKER", "remote")),
            None,
        )
        .expect("register remote direct environment");

    let mut model = RepoModel::new(
        repo.clone(),
        crate::providers::registry::ProviderRegistry::new(),
        None,
        Some(remote_environment_id.clone()),
        None,
        shared_in_memory_attachable_store(),
        shared_in_memory_agent_state_store(),
    );
    model.data.loading = false;

    let identity = fallback_repo_identity(&repo);
    let root = RepoRootState { path: repo.clone(), model, slug: None, repo_bag: EnvironmentBag::new(), unmet: Vec::new(), is_local: true };

    {
        let mut repos = daemon.repos.write().await;
        let mut order = daemon.repo_order.write().await;
        repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
        order.push(identity.clone());
    }
    daemon.path_identities.write().await.insert(repo.clone(), identity);

    let providers = daemon.get_repo_providers_internal(&RepoSelector::Path(repo)).await.expect("repo providers should resolve");

    assert!(
        providers
            .host_discovery
            .iter()
            .any(|entry| entry.kind == "env_var_set" && entry.detail.get("key").map(String::as_str) == Some("REMOTE_MARKER")),
        "host discovery should report the preferred non-local direct environment bag"
    );
    assert!(
        !providers
            .host_discovery
            .iter()
            .any(|entry| entry.kind == "env_var_set" && entry.detail.get("key").map(String::as_str) == Some("LOCAL_MARKER")),
        "host discovery should not fall back to the daemon-local environment bag"
    );
}

#[tokio::test]
async fn convoy_completion_command_updates_convoy_task_status() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;
    let convoys = daemon.resource_backend().using::<Convoy>("flotilla");
    let created = convoys
        .create(&empty_input_meta("convoy-a"), &ConvoySpec {
            workflow_ref: "review-and-fix".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: Some("laptop-docker".to_string()),
            repository: None,
            r#ref: None,
            project_ref: None,
            adopted_checkout_ref: None,
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: None,
            work: [("implement".to_string(), WorkState {
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })]
            .into_iter()
            .collect(),
            crew_work: BTreeMap::new(),
            message: None,
            started_at: None,
            finished_at: None,
            observed_workflow_ref: Some("review-and-fix".to_string()),
            observed_workflows: None,
        })
        .await
        .expect("convoy status update should succeed");

    let mut events = daemon.subscribe();
    let result = force_complete_work(&daemon, &mut events).await;

    assert_eq!(result, CommandValue::Ok);
    let convoy = convoys.get("convoy-a").await.expect("convoy get should succeed");
    let status = convoy.status.expect("convoy status should exist");
    assert_eq!(status.work["implement"].phase, WorkPhase::Complete);
    assert_eq!(status.work["implement"].message.as_deref(), Some("done"));

    for phase in [WorkPhase::Complete, WorkPhase::Failed, WorkPhase::Cancelled] {
        let current = convoys.get("convoy-a").await.expect("convoy get should succeed");
        let mut status = current.status.expect("convoy status should exist");
        status.work.get_mut("implement").expect("implement work").phase = phase;
        convoys.update_status("convoy-a", &current.metadata.resource_version, &status).await.expect("convoy status update should succeed");

        assert_eq!(force_complete_work(&daemon, &mut events).await, CommandValue::Error {
            message: "convoy convoy-a work implement is already terminal".to_string()
        });
    }
}

#[tokio::test]
async fn convoy_create_with_adopted_checkout_creates_adopted_checkout_resource() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    let checkout_path = temp.path().join("repo");
    let remote = "git@github.com:flotilla-org/flotilla.git";
    init_git_repo_with_remote(&checkout_path, remote);

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), git_process_discovery(false), HostName::local()).await;

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "convoy-adopted".to_string(),
                workflow_ref: "scratch".to_string(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
                adopted_checkout: Some(Box::new(checkout_path.clone())),
            },
        })
        .await
        .expect("execute should return a command id");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(err) => panic!("unexpected event error: {err}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result");

    assert_eq!(result, CommandValue::ConvoyCreated { name: "convoy-adopted".to_string() });
    let convoy = daemon.resource_backend().using::<Convoy>("flotilla").get("convoy-adopted").await.expect("convoy should exist");
    assert_eq!(convoy.spec.repository.as_ref().map(|repo| repo.url.as_str()), Some(remote));
    assert_eq!(convoy.spec.r#ref.as_deref(), Some("main"));
    assert_eq!(convoy.spec.adopted_checkout_ref.as_deref(), Some("adopted-checkout-convoy-adopted"));

    let checkout = daemon
        .resource_backend()
        .using::<ResourceCheckout>("flotilla")
        .get("adopted-checkout-convoy-adopted")
        .await
        .expect("adopted checkout should exist");
    assert_eq!(checkout.metadata.lifecycle_authority().expect("authority should parse"), Some(LifecycleAuthority::Adopted));
    match checkout.spec {
        ResourceCheckoutSpec::Observed(spec) => {
            assert_eq!(spec.r#ref, "main");
            assert_eq!(spec.path, std::fs::canonicalize(&checkout_path).expect("canonical path").display().to_string());
            assert_eq!(
                spec.repo_ref,
                flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec").key()
            );
        }
        other => panic!("expected observed checkout spec, got {other:?}"),
    }
    let status = checkout.status.expect("adopted checkout should be ready");
    assert_eq!(status.phase, ResourceCheckoutPhase::Ready);
    assert_eq!(status.path.as_deref(), Some(std::fs::canonicalize(&checkout_path).expect("canonical path").to_string_lossy().as_ref()));
}

#[tokio::test]
async fn duplicate_adopted_convoy_create_does_not_repoint_existing_checkout() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    let checkout_a = temp.path().join("repo-a");
    let checkout_b = temp.path().join("repo-b");
    let remote = "git@github.com:flotilla-org/flotilla.git";
    init_git_repo_with_remote(&checkout_a, remote);
    init_git_repo_with_remote(&checkout_b, remote);

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), git_process_discovery(false), HostName::local()).await;
    let mut events = daemon.subscribe();

    let first_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "convoy-adopted".to_string(),
                workflow_ref: "scratch".to_string(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
                adopted_checkout: Some(Box::new(checkout_a.clone())),
            },
        })
        .await
        .expect("first execute should return a command id");
    assert_eq!(wait_for_command_result(&mut events, first_id).await, CommandValue::ConvoyCreated { name: "convoy-adopted".to_string() });

    let second_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "convoy-adopted".to_string(),
                workflow_ref: "scratch".to_string(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
                adopted_checkout: Some(Box::new(checkout_b)),
            },
        })
        .await
        .expect("second execute should return a command id");
    let result = wait_for_command_result(&mut events, second_id).await;
    assert!(matches!(result, CommandValue::Error { message } if message.contains("convoy convoy-adopted already exists")));

    let checkout = daemon
        .resource_backend()
        .using::<ResourceCheckout>("flotilla")
        .get("adopted-checkout-convoy-adopted")
        .await
        .expect("adopted checkout should still exist");
    match checkout.spec {
        ResourceCheckoutSpec::Observed(spec) => {
            assert_eq!(spec.path, std::fs::canonicalize(&checkout_a).expect("canonical path").display().to_string());
        }
        other => panic!("expected observed checkout spec, got {other:?}"),
    }
    let status = checkout.status.expect("adopted checkout should be ready");
    assert_eq!(status.path.as_deref(), Some(std::fs::canonicalize(&checkout_a).expect("canonical path").to_string_lossy().as_ref()));
}

#[tokio::test]
async fn convoy_completion_command_targets_configured_provisioning_namespace() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;
    daemon.set_provisioning_namespace("custom-ns".to_string()).await;

    let convoys = daemon.resource_backend().using::<Convoy>("custom-ns");
    let created = convoys
        .create(&empty_input_meta("convoy-a"), &ConvoySpec {
            workflow_ref: "review-and-fix".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: Some("laptop-docker".to_string()),
            repository: None,
            r#ref: None,
            project_ref: None,
            adopted_checkout_ref: None,
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: None,
            work: [("implement".to_string(), WorkState {
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })]
            .into_iter()
            .collect(),
            crew_work: BTreeMap::new(),
            message: None,
            started_at: None,
            finished_at: None,
            observed_workflow_ref: Some("review-and-fix".to_string()),
            observed_workflows: None,
        })
        .await
        .expect("convoy status update should succeed");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyWorkForceComplete {
                convoy: "convoy-a".to_string(),
                work: "implement".to_string(),
                message: Some("done".to_string()),
            },
        })
        .await
        .expect("execute should return a command id");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(err) => panic!("unexpected event error: {err}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result");

    assert_eq!(result, CommandValue::Ok);
    let convoy = convoys.get("convoy-a").await.expect("convoy get should succeed");
    let status = convoy.status.expect("convoy status should exist");
    assert_eq!(status.work["implement"].phase, WorkPhase::Complete);

    // The default namespace must NOT contain the convoy — completion should target
    // only the configured provisioning namespace, not the legacy hardcoded one.
    let default_convoys = daemon.resource_backend().using::<Convoy>("flotilla");
    let missing = default_convoys.get("convoy-a").await;
    assert!(missing.is_err(), "convoy should not exist in the default namespace: got {missing:?}");
}

#[tokio::test]
async fn normalize_local_provider_hosts_uses_mount_metadata_for_provisioned_checkouts() {
    struct TestProvisionedEnvironment {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
        mounts: Vec<ProvisionedMount>,
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

        fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
            self.mounts.clone()
        }

        async fn status(&self) -> Result<EnvironmentStatus, String> {
            Ok(EnvironmentStatus::Running)
        }

        async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
            Ok(HashMap::new())
        }

        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::clone(&self.runner)
        }

        async fn destroy(&self) -> Result<(), String> {
            Ok(())
        }
    }

    let local_environment_id = EnvironmentId::new("local-env");
    let local_host_id = HostId::new("local-host-id");
    let environment_manager = EnvironmentManager::from_local_state(
        local_environment_id,
        local_host_id.clone(),
        Arc::new(DiscoveryMockRunner::builder().build()),
        EnvironmentBag::new(),
    );

    let environment_id = EnvironmentId::new("provisioned-env");
    let handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
        id: environment_id.clone(),
        image: ImageId::new("image:test"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        mounts: vec![ProvisionedMount::new("/host/reference-repo", "/workspace/repo")],
    });
    environment_manager
        .register_provisioned_environment(environment_id.clone(), handle, EnvironmentBag::new(), None)
        .expect("register provisioned environment");

    let checkout_path = QualifiedPath::from_host_name(&HostName::local(), "/workspace/repo/feature");
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone(), Checkout {
        branch: "feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path.clone())],
        association_keys: vec![],
        host_name: None,
        environment_id: Some(environment_id.clone()),
    });

    let normalized = normalize_local_provider_hosts(providers, &environment_manager, Some(&environment_id), &HostName::local());
    let expected = QualifiedPath::host(local_host_id, "/host/reference-repo/feature");
    let checkout = normalized.checkouts.get(&expected).expect("mount-covered checkout should be host-qualified");

    assert_eq!(checkout.environment_id.as_ref(), Some(&environment_id));
    assert_eq!(checkout.correlation_keys, vec![CorrelationKey::CheckoutPath(expected.clone())]);
    assert!(
        !normalized.checkouts.contains_key(&checkout_path),
        "environment-local publication should be replaced by the host-qualified path"
    );
}

#[tokio::test]
async fn normalize_local_provider_hosts_preserves_host_qualified_checkout_when_provisioned_mount_lookup_misses() {
    let environment_manager = test_environment_manager();
    let environment_id = EnvironmentId::new("provisioned-env-miss");

    let checkout_path = QualifiedPath::host(HostId::new("persistent-host-id"), "/workspace/repo/feature");
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone(), Checkout {
        branch: "feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path.clone())],
        association_keys: vec![],
        host_name: None,
        environment_id: Some(environment_id.clone()),
    });

    let normalized = normalize_local_provider_hosts(providers, environment_manager, Some(&environment_id), &HostName::local());
    let checkout = normalized.checkouts.get(&checkout_path).expect("host-qualified checkout should be preserved");

    assert_eq!(checkout.environment_id.as_ref(), Some(&environment_id));
    assert_eq!(checkout.correlation_keys, vec![CorrelationKey::CheckoutPath(checkout_path.clone())]);
}

#[tokio::test]
async fn normalize_local_provider_hosts_keeps_environment_qualified_checkout_when_no_host_mapping_exists() {
    let local_environment_id = EnvironmentId::new("local-env-no-mount");
    let local_host_id = HostId::new("local-host-id-no-mount");
    let environment_manager = EnvironmentManager::from_local_state(
        local_environment_id,
        local_host_id,
        Arc::new(DiscoveryMockRunner::builder().build()),
        EnvironmentBag::new(),
    );

    let environment_id = EnvironmentId::new("provisioned-env-no-mount");
    let checkout_path = QualifiedPath::environment(environment_id.clone(), "/workspace/repo/feature");
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone(), Checkout {
        branch: "feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path.clone())],
        association_keys: vec![],
        host_name: None,
        environment_id: Some(environment_id.clone()),
    });

    let normalized = normalize_local_provider_hosts(providers, &environment_manager, Some(&environment_id), &HostName::local());
    let checkout = normalized.checkouts.get(&checkout_path).expect("environment-qualified checkout should remain environment-qualified");

    assert_eq!(checkout.environment_id.as_ref(), Some(&environment_id));
    assert_eq!(checkout.correlation_keys, vec![CorrelationKey::CheckoutPath(checkout_path.clone())]);
}

// --- subscribe_queries reads directly from the Aggregator's authoritative state ---

#[tokio::test]
async fn subscribe_queries_replays_result_set_from_aggregator_state() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    set_local_convoy_rows(&daemon, 7, vec![convoy_row("flotilla", "convoy-1", WireConvoyPhase::Active, None)]).await;

    let events =
        daemon.subscribe_queries(&[QueryCursor { query: QueryId::Convoys, since: None }]).await.expect("subscribe_queries should succeed");
    let result_set = events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Convoys => Some(result_set.clone()),
            _ => None,
        })
        .expect("expected ResultSet in subscribe replay");
    assert_eq!(result_set.seq, 7);
    let rows = result_set.rows.as_convoys().expect("convoy rows");
    assert_eq!(rows[0].name, "convoy-1");
}

#[tokio::test]
async fn subscribe_queries_skips_replay_when_cursor_matches_seq() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    set_local_convoy_rows(&daemon, 7, vec![convoy_row("flotilla", "convoy-1", WireConvoyPhase::Active, None)]).await;

    let events = daemon
        .subscribe_queries(&[QueryCursor { query: QueryId::Convoys, since: Some(7) }])
        .await
        .expect("subscribe_queries should succeed");
    assert!(!events.iter().any(|event| matches!(event, DaemonEvent::ResultSet(_))));
}

/// If the cursor is ahead of the daemon's current seq — e.g. after a daemon
/// restart that resets in-memory seq to 0 — the client still receives a full
/// result set (`==`, not `>=`). Regression guard for the conservative
/// behaviour documented on `DaemonHandle::subscribe_queries`.
#[tokio::test]
async fn subscribe_queries_resends_result_set_when_client_seq_is_ahead() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    set_local_convoy_rows(&daemon, 2, vec![convoy_row("flotilla", "convoy-1", WireConvoyPhase::Active, None)]).await;

    // Client's cursor is ahead of the daemon's seq — simulates daemon restart.
    let events = daemon
        .subscribe_queries(&[QueryCursor { query: QueryId::Convoys, since: Some(99) }])
        .await
        .expect("subscribe_queries should succeed");
    let result_set = events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Convoys => Some(result_set.clone()),
            _ => None,
        })
        .expect("client ahead of daemon must still receive a result set");
    assert_eq!(result_set.seq, 2, "result set reflects the daemon's current seq, not the client's stale claim");
}
