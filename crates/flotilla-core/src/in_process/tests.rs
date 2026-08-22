use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeZone;
use flotilla_resources::{
    controller_patches, ConvoyEnsureStatus, ConvoyStatus, CredentialConsumer, CredentialExpiry, CredentialGrant, CredentialGrantSelector,
    CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec,
    CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, Environment as ResourceEnvironment, EnvironmentPhase,
    EnvironmentSpec as ResourceEnvironmentSpec, EnvironmentStatus as ResourceEnvironmentStatus, HostCondition, HostDirectEnvironmentSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus, PlacementPolicy, PlacementPolicySpec, Selector,
    Stance, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, VesselRequirement, VirtualClock, WorkflowTemplateSpec,
    AGENT_ADAPTERS_CAPABILITY, CONVOY_LABEL, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};

use super::*;
use crate::providers::{
    discovery::test_support::{fake_discovery, fake_discovery_with_provider_set, FakeDiscoveryProviders, FakeTerminalPool},
    terminal::{managed_session_name, ManagedSessionMetadata, TerminalSession},
};

#[test]
fn completed_claims_without_a_decision_ledger_are_flagged_not_hidden() {
    let claimed_at = chrono::Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).single().expect("timestamp");
    let status = ConvoyStatus {
        crew_work: BTreeMap::from([(
            "work".to_string(),
            BTreeMap::from([
                ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(claimed_at).build()),
                (
                    "reviewer".to_string(),
                    CrewWorkState::builder()
                        .phase(CrewWorkPhase::Done)
                        .finished_at(claimed_at)
                        .decision_ledger_ref("https://example.test/pull/1#comment-2".to_string())
                        .build(),
                ),
            ]),
        )]),
        ..Default::default()
    };

    let ledgers = explained_decision_ledgers(Some(&status));
    assert_eq!(ledgers.len(), 2);
    assert!(ledgers.iter().any(|ledger| ledger.role == "coder" && ledger.missing && ledger.comment_url.is_none()));
    assert!(ledgers.iter().any(|ledger| {
        ledger.role == "reviewer" && !ledger.missing && ledger.comment_url.as_deref() == Some("https://example.test/pull/1#comment-2")
    }));
}

fn test_meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

#[tokio::test]
async fn contained_codex_to_claude_handoff_stages_credentials_for_the_latent_reviewer() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"two-crew-contained-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let repository = RepositoryKey("github.com-flotilla-org-flotilla".to_string());
    let requirement = VesselRequirement::builder()
        .name("work".to_string())
        .stance(Stance::Contained)
        .credential_refs(BTreeSet::from(["claude-max".to_string(), "github-crew-pr".to_string()]))
        .credential_scopes(BTreeMap::from([
            ("claude-max".to_string(), BTreeSet::from([repository.clone()])),
            ("github-crew-pr".to_string(), BTreeSet::from([repository])),
        ]))
        .crew(vec![
            CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build(),
            CrewSpec::builder()
                .role("reviewer".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code-review".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: Some("Review the coder's implementation.".to_string()),
                    brief_template: None,
                })
                .build(),
        ])
        .build();

    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(&test_meta("convoy-two-crew"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
        .await
        .expect("convoy");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                exit: None,
                turn_delivery: Default::default(),
                vessels: vec![requirement.clone()],
            }),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([
                    ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ("reviewer".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
                ]),
            )]),
            ..Default::default()
        })
        .await
        .expect("active convoy");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(&test_meta("convoy-two-crew-work"), &flotilla_resources::VesselSpec {
            convoy_ref: "convoy-two-crew".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "contained".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel");

    let coder_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .vessel_index(0)
        .crew_index(0)
        .build();
    let coder_meta = terminal_meta_with_vessel_credentials(coder_identity.input_meta(), &requirement);
    backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(&coder_meta, &ResourceTerminalSessionSpec {
            env_ref: "contained-env".to_string(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Agent {
                selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".to_string(),
                    content: "Implement the issue.".to_string(),
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".to_string(),
                    convoy: "convoy-two-crew".to_string(),
                    vessel_ref: "convoy-two-crew-work".to_string(),
                }),
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "contained".to_string(),
        })
        .await
        .expect("eager coder terminal");

    daemon
        .crew_handoff_internal(
            &CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".to_string()),
                convoy: Some("convoy-two-crew".to_string()),
                vessel_ref: Some("convoy-two-crew-work".to_string()),
                role: Some("coder".to_string()),
            },
            "reviewer",
            "Please review the implementation.",
        )
        .await
        .expect("handoff to latent reviewer");

    let reviewer = backend
        .using::<ResourceTerminalSession>("flotilla")
        .get("terminal-convoy-two-crew-work-reviewer")
        .await
        .expect("latent reviewer terminal");
    let TerminalSessionSource::Agent { selector, .. } = &reviewer.spec.source else {
        panic!("reviewer must be an agent session");
    };
    assert_eq!(selector.adapter.as_deref(), Some("claude-code"));
    assert_eq!(reviewer.spec.env_ref, "contained-env");
    assert_eq!(reviewer.metadata.annotations.get(CREDENTIAL_REFS_ANNOTATION), Some(&r#"["claude-max","github-crew-pr"]"#.to_string()));
    assert_eq!(
        reviewer.metadata.annotations.get(CREDENTIAL_SCOPES_ANNOTATION),
        Some(&r#"{"claude-max":["github.com-flotilla-org-flotilla"],"github-crew-pr":["github.com-flotilla-org-flotilla"]}"#.to_string())
    );
    assert_eq!(reviewer.metadata.annotations, coder_meta.annotations);
}

async fn create_identity_convoy(backend: &ResourceBackend, record: &str, role: &str, project: Option<&str>) {
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

async fn seed_convoy_routing_row(
    daemon: &InProcessDaemon,
    record: &str,
    role: Option<&str>,
    project: Option<&str>,
    phase: flotilla_protocol::ConvoyPhase,
) {
    let resource = flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", record).on_host(daemon.host_name.clone());
    let mut row = flotilla_protocol::ConvoyRow::builder()
        .resource(resource.clone())
        .maybe_address_role(role.map(str::to_string))
        .name(role.unwrap_or(record).to_string())
        .workflow_ref("review".to_string())
        .phase(phase)
        .build();
    row.project_ref = project.map(str::to_string);
    daemon.aggregator_projection_state().await.write().await.local_rows.insert(resource, row);
}

#[test]
fn convoy_role_addresses_reject_malformed_values() {
    assert_eq!(parse_role_address("reviewer"), Ok(("reviewer", None)));
    assert_eq!(parse_role_address("reviewer@flotilla"), Ok(("reviewer", Some("flotilla"))));
    assert_eq!(parse_role_address("reviewer@"), Ok(("reviewer", Some(""))));
    for value in ["@project", "a@b@c"] {
        assert_eq!(parse_role_address(value), Err(format!("invalid convoy address `{value}`: expected role@project")));
    }
    assert_eq!(parse_role_address(""), Err("convoy role cannot be empty".to_string()));
}

#[test]
fn qualified_role_address_is_a_typed_project_role_pair() {
    assert_eq!(
        RoleAddress::from_str("governor@andamento"),
        Ok(RoleAddress { project: "andamento".to_string(), role: "governor".to_string() })
    );
    for value in ["governor", "@andamento", "governor@", "governor@andamento@extra"] {
        assert!(RoleAddress::from_str(value).is_err(), "{value} must not produce a qualified address");
    }
}

#[test]
fn managed_terminal_changes_are_field_scoped_and_deduplicated() {
    let id = flotilla_protocol::AttachableId::new("pane-1");
    let running = ManagedTerminal {
        set_id: flotilla_protocol::AttachableSetId::new("set-1"),
        role: "server".to_string(),
        command: "npm start".to_string(),
        working_directory: "/work/flotilla".into(),
        status: flotilla_protocol::TerminalStatus::Running,
        attention: None,
    };
    let current = HashMap::from([(id.clone(), running.clone())]);
    assert!(matches!(
        managed_terminal_changes(None, &current).as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Added(terminal) }] if key == &id && terminal == &running
    ));
    assert!(managed_terminal_changes(Some(&current), &current).is_empty());

    let mut exited = running;
    exited.status = flotilla_protocol::TerminalStatus::Exited(7);
    exited.attention = Some(flotilla_protocol::PaneExitAttention { exit_code: 7 });
    let updated = HashMap::from([(id.clone(), exited.clone())]);
    assert!(matches!(
        managed_terminal_changes(Some(&current), &updated).as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Updated(terminal) }] if key == &id && terminal == &exited
    ));
    assert!(matches!(
        managed_terminal_changes(Some(&updated), &HashMap::new()).as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Removed }] if key == &id
    ));
}

