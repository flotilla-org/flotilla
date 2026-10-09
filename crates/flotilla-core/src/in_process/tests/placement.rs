use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    Convoy as ResourceConvoy, CredentialConsumer, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle,
    CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec, Host as ResourceHost,
    HostCondition, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus, InMemoryBackend, InputMeta,
    PlacementPolicy, PlacementPolicySpec, Project, ProjectSpec, ResourceBackend, ResourceProvenance, Selector,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource,
    TerminalSessionSpec as ResourceTerminalSessionSpec, TerminalSessionStatus as ResourceTerminalSessionStatus, VesselRequirement,
    WorkflowTemplate, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY, CONVOY_LABEL,
};

use super::support::{
    create_host_direct_placement, create_identity_convoy, create_running_session, create_test_environment, placement_policy, test_meta,
    trusted_codex_workflow,
};
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::{
    default_convoy_placement_policy, resolve_workflow_credentials, validate_workflow_agent_adapters, validate_workflow_credentials,
};
use crate::in_process::{placement_actuator_host_ref, placement_host_ref, placement_target_host, ConditionValue, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;

#[tokio::test]
async fn default_remote_placement_resolves_replicated_credentials_before_admission() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("kiwi"),
        backend.clone(),
    )
    .await;
    backend
        .definitions::<Project>("flotilla")
        .create(
            &test_meta("andamento"),
            &ProjectSpec::builder().display_name("Andamento".to_string()).default_workflow_ref("govern".to_string()).build(),
        )
        .await
        .expect("project");
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "governor@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("credential declaration");
    backend
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("andamento-governor"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["andamento".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("credential grant");
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &test_meta("govern"),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("governor".to_string())
                        .source(CrewSource::Agent {
                            selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                            prompt: None,
                            brief_template: None,
                        })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow");
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta("udder-id"),
            &HostSpec { display_name: "udder".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("remote host");
    hosts
        .update_status(
            "udder-id",
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [
                    (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["claude-code"])),
                    (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"])),
                    ("docker".to_string(), serde_json::json!(true)),
                    ("os".to_string(), serde_json::json!("linux")),
                ]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                ..HostStatus::default()
            },
        )
        .await
        .expect("remote host capabilities");
    let placement_source = ResourceBackend::InMemory(InMemoryBackend::default());
    placement_source
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("docker-udder-id"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "udder-id".to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["claude-code".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("remote placement");
    backend
        .replica_writer::<PlacementPolicy>(NodeId::new("udder-root"), "flotilla")
        .replace(&placement_source.using::<PlacementPolicy>("flotilla").list().await.expect("list remote placement policies"), Utc::now())
        .await
        .expect("replicate remote placement policy");

    let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("andamento".to_string()).build();
    let (_, mut resolved_workflow) = daemon
        .resolve_convoy_admission_workflow(
            "flotilla",
            "andamento",
            &backend.definitions::<Project>("flotilla").get("andamento").await.expect("project").spec,
            &[],
            &intent,
        )
        .await
        .expect("resolve admission workflow");
    let placement =
        backend.including_replicas::<PlacementPolicy>("flotilla").get("docker-udder-id").await.expect("placement replica").object;
    resolve_workflow_credentials(&backend, "flotilla", Some("andamento"), &[], &mut resolved_workflow)
        .await
        .expect("resolve replicated credential grant");
    assert_eq!(resolved_workflow.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));
    validate_workflow_agent_adapters(&backend, "flotilla", &resolved_workflow, Some(&placement), false)
        .await
        .expect("placement should provide agent adapter");
    validate_workflow_credentials(&backend, "flotilla", &resolved_workflow, Some(&placement))
        .await
        .expect("placement should hold resolved credential");

    assert!(matches!(backend.using::<ResourceConvoy>("flotilla").list().await, Ok(list) if list.items.is_empty()));
}

#[tokio::test]
async fn placement_candidates_and_refusals_agree_across_roots() {
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("kiwi-root"));
    let feta = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("feta-root"));
    placement_policy(&kiwi, "kiwi-policy", "kiwi").await;
    placement_policy(&feta, "feta-policy", "feta").await;
    let synced_at = Utc::now();
    for (destination, source, origin) in [(&kiwi, &feta, "feta-root"), (&feta, &kiwi, "kiwi-root")] {
        destination
            .replica_writer::<PlacementPolicy>(NodeId::new(origin), "flotilla")
            .replace(&source.using::<PlacementPolicy>("flotilla").list().await.expect("local policies"), synced_at)
            .await
            .expect("replicate policies");
    }
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let left = default_convoy_placement_policy(&kiwi, "flotilla", None, &[], &workflow, None).await.expect("kiwi candidates");
    let right = default_convoy_placement_policy(&feta, "flotilla", None, &[], &workflow, None).await.expect("feta candidates");
    assert_eq!(left.refused_candidates, right.refused_candidates);
    assert_eq!(
        left.refused_candidates.iter().map(|candidate| candidate.policy_name.as_str()).collect::<Vec<_>>(),
        vec!["feta-policy", "kiwi-policy"]
    );
    for backend in [&kiwi, &feta] {
        assert_eq!(backend.using::<PlacementPolicy>("flotilla").list().await.expect("controller view").items.len(), 1);
    }
    let replica = kiwi.including_replicas::<PlacementPolicy>("flotilla").get("feta-policy").await.expect("replica provenance");
    assert_eq!(replica.provenance, ResourceProvenance::Replica { origin_root: NodeId::new("feta-root"), last_synced_at: synced_at });
}

