use flotilla_admission::support::placement_host_ref;
use flotilla_store_memory::engine_home_backend;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource};
use flotilla_resources::{
    Convoy as ResourceConvoy, CredentialConsumer, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle,
    CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec, FulfilmentFacts,
    FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, Host as ResourceHost, HostCondition, HostSpec, HostStatus,
    Project, ProjectSpec, ResourceProvenance, Selector, VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec,
    AGENT_ADAPTERS_CAPABILITY,
};

use super::support::{
    create_host_direct_host, create_host_direct_placement, create_identity_convoy, create_running_session, create_test_environment,
    ensure_vessel, host_direct_kind, host_direct_kind_spec, install_engine_messages, put_host, read_host, test_engine, test_meta,
    trusted_codex_workflow,
};
use crate::in_process::{placement_actuator_host_ref, placement_target_host, ConditionValue, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;
use flotilla_admission::convoy_admission::{
    placement_agent_adapters, resolve_workflow_credentials, validate_workflow_agent_adapters, validate_workflow_credentials,
    PlacementResolution,
};

/// A daemon resolving placements over its own store, with the directory holding it.
async fn placement_daemon(machine: &str) -> (Arc<InProcessDaemon>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), format!("machine_id = \"{machine}\"\n")).expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new(machine),
        engine_home_backend(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    (daemon, temp)
}

/// Resolves an unpinned convoy's placement at `daemon` through admission's
/// capability path.
async fn created_convoy_placement(daemon: &InProcessDaemon, workflow: &WorkflowTemplateSpec) -> Result<PlacementResolution, String> {
    daemon.admission.resolve_created_convoy_placement("flotilla", "flotilla", &[], workflow, None).await
}

/// Writes FulfilmentKind `name` at `home`, another daemon, and replicates it to `daemon`.
async fn put_kind_at_home(daemon: &InProcessDaemon, home: &str, name: &str, spec: &FulfilmentKindSpec) {
    let target = daemon.decided_store();
    let home = flotilla_provisioning_testkit::other_home_for(&target, home);
    home.kind(name, spec).await;
    home.replicate_to(&target);
}