#[tokio::test]
async fn managed_terminal_refresh_assigns_nested_cwd_to_most_specific_repo() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"pane-owner-test\"\n").expect("daemon config");
    let canonical_outer = temp.path().join("private").join("outer");
    let canonical_inner = canonical_outer.join("nested");
    let configured_outer = temp.path().join("outer-alias");
    let configured_inner = configured_outer.join("nested");
    std::fs::create_dir_all(&canonical_inner).expect("nested repository roots");
    std::os::unix::fs::symlink(&canonical_outer, &configured_outer).expect("configured repository alias");

    let pool = Arc::new(FakeTerminalPool::new());
    let terminal_id = flotilla_protocol::AttachableId::new("pane-1");
    let working_directory = ExecutionEnvironmentPath::new(canonical_inner.join("app"));
    let metadata = ManagedSessionMetadata::builder()
        .set_id(flotilla_protocol::AttachableSetId::new("set-1"))
        .attachable_id(terminal_id.clone())
        .checkout("nested".to_string())
        .role("server".to_string())
        .index(0)
        .working_directory(working_directory.clone())
        .build();
    pool.add_sessions(vec![TerminalSession {
        session_name: managed_session_name(&metadata),
        status: flotilla_protocol::TerminalStatus::Exited(7),
        command: Some("npm start".to_string()),
        working_directory: Some(working_directory),
        screen_activity: None,
    }])
    .await;
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(pool.clone()));
    let daemon = InProcessDaemon::new(
        vec![configured_outer, configured_inner.clone()],
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("local-host"),
    )
    .await;
    let mut events = daemon.subscribe();

    daemon.refresh_managed_terminal_attention().await;

    let deltas = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DaemonEvent::RepoDelta(delta) => Some(delta),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 1, "one pane must be attributed to one repository");
    assert_eq!(deltas[0].repo_identity, fallback_repo_identity(&configured_inner));
    assert!(matches!(
        deltas[0].changes.as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Added(terminal) }]
            if key == &terminal_id && terminal.attention == Some(flotilla_protocol::PaneExitAttention { exit_code: 7 })
    ));
}