#[tokio::test]
async fn placement_decision_prefers_local_home_copy_over_same_name_replica() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    for host in ["local-host", "replica-host"] {
        hosts
            .create(&test_meta(host), &HostSpec { display_name: host.to_string(), connection: Default::default(), ..HostSpec::default() })
            .await
            .expect("placement host");
    }

    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("shared-policy"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .priority(0)
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "local-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("local home policy");
    let replica_source = ResourceBackend::InMemory(InMemoryBackend::default());
    replica_source
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("shared-policy"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .priority(100)
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "replica-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("replica policy source");
    backend
        .replica_writer::<PlacementPolicy>(NodeId::new("remote-root"), "flotilla")
        .replace(&replica_source.using::<PlacementPolicy>("flotilla").list().await.expect("list replica policy"), Utc::now())
        .await
        .expect("replicate colliding policy");

    let resolution = default_convoy_placement_policy(
        &backend,
        "flotilla",
        None,
        &[],
        &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
        None,
    )
    .await
    .expect("resolve placement");
    let selected = resolution.selected.expect("select local home policy");

    assert_eq!(placement_host_ref(&selected), Some("local-host"));
    assert!(resolution.viable_not_selected.is_empty(), "same-name replica must not remain as a second candidate");
}

#[tokio::test]
async fn placement_target_host_rejects_unknown_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policy = placement_policy(&backend, "unknown-host", "missing-host").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("unknown host alias must be rejected");
    assert_eq!(error, "placement `unknown-host` references unknown host `missing-host`");
}