#[tokio::test]
async fn remote_placement_resolves_replicated_credentials_before_admission() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = engine_home_backend();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("kiwi"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    super::test_engine(&daemon);
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
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
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
    put_host(
        &daemon,
        "udder-id",
        &HostSpec { display_name: "udder".to_string(), connection: Default::default(), ..HostSpec::default() },
        &HostStatus {
            capabilities: [
                (
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                ),
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["claude-code"])),
                (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"])),
                ("docker".to_string(), serde_json::json!(true)),
                ("os".to_string(), serde_json::json!("linux")),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            fulfilment_facts: BTreeMap::from([(
                "docker-udder-id".to_string(),
                FulfilmentFacts {
                    harnesses: BTreeMap::from([(
                        "claude-code".to_string(),
                        HarnessFacts { version: "2.0.0".to_string(), models: BTreeMap::new() },
                    )]),
                    observed_at: Utc::now(),
                    ..Default::default()
                },
            )]),
            ..HostStatus::default()
        },
    )
    .await;
    put_kind_at_home(
        &daemon,
        "udder-root",
        "docker-udder-id",
        &FulfilmentKindSpec::builder()
            .host_ref("udder-id".to_string())
            .pool("passthrough".to_string())
            .realisation(FulfilmentRealisation::DockerPerVessel {
                image: Some(flotilla_resources::PlacementImageSource::Literal("crew:latest".to_string())),
            })
            .build(),
    )
    .await;

    let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("andamento".to_string()).build();
    let (_, mut resolved_workflow) = daemon
        .resolve_workflow(
            "flotilla",
            "andamento",
            &backend.definitions::<Project>("flotilla").get("andamento").await.expect("project").spec,
            &[],
            &intent,
        )
        .await
        .expect("resolve admission workflow");
    let placement =
        backend.including_replicas::<FulfilmentKind>("flotilla").get("docker-udder-id").await.expect("placement replica").object;
    resolve_workflow_credentials(&backend, "flotilla", Some("andamento"), &[], &mut resolved_workflow)
        .await
        .expect("resolve replicated credential grant");
    // #3907: a subscription credential reaches its requester only through a binding.
    assert!(resolved_workflow.vessels[0].credential_refs.is_empty());
    let alternatives = flotilla_admission::account_access::validate_account_access(
        &backend,
        "flotilla",
        Some("andamento"),
        &[],
        &resolved_workflow,
        Some(&placement),
    )
    .await
    .expect("the replicated grant authorizes a held Claude account");
    assert_eq!(alternatives[0].usable().into_iter().map(|name| name.0.as_str()).collect::<Vec<_>>(), ["claude-max"]);
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
    let (kiwi, _kiwi_dir) = placement_daemon("kiwi-root").await;
    let (feta, _feta_dir) = placement_daemon("feta-root").await;
    host_direct_kind(&kiwi.resource_backend(), "kiwi-kind", "kiwi").await;
    host_direct_kind(&feta.resource_backend(), "feta-kind", "feta").await;
    let synced_at = Utc::now();
    for (destination, source) in [(&kiwi, &feta), (&feta, &kiwi)] {
        flotilla_provisioning_testkit::replicate_home_at(
            &source.decided_store(),
            source.node_id().as_str(),
            &destination.decided_store(),
            synced_at,
        );
    }
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let left = created_convoy_placement(&kiwi, &workflow).await.expect_err("kiwi candidates");
    let right = created_convoy_placement(&feta, &workflow).await.expect_err("feta candidates");
    assert_eq!(left, right);
    assert!(left.find("feta-kind").expect("feta refused") < left.find("kiwi-kind").expect("kiwi refused"), "{left}");
    for daemon in [&kiwi, &feta] {
        assert_eq!(daemon.resource_backend().using::<FulfilmentKind>("flotilla").list().await.expect("controller view").items.len(), 1);
    }
    let replica =
        kiwi.resource_backend().including_replicas::<FulfilmentKind>("flotilla").get("feta-kind").await.expect("replica provenance");
    // The engine keeps no per-record sync time: a replica's provenance names its home.
    assert!(
        matches!(&replica.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == feta.node_id()),
        "{:?}",
        replica.provenance
    );
}

#[tokio::test]
async fn placement_decision_prefers_local_home_copy_over_same_name_replica() {
    let (daemon, _dir) = placement_daemon("placement-resolver").await;
    create_host_direct_placement(&daemon, "shared-kind", "local-host", BTreeSet::new()).await;
    create_host_direct_host(&daemon, "replica-host", BTreeSet::new()).await;
    put_kind_at_home(&daemon, "remote-root", "shared-kind", &host_direct_kind_spec("replica-host")).await;

    let resolution =
        created_convoy_placement(&daemon, &WorkflowTemplateSpec::builder().vessels(Vec::new()).build()).await.expect("resolve placement");
    let selected = resolution.selected.expect("select local home kind");

    assert_eq!(placement_host_ref(&selected), Some("local-host"));
    assert!(resolution.viable_not_selected.is_empty(), "same-name replica must not remain as a second candidate");
}

#[tokio::test]
async fn placement_target_host_rejects_unknown_display_name() {
    let backend = EngineBackend::open("home");
    let policy = host_direct_kind(&backend, "unknown-host", "missing-host").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("unknown host alias must be rejected");
    assert_eq!(error, "placement `unknown-host` references unknown host `missing-host`");
}

#[tokio::test]
async fn placement_target_host_rejects_ambiguous_display_name() {
    let backend = EngineBackend::open("home");
    for host_id in ["host-id-a", "host-id-b"] {
        let spec = HostSpec { display_name: "shared-name".to_string(), connection: Default::default(), ..HostSpec::default() };
        backend.put_host_at_its_home(host_id, &spec, None).await;
    }
    let policy = host_direct_kind(&backend, "ambiguous-host", "shared-name").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("ambiguous host alias must be rejected");
    assert_eq!(error, "placement `ambiguous-host` host reference `shared-name` is ambiguous");
}