#[tokio::test]
async fn attach_resolves_role_addresses_to_the_live_record_before_planning_the_hop() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    create_identity_convoy(&backend, "convoy-andamento", "governor", Some("andamento")).await;
    create_identity_convoy(&backend, "convoy-flotilla", "governor", Some("flotilla")).await;
    let local_host = daemon.local_host_id().expect("local host identity").to_string();
    backend
        .using::<ResourceHost>("flotilla")
        .create(&test_meta(&local_host), &HostSpec { display_name: "standing-test".to_string() })
        .await
        .expect("local host resource");
    let environment = create_test_environment(&daemon, "governor-env", &local_host).await;
    create_running_session(&daemon, &environment, "governor-session", "convoy-andamento", "governor").await;

    let contextual = daemon
        .resolve_attach_with_context("governor", None, false, AttachMode::Default, Some("andamento"))
        .await
        .expect("bare role resolves inside project context");
    assert_eq!(contextual.binding.as_ref().and_then(|binding| binding.convoy.as_deref()), Some("convoy-andamento"));
    assert!(matches!(contextual.plan.0.as_slice(), [ResolvedAttachAction::Command(_)]));

    let ambiguous = daemon
        .resolve_attach_with_context("governor", None, false, AttachMode::Default, None)
        .await
        .expect_err("bare fleet context must refuse ambiguity");
    assert_eq!(ambiguous, "governor is ambiguous: governor@andamento, governor@flotilla");

    let qualified = daemon
        .resolve_attach_with_context("governor@andamento", None, false, AttachMode::Default, None)
        .await
        .expect("qualified role resolves without project context");
    assert_eq!(qualified.binding.as_ref().and_then(|binding| binding.convoy.as_deref()), Some("convoy-andamento"));

    let from_untracked_repo = daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(flotilla_protocol::RepoSelector::Path(PathBuf::from("/scratch/untracked"))),
                action: CommandAction::Attach { reference: "governor@andamento".to_string(), host: None, mode: AttachMode::Default },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("untracked cwd context must not abort attach");
    assert!(matches!(from_untracked_repo, CommandValue::AttachCommandResolved { .. }));

    let session_in_project_context = daemon
        .resolve_attach_with_context("governor-session", None, false, AttachMode::Default, Some("andamento"))
        .await
        .expect("non-role references must fall back to the attach index in project context");
    assert_eq!(session_in_project_context.binding.as_ref().and_then(|binding| binding.session.as_deref()), Some("governor-session"));

    let wrong_host = daemon
        .resolve_attach_with_context("governor@andamento", Some(&HostName::new("udder")), false, AttachMode::Default, None)
        .await
        .expect_err("an explicit host must constrain role-address resolution");
    assert_eq!(wrong_host, "no attach target matching 'governor@andamento' on host 'udder'");
}

#[test]
fn remote_fleet_attach_references_use_the_canonical_role_address() {
    let row = FleetListRow::builder()
        .convoy("reviewer @ flotilla")
        .convoy_ref("convoy-opaque")
        .vessel("convoy-opaque-implement")
        .crew("implement/coder")
        .crew_state("running")
        .host(HostName::new("remote"))
        .namespace("flotilla")
        .session("session-opaque")
        .staleness(FleetStaleness::Fresh { last_sync: Utc::now() })
        .build();

    let references = fleet_row_attach_reference_keys(&row);
    assert!(references.contains(&"reviewer@flotilla".to_string()));
    assert!(references.contains(&"reviewer@flotilla/implement/coder".to_string()));
    assert!(references.contains(&"convoy-opaque".to_string()));
    assert!(!references.contains(&"reviewer @ flotilla".to_string()));
    assert_eq!(fleet_row_attach_reference_label(&row), "reviewer @ flotilla/implement/coder (remote)");
}

#[tokio::test]
async fn convoy_role_resolution_can_disambiguate_a_projectless_convoy() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", None).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("beta")).await;

    assert_eq!(
        resolve_local_convoy_name(&backend, "flotilla", "reviewer").await,
        Err("convoy role `reviewer` is ambiguous; use one of: reviewer@, reviewer@beta".to_string())
    );
    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@").await, Ok("convoy-one".to_string()));
    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@beta").await, Ok("convoy-two".to_string()));
}

#[tokio::test]
async fn convoy_resolution_falls_back_to_a_unique_terminal_generation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.get("convoy-one").await.expect("terminal convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Landed,
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@flotilla").await, Ok("convoy-one".to_string()));
}

#[tokio::test]
async fn convoy_resolution_refuses_multiple_terminal_generations() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for name in ["convoy-one", "convoy-two"] {
        let created = convoys.get(name).await.expect("terminal convoy");
        convoys
            .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                ..Default::default()
            })
            .await
            .expect("mark convoy terminal");
    }

    assert_eq!(
        resolve_local_convoy_name(&backend, "flotilla", "reviewer@flotilla").await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn convoy_routing_falls_back_to_a_unique_terminal_generation_and_refuses_multiple() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route sole terminal generation").expect("routing target");
    assert_eq!(target.home, daemon.host_name);

    seed_convoy_routing_row(&daemon, "convoy-two", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Failed).await;
    assert_eq!(
        daemon.resolve_existing_convoy_target(&action).await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn convoy_routing_prefers_an_exact_terminal_pre_identity_record() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    seed_convoy_routing_row(&daemon, "pre-identity-record", None, None, flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "pre-identity-record".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route exact terminal record").expect("routing target");
    assert_eq!(target.home, daemon.host_name);
}

#[tokio::test]
async fn convoy_routing_does_not_treat_a_legacy_display_name_as_role_identity() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    seed_convoy_routing_row(&daemon, "reviewer", None, Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route explicit role identity").expect("routing target");
    assert_eq!(target.home, daemon.host_name);
}

#[tokio::test]
async fn convoy_resolution_prefers_an_exact_unlabelled_record_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("pre-identity-record".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
        )
        .await
        .expect("pre-identity convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Landed,
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "pre-identity-record").await, Ok("pre-identity-record".to_string()));
}