#[tokio::test]
async fn placement_target_host_rejects_ambiguous_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    for host_id in ["host-id-a", "host-id-b"] {
        hosts
            .create(
                &test_meta(host_id),
                &HostSpec { display_name: "shared-name".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("host");
    }
    let policy = placement_policy(&backend, "ambiguous-host", "shared-name").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("ambiguous host alias must be rejected");
    assert_eq!(error, "placement `ambiguous-host` host reference `shared-name` is ambiguous");
}

#[tokio::test]
async fn agentless_ssh_host_is_selected_for_trusted_work_and_routes_to_its_owner() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta("ssh-host"),
            &HostSpec {
                display_name: "beaufort".to_string(),
                connection: flotilla_resources::HostConnection::AgentlessSsh {
                    owning_daemon: "owner-host".to_string(),
                    destination: "crew@beaufort.example".to_string(),
                },
                ..HostSpec::default()
            },
        )
        .await
        .expect("SSH Host");
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: BTreeMap::from([
                    (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!([])),
                    ("agentless".to_string(), serde_json::json!(true)),
                    ("transport".to_string(), serde_json::json!("ssh")),
                    ("placement".to_string(), serde_json::json!("host_direct_only")),
                    ("owning_daemon".to_string(), serde_json::json!("owner-host")),
                ]),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                ..HostStatus::default()
            },
        )
        .await
        .expect("SSH observation");
    let policy = placement_policy(&backend, "host-direct-ssh-host", "ssh-host").await;
    let trusted = WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("shell".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "sh".to_string() })
                .build()])
            .build()])
        .build();
    let accepted = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted, None).await.expect("placement scoring");
    assert_eq!(accepted.selected.expect("SSH host selected").metadata.name, policy.metadata.name);
    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("target host");
    assert_eq!(placement_actuator_host_ref(&backend, "flotilla", &target).await.expect("owner").as_str(), "owner-host");

    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("docker-ssh-host"),
            &PlacementPolicySpec::builder()
                .pool("cleat".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "ssh-host".to_string(),
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
        .expect("synthetic Docker policy for SSH host");

    let agent_workflow = flotilla_resources::single_agent_workflow_spec();
    let refused = default_convoy_placement_policy(&backend, "flotilla", None, &[], &agent_workflow, None)
        .await
        .expect_err("agentless SSH host cannot run a local agent adapter");
    assert!(refused.contains("agent adapter `codex`"), "{refused}");

    backend.using::<PlacementPolicy>("flotilla").delete("docker-ssh-host").await.expect("remove synthetic Docker policy");

    let observed = hosts.get("ssh-host").await.expect("SSH Host");
    let mut unreachable = observed.status.expect("observed SSH status");
    unreachable.ready = false;
    hosts.update_status("ssh-host", &observed.metadata.resource_version, &unreachable).await.expect("failed SSH probe");
    let unavailable =
        default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted, None).await.expect("score unreachable host");
    assert!(unavailable.selected.is_none());
    assert!(unavailable.refused_candidates.iter().any(|candidate| candidate.reason.contains("not ready")));
}

#[tokio::test]
async fn default_placement_prefers_local_host_referenced_by_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_host_direct_placement(&backend, "host-direct-a-remote", "remote-host", BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(
            &test_meta("local-host-id"),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("local host");
    hosts
        .update_status(
            &local.metadata.name,
            &local.metadata.resource_version,
            &HostStatus {
                capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"]))].into_iter().collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                ..HostStatus::default()
            },
        )
        .await
        .expect("local host status");
    placement_policy(&backend, "host-direct-z-local", "local-host").await;

    let local_host_id = flotilla_protocol::CanonicalHostId::resolved("local-host-id");
    let resolution = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted_codex_workflow(), Some(&local_host_id))
        .await
        .expect("default placement");
    assert_eq!(resolution.selected.expect("viable placement").metadata.name, "host-direct-z-local");
    assert_eq!(resolution.viable_not_selected[0].reason, "fallback ordering preferred local policy `host-direct-z-local`");
}

#[tokio::test]
async fn default_placement_refuses_unknown_host_without_blocking_tool_workflow() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    placement_policy(&backend, "a-unknown-host", "deleted-host").await;
    create_host_direct_placement(&backend, "z-clean", "clean-host", BTreeSet::new()).await;
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("watcher".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "tail -f log".to_string() })
                .build()])
            .build()])
        .build();

    let resolution = default_convoy_placement_policy(&backend, "flotilla", None, &[], &workflow, None).await.expect("clean candidate");
    assert_eq!(resolution.selected.expect("clean placement").metadata.name, "z-clean");
    assert_eq!(resolution.refused_candidates[0].policy_name, "a-unknown-host");
}