#[tokio::test]
async fn ssh_host_takes_only_pinned_work_and_routes_to_its_owner() {
    let (daemon, _dir) = placement_daemon("placement-resolver").await;
    let backend = daemon.resource_backend();
    let spec = HostSpec {
        display_name: "beaufort".to_string(),
        connection: flotilla_resources::HostConnection::SshHost {
            owning_daemon: "owner-host".to_string(),
            destination: "crew@beaufort.example".to_string(),
        },
        ..HostSpec::default()
    };
    put_host(
        &daemon,
        "ssh-host",
        &spec,
        &HostStatus {
            capabilities: BTreeMap::from([
                (
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                ),
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!([])),
                ("ssh_host".to_string(), serde_json::json!(true)),
                ("transport".to_string(), serde_json::json!("ssh")),
                ("placement".to_string(), serde_json::json!("host_direct_only")),
                ("owning_daemon".to_string(), serde_json::json!("owner-host")),
            ]),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        },
    )
    .await;
    let policy = host_direct_kind(&backend, "host-direct-ssh-host", "ssh-host").await;
    let trusted = WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("shell".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "sh".to_string() })
                .build()])
            .build()])
        .build();
    let unpinned = created_convoy_placement(&daemon, &trusted).await.expect_err("an unpinned convoy must not land on an SSH host");
    assert!(unpinned.contains(&policy.metadata.name), "{unpinned}");
    assert!(unpinned.contains("SSH host takes only convoys that pin its placement"), "{unpinned}");
    validate_workflow_agent_adapters(&backend, "flotilla", &trusted, Some(&policy), false)
        .await
        .expect("pinned trusted work is admissible");
    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("target host");
    assert_eq!(placement_actuator_host_ref(&backend, "flotilla", &target).await.expect("owner").as_str(), "owner-host");

    let agent_workflow = flotilla_resources::single_agent_workflow_spec();
    let refused = validate_workflow_agent_adapters(&backend, "flotilla", &agent_workflow, Some(&policy), false)
        .await
        .expect_err("SSH host has no central Codex login to copy");
    assert!(refused.contains("agent adapter `codex`"), "{refused}");

    // Unreachable is the owner's blocking condition: readiness is judged from it.
    let observed = read_host(&daemon, "ssh-host").await;
    let mut unreachable = observed.status.expect("observed SSH status");
    unreachable.conditions.push(
        HostCondition::builder()
            .condition_type("SshProbe")
            .value(ConditionValue::False)
            .reason("Unreachable")
            .message("ssh probe failed")
            .observed_at(Utc::now())
            .blocks_forge_observation(false)
            .build(),
    );
    put_host(&daemon, "ssh-host", &observed.spec, &unreachable).await;
    let unavailable = placement_agent_adapters(&backend, "flotilla", &policy, false).await.expect_err("unreachable host is not ready");
    assert!(unavailable.contains("not ready"), "{unavailable}");
}