#[tokio::test]
async fn convoy_explain_addresses_an_exact_terminal_pre_identity_record() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("pre-identity-record".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
        )
        .await
        .expect("pre-identity convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal");

    let explanation = daemon.explain_convoy_internal(None, "pre-identity-record").await.expect("explain terminal record");
    assert_eq!(explanation.convoy, "pre-identity-record");
    assert_eq!(explanation.phase, "Failed");
}

#[tokio::test]
async fn convoy_explain_refuses_multiple_terminal_generations() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for name in ["convoy-one", "convoy-two"] {
        let created = convoys.get(name).await.expect("terminal convoy");
        convoys
            .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landed,
                ..Default::default()
            })
            .await
            .expect("mark convoy terminal");
    }

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer@flotilla").await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn convoy_explain_rejects_projectless_and_project_bound_role_ambiguity() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    create_identity_convoy(&backend, "convoy-one", "reviewer", None).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("beta")).await;

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer").await.expect_err("bare role must be ambiguous"),
        "convoy role `reviewer` is ambiguous; use one of: reviewer@, reviewer@beta"
    );
}

#[tokio::test]
async fn projectless_convoys_do_not_share_an_identity_bucket_with_a_project_named_standalone() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let record = convoy_record_name();
    let generation = allocate_convoy_generation(&backend, "flotilla", None, "worker").await.expect("projectless identity");
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), String::new()),
        (ROLE_LABEL.to_string(), "worker".to_string()),
        (GENERATION_LABEL.to_string(), generation.to_string()),
    ]);
    let spec = ConvoySpec::builder().workflow_ref("work".to_string()).role("worker".to_string()).generation(generation).build();
    convoys.create(&InputMeta::builder().name(record).labels(labels).build(), &spec).await.expect("projectless convoy");

    assert!(allocate_convoy_generation(&backend, "flotilla", Some("standalone"), "worker").await.is_ok());
}

async fn standing_ensure_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<VirtualClock>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"standing-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).single().expect("timestamp");
    let clock = Arc::new(VirtualClock::new(now));
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        backend.clone(),
        clock.clone(),
    )
    .await;
    let repository_spec = RepositorySpec::remote("https://github.com/acme/standing").expect("repository spec");
    let repository_key = repository_spec.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository_spec).await.expect("repository");
    backend
        .definitions::<Project>("flotilla")
        .create(
            &test_meta("standing-project"),
            &ProjectSpec::builder()
                .display_name("Standing Project".to_string())
                .default_workflow_ref("quartermaster".to_string())
                .repositories(vec![ProjectRepositorySpec {
                    repo: repository_key.clone(),
                    alias: Some("app".to_string()),
                    roles: BTreeSet::from([ProjectRepositoryRole::Code]),
                    subpath: None,
                    default_branch: Some("main".to_string()),
                }])
                .build(),
        )
        .await
        .expect("project");
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &test_meta("quartermaster"),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .stance(Stance::Trusted)
                    .repository_refs(vec![repository_key.clone()])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("standing workflow");
    backend
        .definitions::<ConvoyEnsure>("flotilla")
        .create(
            &InputMeta::builder()
                .name("quartermaster".to_string())
                .annotations(BTreeMap::from([
                    (MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string()),
                    (SOURCE_REPOSITORY_ANNOTATION.to_string(), repository_key.to_string()),
                    (SOURCE_COMMIT_ANNOTATION.to_string(), "abc123".to_string()),
                    (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), "ops/quartermaster.md".to_string()),
                ]))
                .build(),
            &ConvoyEnsureSpec {
                project_ref: "standing-project".to_string(),
                role: "quartermaster".to_string(),
                workflow_ref: "quartermaster".to_string(),
                placement_policy: None,
                stance: Some(Stance::Trusted),
                repositories: vec![repository_key],
                presents_as: Some("fleet".to_string()),
            },
        )
        .await
        .expect("ensure declaration");
    (daemon, backend, clock, temp)
}

#[tokio::test]
async fn standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let first = convoys.get(&first_ref).await.expect("standing convoy");
    let now = clock.now();
    convoys
        .update_status(&first.metadata.name, &first.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            message: Some("provider registry unavailable".to_string()),
            started_at: Some(now),
            finished_at: Some(now),
            observed_workflow_ref: Some("quartermaster".to_string()),
            ..Default::default()
        })
        .await
        .expect("fail convoy after resolution loss");
    let environments = backend.using::<ResourceEnvironment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder()
                .name("quartermaster-work".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), first_ref.clone())]))
                .build(),
            &ResourceEnvironmentSpec {
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    host_ref: "local".to_string(),
                    image: "standing:latest".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                }),
            },
        )
        .await
        .expect("backing environment");
    environments
        .update_status(&environment.metadata.name, &environment.metadata.resource_version, &ResourceEnvironmentStatus {
            phase: EnvironmentPhase::Ready,
            ready: true,
            docker_container_id: Some("live-container".to_string()),
            ..Default::default()
        })
        .await
        .expect("mark backing live");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold live backing"), vec![
        "ConvoyEnsure/quartermaster held for operator attention"
    ]);
    clock.advance(ChronoDuration::hours(1));
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("continue holding").is_empty());
    assert!(convoys.get(&first_ref).await.is_ok(), "failed convoy and its live container must survive");
    let held = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("held ensure");
    assert_eq!(held.status.as_ref().expect("status").restart_count, 0);
    assert!(
        held.status.as_ref().expect("status").last_failure.as_deref().is_some_and(|failure| failure.contains("not verified dead")),
        "unexpected ensure status: {:?}",
        held.status
    );
    assert_eq!(backend.using::<ResourceDemand>("flotilla").list().await.expect("attention demands").items.len(), 1);

    let environment = environments.get("quartermaster-work").await.expect("backing environment");
    environments
        .update_status(&environment.metadata.name, &environment.metadata.resource_version, &ResourceEnvironmentStatus {
            phase: EnvironmentPhase::Failed,
            ready: false,
            message: Some("Docker container live-container is not running".to_string()),
            ..Default::default()
        })
        .await
        .expect("verify backing dead");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("record crash backoff");
    clock.advance(ChronoDuration::seconds(30));
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("restart dead backing"), vec![
        "started quartermaster@standing-project"
    ]);
    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    assert!(generations.iter().any(|convoy| convoy.metadata.name == first_ref && convoy.spec.generation == 1));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2));
    assert_eq!(backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 1);
}