#[tokio::test]
async fn default_placement_error_lists_each_refusal_and_failed_host_condition_reason() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    for (policy_name, host_name, condition_reason, condition_message) in [
        ("host-direct-feta", "feta", "StoredObjectDecodeFailed", "ConvoyEnsure/quarantined-record failed typed decode"),
        ("host-direct-udder", "udder", "RestartBudgetExhausted", "resource controller stopped after repeated failures"),
    ] {
        create_host_direct_placement(&backend, policy_name, host_name, BTreeSet::from(["codex".to_string()])).await;
        let hosts = backend.using::<ResourceHost>("flotilla");
        let host = hosts.get(host_name).await.expect("host");
        hosts
            .update_status(
                &host.metadata.name,
                &host.metadata.resource_version,
                &HostStatus {
                    capabilities: host.status.expect("host status").capabilities,
                    daemon_generation: Some(format!("{host_name}-generation")),
                    heartbeat_at: Some(Utc::now()),
                    ready: false,
                    conditions: vec![HostCondition::builder()
                        .condition_type("test")
                        .value(ConditionValue::False)
                        .reason(condition_reason)
                        .message(condition_message)
                        .observed_at(Utc::now())
                        .build()],
                    ..HostStatus::default()
                },
            )
            .await
            .expect("degraded host status");
    }

    let error = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted_codex_workflow(), None)
        .await
        .expect_err("all placement candidates should be refused");

    assert_eq!(
        error,
        "no placement policy satisfies adapter `codex`; candidates:\n\
- `host-direct-feta`: placement `host-direct-feta` host `feta` generation `feta-generation` is not ready: \
StoredObjectDecodeFailed: ConvoyEnsure/quarantined-record failed typed decode\n\
- `host-direct-udder`: placement `host-direct-udder` host `udder` generation `udder-generation` is not ready: \
RestartBudgetExhausted: resource controller stopped after repeated failures"
    );
}

#[tokio::test]
async fn default_placement_accepts_a_host_with_an_authorship_collision() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_host_direct_placement(&backend, "host-direct-feta", "feta", BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("feta").await.expect("host");
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: host.status.expect("host status").capabilities,
                heartbeat_at: Some(Utc::now()),
                ready: true,
                conditions: vec![HostCondition::builder()
                    .condition_type("ResourceReplication/AuthorshipCollision")
                    .value(ConditionValue::False)
                    .reason("HomeBoundRecordAuthoredAtMultipleRoots")
                    .message("Convoy/flotilla/standing-collision is authored at multiple roots")
                    .observed_at(Utc::now())
                    .blocks_readiness(false)
                    .build()],
                ..HostStatus::default()
            },
        )
        .await
        .expect("host status with advisory collision");

    let resolution = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted_codex_workflow(), None)
        .await
        .expect("standing authorship collisions must not freeze dispatch placement");

    assert_eq!(resolution.selected.expect("viable placement").metadata.name, "host-direct-feta");
}