// #4037 contract 4: a Codex crew is placeable on an SSH host while its owning daemon
// publishes a central Codex login, and refused with that reason while it publishes none.
// #4093: the host has neither harness on its PATH; its crews run the delivered pins, so
// Claude crews place there too.
#[tokio::test]
async fn codex_crews_place_on_an_ssh_host_only_while_its_owner_has_a_central_login() {
    for central_login in [None, Some(false), Some(true)] {
        // The SSH host is homed at its owning daemon.
        let backend = EngineBackend::open("owner-host");
        let hosts = backend.using::<ResourceHost>("flotilla");
        let connection = flotilla_resources::HostConnection::SshHost {
            owning_daemon: "owner-host".to_string(),
            destination: "crew@beaufort.example".to_string(),
        };
        let host = hosts.create(&test_meta("ssh-host"), &HostSpec { connection, ..HostSpec::default() }).await.expect("SSH Host");
        let mut capabilities = BTreeMap::from([
            (
                flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
            ),
            (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!([])),
            (flotilla_resources::SSH_HOST_CAPABILITY.to_string(), serde_json::json!(true)),
        ]);
        capabilities
            .extend(central_login.map(|held| (flotilla_resources::CODEX_CENTRAL_LOGIN_CAPABILITY.to_string(), serde_json::json!(held))));
        let status = HostStatus { capabilities, heartbeat_at: Some(Utc::now()), ready: true, ..HostStatus::default() };
        hosts.update_status(&host.metadata.name, &host.metadata.resource_version, &status).await.expect("SSH observation");
        let policy = host_direct_kind(&backend, "host-direct-ssh-host", "ssh-host").await;

        let placed =
            validate_workflow_agent_adapters(&backend, "flotilla", &flotilla_resources::single_agent_workflow_spec(), Some(&policy), false)
                .await;

        let claude = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".to_string())
                .crew(vec![CrewSpec::builder()
                    .role("coder".to_string())
                    .source(CrewSource::Agent {
                        selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                        prompt: None,
                        brief_template: None,
                    })
                    .build()])
                .build()])
            .build();
        validate_workflow_agent_adapters(&backend, "flotilla", &claude, Some(&policy), false)
            .await
            .expect("the pinned Claude harness is offered without a host install");

        match central_login {
            Some(true) => placed.expect("the owner's central login serves the host's Codex crews"),
            _ => assert_eq!(
                placed.expect_err("no central login to copy"),
                "workflow requires agent adapter `codex`, which is not available in placement `host-direct-ssh-host` \
                 (host `ssh-host`, whose owning daemon has no central Codex login)"
            ),
        }
    }
}

#[tokio::test]
async fn placement_refuses_unknown_host_without_blocking_tool_workflow() {
    let (daemon, _dir) = placement_daemon("placement-resolver").await;
    host_direct_kind(&daemon.resource_backend(), "a-unknown-host", "deleted-host").await;
    create_host_direct_placement(&daemon, "z-clean", "clean-host", BTreeSet::new()).await;
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("watcher".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "tail -f log".to_string() })
                .build()])
            .build()])
        .build();

    let resolution = created_convoy_placement(&daemon, &workflow).await.expect("clean candidate");
    assert_eq!(resolution.selected.expect("clean placement").metadata.name, "z-clean");
    assert_eq!(resolution.refused_candidates[0].kind, "a-unknown-host");
}