#[tokio::test]
async fn abandoned_ensure_generation_survives_a_stale_reconcile_write_and_is_superseded() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let first_ref =
        ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref).expect("first convoy ref");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first = convoys.get(&first_ref).await.expect("first generation");
    let workflow_snapshot_ref =
        first.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).cloned().expect("workflow archive pointer");

    daemon.abandon_convoy_internal("flotilla", &first_ref, "operator requested replacement").await.expect("abandon generation");

    // This patch represents a reconcile that read the generation while it was
    // still active and lost the optimistic write race to the abandon command.
    // Retrying it against the newer status must not resurrect the generation.
    apply_resource_status_patch(&convoys, &first_ref, &controller_patches::roll_up_phase(ConvoyPhase::Active, Some(clock.now()), None))
        .await
        .expect("stale reconcile write");

    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("observe abandoned generation");
    clock.advance(ChronoDuration::seconds(30));
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("supersede abandoned generation"), vec![
        "started quartermaster@standing-project"
    ]);

    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    let abandoned = generations.iter().find(|convoy| convoy.metadata.name == first_ref).expect("abandoned history");
    let abandoned_status = abandoned.status.as_ref().expect("abandoned status");
    assert_eq!(abandoned.spec.generation, 1);
    assert_eq!(abandoned_status.phase, ConvoyPhase::Abandoned);
    assert_eq!(abandoned_status.message.as_deref(), Some("abandoned by HumanOverride: operator requested replacement"));
    assert_eq!(abandoned.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION), Some(&workflow_snapshot_ref));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2 && convoy.metadata.name != first_ref));
}

#[tokio::test]
async fn operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart() {
    let (daemon, backend, clock, temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let first_ref = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("first convoy ref");
    ensures
        .update_status(&ensure.metadata.name, &ensure.metadata.resource_version, &ConvoyEnsureStatus {
            convoy_ref: Some(first_ref.clone()),
            restart_count: 7,
            running_since: Some(clock.now()),
            retry_at: None,
            last_failure: None,
        })
        .await
        .expect("seed crash budget");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    convoys.delete(&first_ref).await.expect("operator reap");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("prompt resurrection"), vec![
        "started quartermaster@standing-project"
    ]);
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 7);

    backend.using::<WorkflowTemplate>("flotilla").delete("quartermaster").await.expect("temporary resolution loss");
    let second_ref =
        ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref).expect("second convoy ref");
    convoys.delete(&second_ref).await.expect("second operator reap");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("unresolved workflow schedules retry");
    let retrying = ensures.get("quartermaster").await.expect("retrying ensure");
    assert_eq!(retrying.status.as_ref().expect("status").restart_count, 7);
    let retry_at = retrying.status.as_ref().expect("status").retry_at.expect("durable retry time");

    let repository_key = backend.using::<Repository>("flotilla").list().await.expect("repositories").items[0].spec.key();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &test_meta("quartermaster"),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .stance(Stance::Trusted)
                    .repository_refs(vec![repository_key])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("restore workflow resolution");
    let restarted_daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        backend.clone(),
        clock.clone(),
    )
    .await;
    assert!(restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("retry not due").is_empty());
    clock.set(retry_at);
    assert_eq!(restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("past-due retry"), vec![
        "started quartermaster@standing-project"
    ]);
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 7);
}

#[test]
fn crew_attention_keeps_monitoring_distinct_from_lifecycle_state() {
    let now = Utc::now();
    let mut status = ResourceTerminalSessionStatus {
        phase: ResourceTerminalSessionPhase::Running,
        attention: Some(TerminalAttention { state: TerminalAttentionState::Idle, as_of: now, source: TerminalAttentionSource::Screen }),
        ..Default::default()
    };

    assert_eq!(crew_attention(Some(&status), true, now), Some(CrewAttention::Stalled));
    assert_eq!(crew_attention(Some(&status), false, now), Some(CrewAttention::Idle));

    status.degraded = Some(flotilla_resources::TerminalSessionDegradedCondition {
        reason: "DeliveryUnconfirmed".to_string(),
        message: "composer retained delivery".to_string(),
        message_id: Some("handoff-1".to_string()),
        consecutive_failures: 1,
        observed_at: now,
    });
    assert_eq!(crew_attention(Some(&status), true, now), Some(CrewAttention::DeliveryUnconfirmed));
    status.degraded = None;

    status.attention.as_mut().expect("attention").as_of = now - chrono::Duration::seconds(31);
    assert_eq!(crew_attention(Some(&status), true, now), Some(CrewAttention::Unobservable));

    status.phase = ResourceTerminalSessionPhase::Stopped;
    assert_eq!(crew_attention(Some(&status), true, now), None);
}