#[tokio::test]
async fn fleet_list_falls_back_per_row_for_an_ambiguous_host_alias() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let local_host = daemon.local_host_id().expect("local host id").to_string();
    let local_env = create_test_environment(&daemon, "local-env", &local_host).await;
    let ambiguous_env = create_test_environment(&daemon, "ambiguous-env", "shared-host").await;
    let hosts = daemon.resource_backend().using::<ResourceHost>("flotilla");
    for host_id in ["shared-host-id-a", "shared-host-id-b"] {
        hosts
            .create(
                &test_meta(host_id),
                &HostSpec { display_name: "shared-host".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("ambiguous host");
    }
    create_running_session(&daemon, &ambiguous_env, "terminal-ambiguous", "convoy-ambiguous", "watcher").await;
    create_running_session(&daemon, &local_env, "terminal-local", "convoy-local", "watcher").await;

    let rows = daemon.fleet_list_internal().await.expect("fleet list").rows;
    let hosts_by_convoy = rows.into_iter().map(|row| (row.convoy, row.host)).collect::<BTreeMap<_, _>>();
    assert_eq!(hosts_by_convoy.get("convoy-ambiguous"), Some(&HostName::new("shared-host")));
    assert_eq!(hosts_by_convoy.get("convoy-local"), Some(&daemon.host_name));
}

#[tokio::test]
async fn fleet_list_scopes_rows_to_the_live_convoy_project() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let host = daemon.local_host_id().expect("local host id").to_string();
    let env = create_test_environment(&daemon, "local-env", &host).await;
    for (convoy, project) in [("convoy-one", "island-one"), ("convoy-two", "island-two")] {
        create_identity_convoy(&daemon.resource_backend(), convoy, convoy, Some(project)).await;
        if convoy == "convoy-one" {
            let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
            let name = "terminal-convoy-one";
            let created = terminals
                .create(
                    &InputMeta::builder()
                        .name(name.to_string())
                        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.to_string())]))
                        .build(),
                    &ResourceTerminalSessionSpec {
                        env_ref: env.clone(),
                        role: "coder".to_string(),
                        source: TerminalSessionSource::Agent {
                            selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                            brief: flotilla_resources::TerminalBrief {
                                artifact_digest: None,
                                path: "brief.md".to_string(),
                                content: "Work".to_string(),
                                copies: vec![],
                            },
                            context: Box::new(flotilla_resources::TerminalCrewContext {
                                namespace: "flotilla".to_string(),
                                convoy: convoy.to_string(),
                                vessel_ref: "vessel-one".to_string(),
                            }),
                            message: None,
                        },
                        cwd: "/repo".to_string(),
                        env: Default::default(),
                        pool: "passthrough".to_string(),
                    },
                )
                .await
                .expect("agent session");
            terminals
                .update_status(
                    name,
                    &created.metadata.resource_version,
                    &ResourceTerminalSessionStatus {
                        phase: ResourceTerminalSessionPhase::Running,
                        session_id: Some("session-one".to_string()),
                        crew: Some(flotilla_resources::CrewSessionStatus {
                            id: "crew-one".to_string(),
                            adapter: "codex".to_string(),
                            model: None,
                            stance: "coder".to_string(),
                        }),
                        ..Default::default()
                    },
                )
                .await
                .expect("agent status");
        } else {
            create_running_session(&daemon, &env, &format!("terminal-{convoy}"), convoy, "coder").await;
        }
    }

    let fleet = daemon.scoped_fleet_list(None, None, None).await.expect("fleet");
    assert_eq!(fleet.rows.len(), 2);
    let scoped = daemon.scoped_fleet_list(None, None, Some("convoy-one")).await.expect("crew convoy scope");
    assert_eq!(scoped.rows.len(), 1);
    assert_eq!(scoped.rows[0].convoy_ref.as_deref(), Some("convoy-one"));
    let by_crew = daemon.scoped_fleet_list(None, Some("crew-one"), None).await.expect("crew identity scope");
    assert_eq!(by_crew.rows.len(), 1);
    assert_eq!(by_crew.rows[0].convoy_ref.as_deref(), Some("convoy-one"));
    assert!(daemon.scoped_fleet_list(None, Some("missing"), None).await.is_err());
    let explicit = daemon.scoped_fleet_list(Some("island-two"), None, Some("convoy-one")).await.expect("explicit scope");
    assert_eq!(explicit.rows.len(), 1);
    assert_eq!(explicit.rows[0].convoy_ref.as_deref(), Some("convoy-two"));
    let explicit_with_stale_crew = daemon.scoped_fleet_list(Some("island-two"), Some("missing"), None).await.expect("explicit scope wins");
    assert_eq!(explicit_with_stale_crew.rows.len(), 1);
}