#[tokio::test]
async fn placement_error_lists_each_refusal_and_failed_host_condition_reason() {
    let (daemon, _dir) = placement_daemon("placement-resolver").await;
    for (policy_name, host_name, condition_reason, condition_message) in [
        ("host-direct-feta", "feta", "StoredObjectDecodeFailed", "ConvoyEnsure/quarantined-record failed typed decode"),
        ("host-direct-udder", "udder", "RestartBudgetExhausted", "resource controller stopped after repeated failures"),
    ] {
        create_host_direct_placement(&daemon, policy_name, host_name, BTreeSet::from(["codex".to_string()])).await;
        let host = read_host(&daemon, host_name).await;
        put_host(
            &daemon,
            host_name,
            &host.spec,
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
                    // This test admission condition does not disable forge reads.
                    .blocks_forge_observation(false)
                    .build()],
                ..HostStatus::default()
            },
        )
        .await;
    }

    let error = created_convoy_placement(&daemon, &trusted_codex_workflow()).await.expect_err("all placement candidates should be refused");

    assert!(error.starts_with("no fulfilment kind covers"), "{error}");
    for refusal in [
        "host-direct-feta: placement `host-direct-feta` host `feta` generation `feta-generation` is not ready: \
StoredObjectDecodeFailed: ConvoyEnsure/quarantined-record failed typed decode",
        "host-direct-udder: placement `host-direct-udder` host `udder` generation `udder-generation` is not ready: \
RestartBudgetExhausted: resource controller stopped after repeated failures",
    ] {
        assert!(error.contains(refusal), "{error}");
    }
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
        engine_home_backend(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let local_host = daemon.local_host_id().expect("local host id").to_string();
    let _messages = install_engine_messages(&daemon);
    let engine = test_engine(&daemon);
    let local_env = create_test_environment(&engine.environments, "local-env", &local_host);
    let ambiguous_env = create_test_environment(&engine.environments, "ambiguous-env", "shared-host");
    for host_id in ["shared-host-id-a", "shared-host-id-b"] {
        let spec = HostSpec { display_name: "shared-host".to_string(), connection: Default::default(), ..HostSpec::default() };
        put_host(&daemon, host_id, &spec, &HostStatus::default()).await;
    }
    create_running_session(&engine, &ambiguous_env, "terminal-ambiguous", "convoy-ambiguous", "watcher").await;
    create_running_session(&engine, &local_env, "terminal-local", "convoy-local", "watcher").await;

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
        engine_home_backend(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let host = daemon.local_host_id().expect("local host id").to_string();
    let _messages = install_engine_messages(&daemon);
    let engine = test_engine(&daemon);
    let env = create_test_environment(&engine.environments, "local-env", &host);
    let mut crew_one = String::new();
    let mut launches = BTreeMap::new();
    for (record, project) in [("convoy-one", "island-one"), ("convoy-two", "island-two")] {
        let convoy = create_identity_convoy(&daemon, record, record, project).await;
        launches.insert(record, convoy.clone());
        if record == "convoy-one" {
            let vessel = ensure_vessel(&engine, &convoy, "work");
            let launch = engine.terminals.request(&TerminalLaunchRequest {
                session: "terminal-convoy-one".to_string(),
                environment: env.clone(),
                vessel: Some(vessel),
                role: "coder".to_string(),
                pool: "passthrough".to_string(),
                cwd: "/repo".to_string(),
                env: Default::default(),
                source: TerminalLaunchSource::Agent {
                    capability: "code".to_string(),
                    adapter: Some("codex".to_string()),
                    model: None,
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief.md".to_string(),
                        content: "Work".to_string(),
                        copies: vec![],
                    },
                },
            });
            let facts = TerminalLaunchFacts {
                agent: Some(AgentLaunch { adapter: "codex".to_string(), model: None, stance: "coder".to_string() }),
                ..Default::default()
            };
            engine.terminals.ready(&launch, "session-one", &facts);
            // Its crew id is its launch's name.
            crew_one = launch;
        } else {
            create_running_session(&engine, &env, &format!("terminal-{record}"), &convoy, "coder").await;
        }
    }

    let fleet = daemon.scoped_fleet_list(None, None, None).await.expect("fleet");
    assert_eq!(fleet.rows.len(), 2);
    let (one, two) = (launches["convoy-one"].as_str(), launches["convoy-two"].as_str());
    let scoped = daemon.scoped_fleet_list(None, None, Some(one)).await.expect("crew convoy scope");
    assert_eq!(scoped.rows.len(), 1);
    assert_eq!(scoped.rows[0].convoy_ref.as_deref(), Some(one));
    let by_crew = daemon.scoped_fleet_list(None, Some(&crew_one), None).await.expect("crew identity scope");
    assert_eq!(by_crew.rows.len(), 1);
    assert_eq!(by_crew.rows[0].convoy_ref.as_deref(), Some(one));
    assert!(daemon.scoped_fleet_list(None, Some("missing"), None).await.is_err());
    let explicit = daemon.scoped_fleet_list(Some("island-two"), None, Some(one)).await.expect("explicit scope");
    assert_eq!(explicit.rows.len(), 1);
    assert_eq!(explicit.rows[0].convoy_ref.as_deref(), Some(two));
    let explicit_with_stale_crew = daemon.scoped_fleet_list(Some("island-two"), Some("missing"), None).await.expect("explicit scope wins");
    assert_eq!(explicit_with_stale_crew.rows.len(), 1);
}