#[test]
fn handed_back_crew_is_settled_for_its_own_attention() {
    assert!(crew_work_unsettled(CrewWorkPhase::Working));
    assert!(!crew_work_unsettled(CrewWorkPhase::Done));
    assert!(!crew_work_unsettled(CrewWorkPhase::HandedBack));
    assert!(!crew_work_unsettled(CrewWorkPhase::Failed));
}

#[tokio::test]
async fn self_targeted_admission_uses_live_local_host_over_stale_self_origin_replica() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();

    let stale_source = ResourceBackend::InMemory(InMemoryBackend::default());
    stale_source
        .using::<ResourceHost>("flotilla")
        .create(&test_meta(&host_id), &HostSpec { display_name: "local-host".to_string() })
        .await
        .expect("stale self-origin host");
    backend
        .replica_writer::<ResourceHost>(daemon.node_id.clone(), "flotilla")
        .replace(&stale_source.using::<ResourceHost>("flotilla").list().await.expect("stale host list"), Utc::now())
        .await
        .expect("seed stale self-origin replica");

    let hosts = backend.using::<ResourceHost>("flotilla");
    let local =
        hosts.create(&test_meta(&host_id), &HostSpec { display_name: "local-host".to_string() }).await.expect("authoritative local host");
    hosts
        .update_status(&host_id, &local.metadata.resource_version, &HostStatus {
            disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
            admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
            ..HostStatus::default()
        })
        .await
        .expect("publish live local capacity");
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("self-targeted"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: host_id.clone(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("self-targeted placement policy");

    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                policy_name: "self-targeted".to_string(),
                target_host: PlacementTargetHost {
                    reference: flotilla_protocol::CanonicalHostId::resolved(host_id),
                    display_name: "local-host".to_string(),
                },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

#[tokio::test]
async fn resource_host_routing_refuses_unresolved_host_ref() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;

    let error = daemon
        .target_host_for_resource_ref("flotilla", "unregistered-host-id")
        .await
        .expect_err("unknown host refs must not cross the canonical identity boundary");

    assert_eq!(error, "references unknown host `unregistered-host-id`");
}

#[tokio::test]
async fn self_targeted_admission_resolves_display_name_policy_to_live_local_host() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();
    let hosts = backend.using::<ResourceHost>("flotilla");
    let local =
        hosts.create(&test_meta(&host_id), &HostSpec { display_name: "local-host".to_string() }).await.expect("authoritative local host");
    hosts
        .update_status(&host_id, &local.metadata.resource_version, &HostStatus {
            disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
            admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
            ..HostStatus::default()
        })
        .await
        .expect("publish live local capacity");
    let policy = backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("self-targeted"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "local-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("self-targeted placement policy");

    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("resolve display-name host reference");
    assert_eq!(target.reference.as_str(), host_id);
    assert_eq!(daemon.remote_placement_host("flotilla", Some("self-targeted")).await.expect("resolve host-direct routing"), None);
    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                policy_name: "self-targeted".to_string(),
                target_host: target,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

#[tokio::test]
async fn default_remote_placement_routes_before_admission() {
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
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "governor@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("credential declaration");
    backend
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("andamento-governor"),
            &CredentialGrantSpec::builder()
                .selector(
                    CredentialGrantSelector::builder().stance(Stance::Trusted).projects(BTreeSet::from(["andamento".to_string()])).build(),
                )
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
                    .stance(Stance::Trusted)
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
    let host = hosts.create(&test_meta("udder-id"), &HostSpec { display_name: "udder".to_string() }).await.expect("remote host");
    hosts
        .update_status("udder-id", &host.metadata.resource_version, &HostStatus {
            capabilities: [
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["claude-code"])),
                (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"])),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
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
                    host_ref: "udder-id".to_string(),
                    image: "crew:latest".to_string(),
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
    let (_, resolved_workflow) = daemon
        .resolve_convoy_admission_workflow(
            "flotilla",
            "andamento",
            &backend.definitions::<Project>("flotilla").get("andamento").await.expect("project").spec,
            &[],
            &intent,
            None,
        )
        .await
        .expect("resolve admission workflow");
    assert_eq!(resolved_workflow.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));
    let placement =
        backend.including_replicas::<PlacementPolicy>("flotilla").get("docker-udder-id").await.expect("placement replica").object;
    validate_workflow_agent_adapters(&backend, "flotilla", &resolved_workflow, Some(&placement))
        .await
        .expect("placement should provide agent adapter");
    validate_workflow_credentials(&backend, "flotilla", &resolved_workflow, Some(&placement))
        .await
        .expect("placement should hold resolved credential");

    let target = daemon
        .convoy_start_placement_host("flotilla", &intent)
        .await
        .expect("resolve default placement")
        .expect("default placement should be remote");

    assert_eq!(target.as_str(), "udder-id");
    assert!(matches!(backend.using::<ResourceConvoy>("flotilla").list().await, Ok(list) if list.items.is_empty()));
}

#[tokio::test]
async fn placement_decision_prefers_local_home_copy_over_same_name_replica() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    for host in ["local-host", "replica-host"] {
        hosts.create(&test_meta(host), &HostSpec { display_name: host.to_string() }).await.expect("placement host");
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

    let resolution =
        default_convoy_placement_policy(&backend, "flotilla", &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(), None)
            .await
            .expect("resolve placement");
    let selected = resolution.selected.expect("select local home policy");

    assert_eq!(placement_host_ref(&selected), Some("local-host"));
    assert!(resolution.viable_not_selected.is_empty(), "same-name replica must not remain as a second candidate");
}

async fn placement_policy(backend: &ResourceBackend, name: &str, host_ref: &str) -> ResourceObject<PlacementPolicy> {
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
        hosts.create(&test_meta(host_id), &HostSpec { display_name: "shared-name".to_string() }).await.expect("host");
    }
    let policy = placement_policy(&backend, "ambiguous-host", "shared-name").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("ambiguous host alias must be rejected");
    assert_eq!(error, "placement `ambiguous-host` host reference `shared-name` is ambiguous");
}

async fn create_host_direct_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, agent_adapters: BTreeSet<String>) {
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.create(&test_meta(host_ref), &HostSpec { display_name: host_ref.to_string() }).await.expect("host create");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(agent_adapters))].into_iter().collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("host status update");
    placement_policy(backend, policy_name, host_ref).await;
}

fn trusted_codex_workflow() -> WorkflowTemplateSpec {
    let mut workflow = flotilla_resources::single_agent_contained_workflow_spec();
    workflow.vessels[0].stance = Stance::Trusted;
    workflow
}

#[tokio::test]
async fn default_placement_prefers_local_host_referenced_by_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_host_direct_placement(&backend, "host-direct-a-remote", "remote-host", BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts.create(&test_meta("local-host-id"), &HostSpec { display_name: "local-host".to_string() }).await.expect("local host");
    hosts
        .update_status(&local.metadata.name, &local.metadata.resource_version, &HostStatus {
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"]))].into_iter().collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("local host status");
    placement_policy(&backend, "host-direct-z-local", "local-host").await;

    let local_host_id = flotilla_protocol::CanonicalHostId::resolved("local-host-id");
    let resolution = default_convoy_placement_policy(&backend, "flotilla", &trusted_codex_workflow(), Some(&local_host_id))
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
            .stance(Stance::Trusted)
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("watcher".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "tail -f log".to_string() })
                .build()])
            .build()])
        .build();

    let resolution = default_convoy_placement_policy(&backend, "flotilla", &workflow, None).await.expect("clean candidate");
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
            .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
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
            })
            .await
            .expect("degraded host status");
    }

    let error = default_convoy_placement_policy(&backend, "flotilla", &trusted_codex_workflow(), None)
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
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
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
        })
        .await
        .expect("host status with advisory collision");

    let resolution = default_convoy_placement_policy(&backend, "flotilla", &trusted_codex_workflow(), None)
        .await
        .expect("standing authorship collisions must not freeze dispatch placement");

    assert_eq!(resolution.selected.expect("viable placement").metadata.name, "host-direct-feta");
}

async fn create_test_environment(daemon: &InProcessDaemon, name: &str, host_ref: &str) -> String {
    daemon
        .resource_backend()
        .using::<ResourceEnvironment>("flotilla")
        .create(&test_meta(name), &ResourceEnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_ref.to_string(), repo_default_dir: "/tmp".to_string() }),
            docker: None,
        })
        .await
        .expect("environment");
    name.to_string()
}

async fn create_running_session(daemon: &InProcessDaemon, env_ref: &str, name: &str, convoy: &str, role: &str) {
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
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("terminal session");
    terminals
        .update_status(name, &created.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            session_id: Some(format!("session-{name}")),
            ..Default::default()
        })
        .await
        .expect("running session");
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
        hosts.create(&test_meta(host_id), &HostSpec { display_name: "shared-host".to_string() }).await.expect("ambiguous host");
    }
    create_running_session(&daemon, &ambiguous_env, "terminal-ambiguous", "convoy-ambiguous", "watcher").await;
    create_running_session(&daemon, &local_env, "terminal-local", "convoy-local", "watcher").await;

    let rows = daemon.fleet_list_internal().await.expect("fleet list").rows;
    let hosts_by_convoy = rows.into_iter().map(|row| (row.convoy, row.host)).collect::<BTreeMap<_, _>>();
    assert_eq!(hosts_by_convoy.get("convoy-ambiguous"), Some(&HostName::new("shared-host")));
    assert_eq!(hosts_by_convoy.get("convoy-local"), Some(&daemon.host_name));
}

async fn create_docker_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, held_credentials: BTreeSet<String>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.create(&test_meta(host_ref), &HostSpec { display_name: host_ref.to_string() }).await.expect("host create");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: [(flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(held_credentials))]
                .into_iter()
                .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            resource_store: None,
            ..HostStatus::default()
        })
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
                    host_ref: host_ref.to_string(),
                    image: "crew:latest".to_string(),
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

#[tokio::test]
async fn contained_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Contained)
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

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("contained Claude must not reach interactive login without OAuth");
    assert_eq!(
        error,
        "contained agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it"
    );

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-contained"),
            &CredentialGrantSpec::builder()
                .selector(
                    CredentialGrantSelector::builder().stance(Stance::Contained).projects(BTreeSet::from(["flotilla".to_string()])).build(),
                )
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("create project-selected Claude grant");
    let mut with_grant = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut with_grant)
        .await
        .expect("resolve matching Claude grant");
    assert_eq!(with_grant.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));

    create_docker_placement(&backend, "docker-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-claude").await.expect("get placement");
    validate_workflow_credentials(&backend, "flotilla", &with_grant, Some(&placement))
        .await
        .expect("matching held OAuth grant admits contained Claude");
}

#[tokio::test]
async fn remote_placement_uses_replicated_host_capabilities() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("kiwi-root"));
    let now = Utc::now();
    let feta = ResourceBackend::InMemory(InMemoryBackend::default());
    let feta_hosts = feta.using::<ResourceHost>("flotilla");
    let fresh = feta_hosts
        .create(&test_meta("feta-host"), &HostSpec { display_name: "feta".to_string() })
        .await
        .expect("create fresh feta self-report");
    feta_hosts
        .update_status(&fresh.metadata.name, &fresh.metadata.resource_version, &HostStatus {
            capabilities: [(
                flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(),
                serde_json::json!(BTreeSet::from(["claude-max".to_string()])),
            )]
            .into_iter()
            .collect(),
            heartbeat_at: Some(now - chrono::Duration::seconds(1)),
            ready: true,
            daemon_generation: Some("fresh-feta-generation".to_string()),
            daemon_started_at: Some(now - chrono::Duration::minutes(1)),
            ..HostStatus::default()
        })
        .await
        .expect("write fresh feta capabilities");
    backend
        .replica_writer::<ResourceHost>(NodeId::new("feta-root"), "flotilla")
        .replace(&feta_hosts.list().await.expect("list feta self-report"), Utc::now())
        .await
        .expect("replicate fresh feta self-report to kiwi");

    let sources = backend.including_replicas::<ResourceHost>("flotilla").list().await.expect("list host sources");
    assert_eq!(sources.items.len(), 1, "a Host should have only its home-authored source");

    let placement = backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("feta-docker"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    host_ref: "feta-host".to_string(),
                    image: "crew:latest".to_string(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::new(),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("create feta placement");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).stance(Stance::Contained).crew(Vec::new()).build()])
        .build();
    workflow.vessels[0].credential_refs = BTreeSet::from(["claude-max".to_string()]);

    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("kiwi-to-feta admission should use feta's fresh self-report");

    workflow.vessels[0].credential_refs.insert("github-crew-pr".to_string());
    let error = validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect_err("a real missing credential should still refuse admission");
    assert_eq!(
        error,
        "workflow requires credential `github-crew-pr`, which placement `feta-docker` host `feta` generation `fresh-feta-generation` does not hold"
    );
}

#[tokio::test]
async fn trusted_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Trusted)
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

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("trusted Claude must not reach ambient login without delivered OAuth");
    assert_eq!(error, "trusted agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-trusted"),
            &CredentialGrantSpec::builder()
                .selector(
                    CredentialGrantSelector::builder().stance(Stance::Trusted).projects(BTreeSet::from(["flotilla".to_string()])).build(),
                )
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("create project-selected trusted Claude grant");
    let mut with_grant = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut with_grant)
        .await
        .expect("resolve matching trusted Claude grant");
    assert_eq!(with_grant.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));

    create_docker_placement(&backend, "host-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("host-claude").await.expect("get placement");
    let expired_ambient = CredentialExpiry::builder().refresh_expires_at("2026-07-30T00:00:00Z".parse().expect("timestamp")).build();
    set_host_credential_expiry(
        &backend,
        "host-a",
        BTreeMap::from([(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expired_ambient)]),
    )
    .await;
    validate_workflow_credentials(&backend, "flotilla", &with_grant, Some(&placement))
        .await
        .expect("delivered OAuth admits trusted Claude despite an expired ambient login");
}

#[tokio::test]
async fn ambient_only_adapter_is_refused_when_the_host_login_expired() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Trusted)
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent { selector: Selector::for_capability("ambient-only"), prompt: None, brief_template: None })
                .build()])
            .build()])
        .build();
    create_docker_placement(&backend, "ambient-host", "host-a", BTreeSet::new()).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("ambient-host").await.expect("get placement");
    let expired = CredentialExpiry::builder().refresh_expires_at("2020-02-01T00:00:00Z".parse().expect("timestamp")).build();
    set_host_credential_expiry(
        &backend,
        "host-a",
        BTreeMap::from([(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expired)]),
    )
    .await;
    let capabilities = CapabilityTable::seeded().with_ambient_only_test_requirement("ambient-only");

    let error = validate_workflow_credentials_with_capabilities(&backend, "flotilla", &workflow, Some(&placement), &capabilities)
        .await
        .expect_err("expired ambient-only authentication must refuse dispatch");

    assert_eq!(
        error,
        "vessel `work` depends on the ambient claude login on host `host-a`, which expired on 2020-02-01 — log in again on that host or grant a delivered claude credential"
    );
}

async fn set_host_credential_expiry(backend: &ResourceBackend, host_ref: &str, expiry: BTreeMap<String, CredentialExpiry>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get(host_ref).await.expect("host resource");
    let mut status = host.status.expect("host status");
    status.capabilities.insert(flotilla_resources::CREDENTIAL_EXPIRY_CAPABILITY.to_string(), serde_json::json!(expiry));
    hosts.update_status(host_ref, &host.metadata.resource_version, &status).await.expect("update host status");
}

#[tokio::test]
async fn dispatch_against_an_expired_credential_is_refused_with_the_credential_and_host_named() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Contained)
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
    workflow.vessels[0].credential_refs = BTreeSet::from(["claude-max".to_string()]);
    create_docker_placement(&backend, "docker-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-claude").await.expect("get placement");

    let near_expiry = CredentialExpiry::builder().refresh_expires_at(Utc::now() + chrono::Duration::days(3)).build();
    set_host_credential_expiry(&backend, "host-a", BTreeMap::from([("claude-max".to_string(), near_expiry)])).await;
    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("near-expiry material still admits dispatch");

    let expired = CredentialExpiry::builder()
        .expires_at("2020-01-01T00:00:00Z".parse().expect("timestamp"))
        .refresh_expires_at("2020-02-01T00:00:00Z".parse().expect("timestamp"))
        .build();
    set_host_credential_expiry(&backend, "host-a", BTreeMap::from([("claude-max".to_string(), expired)])).await;
    let error = validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect_err("expired credential must refuse dispatch");
    assert_eq!(error, "credential `claude-max` expired on host `host-a` on 2020-02-01 — refresh its material before dispatching");
}
