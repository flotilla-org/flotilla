use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicUsize,
};

use chrono::TimeZone;
use flotilla_resources::{
    ConvoyStatus, CredentialConsumer, CredentialExpiry, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle,
    CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec, CrewWorkPhase,
    CrewWorkState, Environment as ResourceEnvironment, EnvironmentSpec as ResourceEnvironmentSpec, FulfilmentFacts, FulfilmentKindSpec,
    FulfilmentRealisation, HarnessFacts, HostCondition, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, HostSpec, HostStatus, ImageAcquisitionCost, PlacementPolicy, PlacementPolicySpec, Selector,
    TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, VesselRequirement, VesselSpec, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY,
    CONVOY_LABEL, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};

use super::{
    convoy_admission::{
        default_convoy_placement_policy, parse_role_address, resolve_workflow_credentials, validate_workflow_agent_adapters,
        validate_workflow_credentials, validate_workflow_credentials_with_capabilities, KindCandidate, PlacementTieBreak,
        RepositoryChangeRequestProvider,
    },
    crew_ops::{convoy_sender_address, queue_pending_crew_message, terminal_meta_with_vessel_credentials},
    *,
};
use crate::{
    admission::AvailableSpaceProbe,
    providers::{
        change_request::ChangeRequestTracker,
        discovery::{Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        types::ChangeRequest,
        vcs::git_worktree::GitWorktreeStrategy,
        CommandOutput,
    },
    repository_inspection::{LocalCheckoutInspection, RepositoryContinuity, RepositoryInspection, RepositoryInspector},
    vcs::GitCheckoutStrategy,
};

// #2597: failed reads retain the resource identity and emit scoped debug diagnostics;
// successful reads never emit fallback diagnostics. Glue: exhaust the three read outcomes.
#[tokio::test]
async fn convoy_sender_lookup_diagnostics_preserve_fallbacks() {
    use crate::providers::testing::capture_logs;
    let memory = ResourceBackend::InMemory(InMemoryBackend::default());
    memory
        .using::<ResourceConvoy>("attribution")
        .create(&test_meta("supervisor"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).role("governor".to_string()).build())
        .await
        .expect("convoy");
    // Real HTTP collaborator with an invalid URL fails before any network request.
    let invalid = ResourceBackend::Http(flotilla_resources::HttpBackend::new(crate::tls::client(), "://invalid"));
    for (backend, name, expected, failure) in
        [(&memory, "supervisor", "governor", false), (&memory, "missing", "missing", true), (&invalid, "unavailable", "unavailable", true)]
    {
        // Mirror the sender lookup's replica-inclusive read to check the exact backend error.
        // Keep this read aligned if the attribution lookup path changes.
        let read = backend.including_replicas::<ResourceConvoy>("attribution").get(name).await;
        let (address, logs) = capture_logs(tracing::Level::DEBUG, convoy_sender_address(backend, "attribution", name)).await;
        assert_eq!(address, expected);
        if failure {
            let error = read.expect_err("failed read").to_string();
            assert!(logs.contains("DEBUG"), "{logs}");
            assert!(logs.contains("namespace=attribution"), "{logs}");
            assert!(logs.contains(&format!("convoy_ref={name}")), "{logs}");
            assert!(logs.contains(&format!("error={error}")), "{logs}");
            assert_eq!(logs.matches("convoy sender attribution lookup failed").count(), 1, "{logs}");
        } else {
            assert!(!logs.contains("convoy sender attribution lookup failed"), "{logs}");
        }
    }
}

// #2592: attribution uses role/project addresses, preserves legacy resource names,
// and remains available when a supervisor convoy is absent from the replica view.
// Formatting glue: these rows exhaust empty/nonempty role and absent/present project.
#[tokio::test]
async fn convoy_sender_addresses_preserve_legacy_and_missing_convoy_identity() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    for (name, role, project, expected) in [
        ("legacy", "", None, "legacy"),
        ("legacy-project", "", Some("project"), "legacy-project"),
        ("named", "graphql-budget", None, "graphql-budget"),
        ("named-project", "graphql-budget", Some("project"), "graphql-budget@project"),
    ] {
        let convoy = convoys
            .create(
                &test_meta(name),
                &ConvoySpec::builder()
                    .workflow_ref("workflow".to_string())
                    .role(role.to_string())
                    .maybe_project_ref(project.map(str::to_string))
                    .build(),
            )
            .await
            .expect("convoy");
        assert_eq!(convoy_message_address(&convoy), expected);
        assert_eq!(convoy_sender_address(&backend, "flotilla", name).await, expected);
    }
    assert_eq!(convoy_sender_address(&backend, "flotilla", "missing-governor").await, "missing-governor");
}

#[tokio::test]
async fn repository_watch_evicts_deleted_providers_and_relist_preserves_live_providers() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"provider-watch-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repositories = backend.clone().using::<Repository>("flotilla");
    let first = RepositorySpec::remote("https://github.com/example/first").expect("first repository");
    let second = RepositorySpec::remote("https://github.com/example/second").expect("second repository");
    for repository in [&first, &second] {
        repositories.create(&test_meta(&repository.key().to_string()), repository).await.expect("create repository");
    }
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let provider: Arc<dyn ChangeRequestTracker> = Arc::new(FakeChangeRequest::new());
    for repository in [&first, &second] {
        daemon.convoy_admission.repository_change_requests.write().await.insert(
            repository.key(),
            RepositoryChangeRequestProvider {
                service_url: "https://github.com".to_string(),
                repository: repository.key().to_string(),
                provider: Arc::clone(&provider),
            },
        );
    }
    repositories.delete(&first.key().to_string()).await.expect("delete first repository");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !daemon.convoy_admission.repository_change_requests.read().await.contains_key(&first.key()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repository watch should evict provider");
    assert!(
        daemon.convoy_admission.repository_change_requests.read().await.contains_key(&second.key()),
        "live repository keeps its provider"
    );

    let third = RepositorySpec::remote("https://github.com/example/third").expect("third repository");
    repositories.create(&test_meta(&third.key().to_string()), &third).await.expect("create third repository");
    daemon.convoy_admission.repository_change_requests.write().await.insert(
        third.key(),
        RepositoryChangeRequestProvider { service_url: "https://github.com".to_string(), repository: third.key().to_string(), provider },
    );
    backend
        .clone()
        .using::<Repository>("alternate")
        .create(&test_meta(&second.key().to_string()), &second)
        .await
        .expect("repository in replacement namespace");
    daemon.set_provisioning_namespace("alternate".to_string()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !daemon.convoy_admission.repository_change_requests.read().await.contains_key(&third.key()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restarted watch should reconcile cached providers from the new list");
    assert!(
        daemon.convoy_admission.repository_change_requests.read().await.contains_key(&second.key()),
        "relist retains the live provider"
    );
}

#[tokio::test]
async fn cli_lists_include_observed_checkouts_and_only_open_change_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository = RepositorySpec::remote("https://github.com/team/repo.git").expect("repository");
    let key = repository.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository record");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    daemon
        .observed_resource_backend()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &test_meta("observed-checkout"),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "feature".into(),
                path: "/tmp/repo-feature".into(),
                repo_ref: key,
                host_ref: "local".into(),
                is_main: false,
            }),
        )
        .await
        .expect("observed checkout");
    let change_requests = backend.using::<ResourceChangeRequest>("flotilla");
    for (name, number, state) in [("open-pr", 42, ObservedChangeRequestState::Open), ("closed-pr", 43, ObservedChangeRequestState::Closed)]
    {
        let created = change_requests
            .create(
                &test_meta(name),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("https://github.com".to_string())
                    .scope("team/repo".to_string())
                    .number(number)
                    .observing_authority("github".to_string())
                    .build(),
            )
            .await
            .expect("change request");
        let observed_at = Utc::now();
        change_requests
            .update_status(
                name,
                &created.metadata.resource_version,
                &flotilla_resources::ChangeRequestStatus {
                    title: flotilla_resources::Observation::known(format!("PR {number}"), observed_at),
                    author: Default::default(),
                    review_decision: Default::default(),
                    review_requested_from_owner: Default::default(),
                    state: flotilla_resources::Observation::known(state, observed_at),
                    head_sha: flotilla_resources::Observation::unknown(observed_at),
                    checks: flotilla_resources::Observation::unknown(observed_at),
                    review: flotilla_resources::ChangeRequestReviewObservation {
                        actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
                    },
                    mergeable: flotilla_resources::Observation::unknown(observed_at),
                },
            )
            .await
            .expect("change request status");
    }

    let list = |kind| Command::builder().action(CommandAction::QueryCliList { kind }).build();
    let CommandValue::CliList(repos) = daemon.execute_query(list(CliListKind::Repo), uuid::Uuid::new_v4()).await.expect("repos") else {
        panic!("expected repo list");
    };
    assert_eq!(repos.items.len(), 1);
    let CommandValue::CliList(checkouts) =
        daemon.execute_query(list(CliListKind::Checkout), uuid::Uuid::new_v4()).await.expect("checkouts")
    else {
        panic!("expected checkout list");
    };
    assert_eq!(checkouts.items.len(), 1);
    assert_eq!(checkouts.items[0].name, "feature");
    let CommandValue::CliList(crs) = daemon.execute_query(list(CliListKind::Cr), uuid::Uuid::new_v4()).await.expect("change requests")
    else {
        panic!("expected change request list");
    };
    assert_eq!(crs.items.len(), 1);
    assert_eq!(crs.items[0].name, "PR 42");
}

#[tokio::test]
async fn cli_lists_include_active_provider_sessions_and_workspaces() {
    use crate::providers::{
        coding_agent::CloudAgentService,
        discovery::{test_support::FakePresentationManager, ProviderCategory, ProviderDescriptor},
        types::RepoCriteria,
    };

    struct Sessions;
    struct BrokenSessions;

    #[async_trait::async_trait]
    impl CloudAgentService for Sessions {
        async fn list_sessions(&self, criteria: &RepoCriteria) -> Result<Vec<(String, flotilla_protocol::CloudAgentSession)>, String> {
            assert_eq!(criteria.repo_slug.as_deref(), Some("team/repo"));
            let session = |title: &str, status| flotilla_protocol::CloudAgentSession {
                title: title.into(),
                status,
                model: None,
                updated_at: None,
                provider_name: "fake".into(),
                provider_display_name: "Fake".into(),
                item_noun: "session".into(),
            };
            Ok(vec![
                ("active".into(), session("Active", flotilla_protocol::SessionStatus::Running)),
                ("archived".into(), session("Archived", flotilla_protocol::SessionStatus::Archived)),
            ])
        }

        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }

        async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
            Ok("true".into())
        }
    }

    #[async_trait::async_trait]
    impl CloudAgentService for BrokenSessions {
        async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, flotilla_protocol::CloudAgentSession)>, String> {
            Err("provider unavailable".into())
        }

        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }

        async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
            Ok("true".into())
        }
    }

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
    let workspace_provider = Arc::new(FakePresentationManager::new());
    workspace_provider
        .add_workspaces(vec![("workspace:1".into(), flotilla_protocol::Workspace { name: "Current work".into(), attachable_set_id: None })])
        .await;
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("broken", ProviderDescriptor::named(ProviderCategory::CloudAgent, "broken"), Arc::new(BrokenSessions));
    registry.cloud_agents.insert("fake", ProviderDescriptor::named(ProviderCategory::CloudAgent, "fake"), Arc::new(Sessions));
    let broken_workspace_provider = Arc::new(FakePresentationManager::new());
    *broken_workspace_provider.list_error.lock().await = Some("provider unavailable".into());
    registry.presentation_managers.insert(
        "broken",
        ProviderDescriptor::named(ProviderCategory::WorkspaceManager, "broken"),
        broken_workspace_provider,
    );
    registry.presentation_managers.insert(
        "fake",
        ProviderDescriptor::named(ProviderCategory::WorkspaceManager, "fake"),
        workspace_provider,
    );
    let identity = RepoIdentity { authority: "github.com".into(), path: "team/repo".into() };
    daemon.repos.write().await.insert(
        identity.clone(),
        RepoState::new(
            identity,
            RepoRootState {
                path: temp.path().join("repo"),
                model: RepoModel::new(registry, None),
                slug: Some("team/repo".into()),
                unmet: vec![],
                is_local: true,
            },
        ),
    );

    let list = |kind| Command::builder().action(CommandAction::QueryCliList { kind }).build();
    let CommandValue::CliList(agents) = daemon.execute_query(list(CliListKind::Agent), uuid::Uuid::new_v4()).await.expect("agents") else {
        panic!("expected agent list");
    };
    assert_eq!(agents.items.len(), 1);
    assert_eq!(agents.items[0].reference, "active");
    let CommandValue::CliList(workspaces) =
        daemon.execute_query(list(CliListKind::Workspace), uuid::Uuid::new_v4()).await.expect("workspaces")
    else {
        panic!("expected workspace list");
    };
    assert_eq!(workspaces.items.len(), 1);
    assert_eq!(workspaces.items[0].name, "Current work");
    assert_eq!(workspaces.items[0].repo, None);
}

#[test]
fn bound_change_request_identity_uses_matching_declared_or_discovered_subject() {
    let requested = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number: 42 };
    let subject = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "team/repo".into() },
        id: "42".into(),
    };
    let spec = ConvoySpec::builder().workflow_ref("review".to_string()).build();
    let mut status = ConvoyStatus::default();
    status.discover_subject(
        subject.clone(),
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Claim,
        Utc::now(),
    );
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(Vec::new())
            .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
            .build()],
    });
    let mut convoy = ResourceObject::<ResourceConvoy> {
        metadata: ObjectMeta {
            name: "review-convoy".to_string(),
            namespace: "flotilla".to_string(),
            resource_version: "1".to_string(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec,
        status: Some(status),
    };
    let bound = convoy_change_request_credential_refs(&convoy, &requested).expect("active PR subjects");
    assert_eq!(bound.numbers, BTreeSet::from([42]));
    assert_eq!(bound.credentials_by_number[&42], BTreeSet::from(["github-crew-pr".to_string()]));
    let mut unrelated = requested.clone();
    unrelated.scope = "team/other".into();
    assert!(convoy_change_request_credential_refs(&convoy, &unrelated).expect("other scope").numbers.is_empty());
    convoy.status.as_mut().expect("status").discover_subject(
        subject,
        flotilla_protocol::Relationship::Supersedes,
        flotilla_resources::SubjectDiscoverySource::Operator,
        Utc::now(),
    );
    assert!(convoy_change_request_credential_refs(&convoy, &requested).expect("superseded PR").numbers.is_empty());
}

#[tokio::test]
async fn operator_brief_survives_a_racing_nudge_until_delivery() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("racing-messages".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "convoy".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "Initial".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "convoy".to_string(),
                        vessel_ref: "work".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("session");
    backend
        .using::<ResourceConvoy>("flotilla")
        .create(&test_meta("convoy"), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("convoy context");
    queue_pending_crew_message(&sessions, &session, CrewMessageSender::OperatorResume { principal: None }, "Continue the work")
        .await
        .expect("operator brief queued");
    let queued = sessions.get("racing-messages").await.expect("queued operator brief");
    queue_pending_crew_message(&sessions, &queued, CrewMessageSender::FlotillaNudge, "Please settle").await.expect("nudge queued");
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("records").items;
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|message| message.spec.sender == "principal:implicit" && message.spec.body.contains("Continue the work")));
    assert!(records.iter().any(|message| message.spec.sender == "system:nudge"));

    let concurrent = sessions
        .create(&InputMeta::builder().name("concurrent-messages".into()).labels(session.metadata.labels.clone()).build(), &session.spec)
        .await
        .expect("concurrent session");
    let (operator, nudge) = tokio::join!(
        queue_pending_crew_message(&sessions, &concurrent, CrewMessageSender::OperatorResume { principal: None }, "New guidance"),
        queue_pending_crew_message(&sessions, &concurrent, CrewMessageSender::FlotillaNudge, "Please settle"),
    );
    operator.expect("operator brief survives concurrent write");
    nudge.expect("nudge does not erase concurrent brief");
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("concurrent records").items;
    assert_eq!(records.len(), 4);
    assert!(records.iter().any(|message| message.spec.body.contains("New guidance")));
}

// #2559: an idle reconciliation pass must neither write resources nor emit
// events, because its runtime caller is triggered by those same watches.
#[hegel::test]
fn supervisor_turn_reconciliation_noop_contract(tc: hegel::TestCase) {
    use hegel::generators as gs;

    // Generate repeated idle passes and pending/terminal Message states.
    // Every case uses both real storage backends with new-only terminal shapes.
    let passes = tc.draw(gs::integers::<usize>().min_value(2).max_value(4));
    let terminal_message = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for sqlite in [false, true] {
            let temp = tempfile::tempdir().expect("contract directory");
            std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"noop-contract\"\n").expect("daemon identity");
            let backend = if sqlite {
                ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open(temp.path().join("resources.db")).expect("sqlite backend"))
            } else {
                ResourceBackend::InMemory(InMemoryBackend::default())
            };
            let daemon = InProcessDaemon::new_with_resource_backend(
                Vec::new(),
                Arc::new(ConfigStore::with_base(temp.path())),
                fake_discovery(false),
                HostName::new("noop-contract"),
                backend.clone(),
            )
            .await;
            // Keep contract resources outside the daemon's background provisioning namespace.
            let namespace = "noop-contract";
            let sessions = backend.clone().using::<ResourceTerminalSession>(namespace);
            let convoys = backend.clone().using::<ResourceConvoy>(namespace);
            assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "empty store", passes).await;

            let spec = ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "governor".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("governor"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "standing brief".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: namespace.to_string(),
                        convoy: "convoy".to_string(),
                        vessel_ref: "govern".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            };
            sessions.create(&test_meta("unrelated"), &spec).await.expect("unrelated terminal");
            assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "unrelated terminal", passes).await;

            let convoy = convoys
                .create(&test_meta("convoy"), &ConvoySpec::builder().workflow_ref("governor".to_string()).build())
                .await
                .expect("convoy");
            convoys
                .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus::default())
                .await
                .expect("pending turn");
            let session = sessions
                .create(
                    &InputMeta::builder()
                        .name("governor".to_string())
                        .labels(BTreeMap::from([
                            (CONVOY_LABEL.to_string(), "convoy".to_string()),
                            (VESSEL_LABEL.to_string(), "govern".to_string()),
                            (ROLE_LABEL.to_string(), "governor".to_string()),
                        ]))
                        .build(),
                    &spec,
                )
                .await
                .expect("queued terminal");
            sessions
                .update_status(
                    &session.metadata.name,
                    &session.metadata.resource_version,
                    &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
                )
                .await
                .expect("running terminal");
            let intent = flotilla_resources::MessageSpec::builder()
                .sender("system:test".into())
                .receiver("noop-contract/convoy/govern/governor".into())
                .relation(flotilla_resources::MessageRelation::System)
                .body("supervise".into())
                .build();
            let inbox = daemon.message_inbox(namespace).await;
            inbox.accept(&test_meta("input"), &intent, Utc::now()).await.expect("new Message");
            if terminal_message {
                let messages = backend.using::<flotilla_resources::Message>(namespace);
                let record = messages.get("input").await.unwrap();
                let mut status = record.status.unwrap();
                status.phase = flotilla_resources::MessagePhase::Expired;
                status.since = Utc::now();
                messages.update_status("input", &record.metadata.resource_version, &status).await.unwrap();
            }
            assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "new-only Message", passes).await;
        }
    });
}

async fn assert_supervisor_turn_passes_are_idle(
    daemon: &InProcessDaemon,
    backend: &ResourceBackend,
    namespace: &str,
    state: &str,
    passes: usize,
) {
    let backend_name = match backend {
        ResourceBackend::InMemory(_) => "in-memory",
        ResourceBackend::Sqlite(_) => "sqlite",
        _ => panic!("unsupported contract backend"),
    };
    let state = format!("{backend_name}: {state}");
    let sessions = backend.clone().using::<ResourceTerminalSession>(namespace);
    let convoys = backend.clone().using::<ResourceConvoy>(namespace);
    // This namespace has no concurrent writers, so list-to-subscribe cannot miss a write.
    let before_sessions = serde_json::to_value(sessions.list().await.expect("terminal baseline")).expect("serialize terminal baseline");
    let before_convoys = serde_json::to_value(convoys.list().await.expect("convoy baseline")).expect("serialize convoy baseline");
    let mut session_watch = sessions.watch(flotilla_resources::WatchStart::Now).await.expect("terminal watch");
    let mut convoy_watch = convoys.watch(flotilla_resources::WatchStart::Now).await.expect("convoy watch");
    for pass in 0..passes {
        daemon.reconcile_pending_supervisor_turns_once(namespace).await.expect("idle pass succeeds");
        // Resource objects and list versions expose even writes of unchanged values.
        assert_eq!(
            serde_json::to_value(sessions.list().await.expect("terminal after pass")).expect("serialize terminals"),
            before_sessions,
            "{state}, pass {pass}: terminal write"
        );
        assert_eq!(
            serde_json::to_value(convoys.list().await.expect("convoy after pass")).expect("serialize convoys"),
            before_convoys,
            "{state}, pass {pass}: convoy write"
        );
        // Both local backends publish before their write completes. Poll directly:
        // an idle watch must be pending, never an event, error, or closed stream.
        assert!(session_watch.next().now_or_never().is_none(), "{state}, pass {pass}: terminal watch activity");
        assert!(convoy_watch.next().now_or_never().is_none(), "{state}, pass {pass}: convoy watch activity");
    }
}

#[tokio::test]
async fn standing_governor_on_another_host_receives_a_stalled_crew_turn() {
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("home"));
    let placement = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("placement"));
    let temp = tempfile::tempdir().expect("config dir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"home\"\n").expect("machine identity");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        home.clone(),
    )
    .await;
    let placement_config = tempfile::tempdir().expect("placement config dir");
    std::fs::write(placement_config.path().join("daemon.toml"), "machine_id = \"placement\"\n").expect("placement identity");
    let placement_daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(placement_config.path())),
        fake_discovery(false),
        HostName::local(),
        placement.clone(),
    )
    .await;
    let convoys = home.clone().using::<ResourceConvoy>("flotilla");
    let governor = convoys
        .create(
            &test_meta("governor-convoy"),
            &ConvoySpec::builder().workflow_ref("governor".to_string()).role("governor".to_string()).build(),
        )
        .await
        .expect("governor convoy");
    let existing_attention = flotilla_resources::ConvoyAttention {
        source: "existing-attention".to_string(),
        reason: "Existing concern".to_string(),
        raised_at: Utc::now(),
    };
    convoys
        .update_status(
            &governor.metadata.name,
            &governor.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                work: BTreeMap::from([(
                    "govern".to_string(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "govern".to_string(),
                    BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                attention: Some(existing_attention.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("running governor");
    let sessions = placement.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("governor-terminal".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "governor-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "govern".to_string()),
                    (ROLE_LABEL.to_string(), "governor".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "remote-env".to_string(),
                role: "governor".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("governor"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "standing brief".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "governor-convoy".to_string(),
                        vessel_ref: "governor-convoy-govern".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("governor terminal on placement host");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
        )
        .await
        .expect("running terminal");
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("governor-convoy".to_string())
        .source("supervision-stalled-crew".to_string())
        .vessel("govern".to_string())
        .role("governor".to_string())
        .brief("Escalated from coder@work in graphql-budget@flotilla:\n\nSupervise the stalled crew".to_string())
        .subject_revision("stall-1".to_string())
        .sender("system:stall-judge".into())
        .relation(flotilla_resources::MessageRelation::Supervisor)
        .build();
    // Boundary fake for publication to the owning host; receiver admission
    // still runs through the real daemon's ordinary resource mutation handler.
    struct PlacementPublisher(Arc<InProcessDaemon>);
    #[async_trait]
    impl crate::leaf_engine::ResourceIntentPublisher for PlacementPublisher {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<flotilla_protocol::ResourceRef, String> {
            let admitted = self.0.apply_intent_document(namespace, document).await.map_err(|error| error.to_string())?;
            Ok(flotilla_protocol::ResourceRef::new(
                "flotilla.work/v1",
                admitted.kind,
                admitted.namespace,
                admitted.value["metadata"]["name"].as_str().expect("admitted resource name"),
            ))
        }
    }
    let publisher: Arc<dyn crate::leaf_engine::ResourceIntentPublisher> = Arc::new(PlacementPublisher(placement_daemon));
    daemon.set_resource_intent_publisher(Arc::downgrade(&publisher));
    daemon.deliver_standing_turn(&request).await.expect("remote governor intent accepted");
    let messages = placement.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver messages").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.receiver, "flotilla/governor-convoy/govern/governor");
    assert_eq!(messages[0].spec.relation, flotilla_resources::MessageRelation::Supervisor);
    assert_eq!(messages[0].spec.body, "Escalated from coder@work in graphql-budget@flotilla:\n\nSupervise the stalled crew");
    assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
    assert!(home.using::<flotilla_resources::Message>("flotilla").list().await.expect("sender messages").items.is_empty());
    assert_eq!(convoys.get("governor-convoy").await.expect("governor").status.expect("status").attention, Some(existing_attention));
}

#[test]
fn placement_tiebreak_reserves_scarce_platforms_for_named_needs() {
    let now = chrono::Utc::now();
    for platform in flotilla_resources::Platform::ALL {
        let kind = ResourceObject::<FulfilmentKind> {
            metadata: flotilla_resources::ObjectMeta {
                name: platform.to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: now,
                merge: None,
            },
            spec: FulfilmentKindSpec::builder()
                .host_ref("host".to_string())
                .pool("test".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::platform(platform.to_string())]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
            status: None,
        };
        let candidate = KindCandidate {
            kind,
            placement: PlacementResolution {
                selected: None,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            },
            free_slots: Some(1),
            host_ready: true,
            image_cost: ImageAcquisitionCost::Held,
            sleeping_until: None,
        };
        let no_need = BTreeSet::new();
        assert_eq!(PlacementTieBreak { needs: &no_need, now }.reserved(&candidate), platform.is_reserved(), "{platform}");
        let named = BTreeSet::from([CapabilityNeed::Platform(platform.to_string())]);
        assert!(!PlacementTieBreak { needs: &named, now }.reserved(&candidate));
    }
}
#[cfg(unix)]
use crate::providers::{
    discovery::test_support::FakeTerminalPool,
    terminal::{managed_session_name, ManagedSessionMetadata, TerminalSession},
};
use crate::providers::{
    discovery::test_support::{
        fake_discovery, fake_discovery_with_provider_set, fake_discovery_with_runner, FakeChangeRequest, FakeDiscoveryProviders,
        FakeVcsFactory, FakeVcsState,
    },
    testing::MockRunner,
};

#[test]
fn standalone_issue_source_lookup_round_trips_installation_identity() {
    let lab = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.example".into()]))
        .https_url("https://forgejo.example/lab".into())
        .git_ssh_host("forgejo.example".into())
        .build();
    let mut stage = lab.clone();
    stage.forge_id = "stage".into();
    stage.https_url = "https://forgejo.example/stage".into();
    let forges = [lab, stage];
    for (service, expected) in [
        ("lab", "https://forgejo.example/lab"),
        ("stage", "https://forgejo.example/stage"),
        ("forgejo.example%2flab", "https://forgejo.example/lab"),
        ("forgejo.example%2fstage", "https://forgejo.example/stage"),
        ("http%3a%2f%2fforgejo.example%2flab", "http://forgejo.example/lab"),
        ("host%3aforgejo", "https://forgejo"),
    ] {
        let subject = crate::issue_observer::IssueRef {
            namespace: "flotilla".into(),
            service: service.into(),
            scope: "Team/Repo".into(),
            number: 12,
        };
        let source = issue_source_for_subject(&subject, &forges).expect("source");
        assert_eq!(source.service, expected);
        let reference = flotilla_protocol::IssueRef { source, id: "12".into() };
        let declarations = if service.contains("%2f") { &[][..] } else { &forges[..] };
        assert_eq!(
            flotilla_resources::issue_address_with_forges(&reference, declarations).expect("round trip").to_string(),
            format!("issue/{service}/Team/Repo/12")
        );
    }
    let orphaned =
        crate::issue_observer::IssueRef { namespace: "flotilla".into(), service: "lab".into(), scope: "Team/Repo".into(), number: 12 };
    assert!(issue_source_for_subject(&orphaned, &[]).expect_err("missing Forge").contains("no Forge declaration"));
}

struct RecordingWorkCredentials {
    backend: ResourceBackend,
    delivered: tokio::sync::Mutex<BTreeSet<String>>,
    fail_next: std::sync::atomic::AtomicBool,
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

struct SessionStagingProbe {
    backend: ResourceBackend,
    session: String,
    environment: String,
    fail_next: std::sync::atomic::AtomicBool,
    invalidate_next: std::sync::atomic::AtomicBool,
    staged: std::sync::atomic::AtomicUsize,
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

async fn resume_staging_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    resume_staging_fixture_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await
}

async fn resume_staging_fixture_with_backend(
    backend: ResourceBackend,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    resume_staging_fixture_with_clock(backend, Arc::new(flotilla_resources::SystemClock)).await
}

async fn resume_staging_fixture_with_clock(
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

// Working crews already own staged credentials. Operator input is admitted as
// durable intent; ordinary attention changes cannot fabricate its delivery receipt.
#[tokio::test]
async fn working_resume_uses_message_delivery_instead_of_pending_brief_release() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let current = convoys.get("resume-staging").await.expect("convoy");
    let mut status = current.status.expect("status");
    status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status(&current.metadata.name, &current.metadata.resource_version, &status).await.expect("working crew");
    daemon
        .convoy_resume_internal("flotilla", "resume-staging", "operator guidance", Some("work"), Some("coder"))
        .await
        .expect("queue intent");
    daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("migration scan");
    assert!(convoys.get("resume-staging").await.expect("convoy").status.expect("status").pending_brief().is_none());
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].spec.body, "operator guidance");
    assert_eq!(records[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
    assert!(records[0].status.as_ref().expect("status").resolved_receiver.is_none());
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resume_stages_credentials_before_message_and_retries_failure() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let resume = || daemon.convoy_resume_internal("flotilla", "resume-staging", "continue", Some("work"), Some("coder"));
    assert!(resume().await.expect_err("staging should fail").contains("credential staging failed"));
    assert_eq!(
        convoys.get("resume-staging").await.expect("convoy").status.expect("status").crew_work["work"]["coder"].phase,
        CrewWorkPhase::Done
    );
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
    let TerminalSessionSource::Agent { message, .. } = sessions.get("resume-staging-session").await.expect("session").spec.source else {
        panic!("agent")
    };
    assert!(message.is_none());
    resume().await.expect("retry resume");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let TerminalSessionSource::Agent { message, .. } = sessions.get("resume-staging-session").await.expect("session").spec.source else {
        panic!("agent")
    };
    assert!(message.is_none());
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(records.len(), 1);
    // Native Message attribution is separate from its unframed body.
    assert_eq!(records[0].spec.body, "continue");
    assert_eq!(records[0].spec.sender, "principal:implicit");
}

#[tokio::test]
async fn resume_retries_a_racing_session_write_after_staging() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    probe.invalidate_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = daemon
        .convoy_resume_internal("flotilla", "resume-staging", "continue", Some("work"), Some("coder"))
        .await
        .expect("stale session write retried");
    assert_eq!(outcome, ConvoyResumeOutcome::Queued { displaced: None });
    let status = backend.using::<ResourceConvoy>("flotilla").get("resume-staging").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("resume-staging-session").await.expect("session");
    assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
    assert!(backend
        .using::<flotilla_resources::Message>("flotilla")
        .list()
        .await
        .expect("inbox")
        .items
        .iter()
        .any(|message| message.spec.body.contains("continue")));
}

#[tokio::test]
async fn declared_access_stall_routes_to_project_governor_and_resumes() {
    #[derive(Default)]
    struct AcceptSupervision {
        requests: std::sync::Mutex<Vec<crate::leaf_engine::CrewTurnIntent>>,
    }
    #[async_trait]
    impl crate::leaf_engine::TurnDeliveryActuator for AcceptSupervision {
        async fn deliver(&self, request: &crate::leaf_engine::CrewTurnIntent) -> Result<crate::leaf_engine::CrewTurnAdmission, String> {
            self.requests.lock().expect("supervision requests").push(request.clone());
            Ok(crate::leaf_engine::CrewTurnAdmission {
                new_turn: true,
                rung: flotilla_resources::TurnDeliveryRung::WarmSession,
                message: flotilla_protocol::ResourceRef::new(
                    "flotilla.work/v1",
                    "Message",
                    &request.namespace,
                    format!("fake-{}-{}", request.source, request.subject_revision),
                ),
            })
        }
        async fn hold(
            &self,
            _request: &crate::leaf_engine::CrewTurnIntent,
            _act: &flotilla_resources::HoldAct,
            _reason: &str,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    let database_dir = tempfile::tempdir().expect("resource database");
    let database_path = database_dir.path().join("resources.db");
    let (mut daemon, mut backend, probe) = resume_staging_fixture_with_backend(ResourceBackend::Sqlite(
        flotilla_resources::SqliteBackend::open(&database_path).expect("resource store"),
    ))
    .await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let supervision = Arc::new(AcceptSupervision::default());
    daemon.crew_ops.set_turn_delivery_actuator(supervision.clone()).await;
    backend
        .clone()
        .using::<flotilla_resources::Project>("flotilla")
        .create(
            &test_meta("project"),
            &flotilla_resources::ProjectSpec::builder()
                .display_name("Project".to_string())
                .default_workflow_ref("workflow".to_string())
                .build(),
        )
        .await
        .expect("project");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let source = convoys.get("resume-staging").await.expect("source convoy");
    let mut spec = source.spec.clone();
    spec.project_ref = Some("project".to_string());
    spec.role = "graphql-budget".to_string();
    let source =
        convoys.update(&input_meta_from_resource(&source), &source.metadata.resource_version, &spec).await.expect("project source");
    let mut status = source.status.expect("source status");
    status.phase = flotilla_resources::ConvoyPhase::Active;
    status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status("resume-staging", &source.metadata.resource_version, &status).await.expect("activate source");
    let governor = convoys
        .create(
            &test_meta("governor"),
            &ConvoySpec::builder()
                .workflow_ref("workflow".to_string())
                .role("governor".to_string())
                .project_ref("project".to_string())
                .build(),
        )
        .await
        .expect("governor convoy");
    convoys
        .update_status(
            "governor",
            &governor.metadata.resource_version,
            &ConvoyStatus {
                crew_work: BTreeMap::from([(
                    "watch".to_string(),
                    BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("governor status");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    let mut session_status = ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() };
    session_status.attention = Some(TerminalAttention {
        state: TerminalAttentionState::Working,
        as_of: chrono::Utc::now(),
        source: TerminalAttentionSource::Hook,
    });
    sessions
        .update_status("resume-staging-session", &source_session.metadata.resource_version, &session_status)
        .await
        .expect("source working");
    let mut governor_spec = source_session.spec.clone();
    governor_spec.role = "governor".to_string();
    let TerminalSessionSource::Agent { context, .. } = &mut governor_spec.source else { panic!("agent session") };
    context.convoy = "governor".to_string();
    let governor_session = sessions
        .create(
            &InputMeta::builder()
                .name("governor-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "governor".to_string()),
                    (VESSEL_LABEL.to_string(), "watch".to_string()),
                    (ROLE_LABEL.to_string(), "governor".to_string()),
                ]))
                .build(),
            &governor_spec,
        )
        .await
        .expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                crew: Some(flotilla_resources::CrewSessionStatus {
                    id: "governor-crew".to_string(),
                    adapter: "codex".to_string(),
                    model: None,
                    stance: "governor".to_string(),
                }),
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("governor working");
    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    let mut task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    let patch = convoy_external_patches::mark_crew_stalled(
        "resume-staging".to_string(),
        "work".to_string(),
        "coder".to_string(),
        chrono::Utc::now(),
        flotilla_resources::StallReason::Access,
        Some(flotilla_resources::StallProposedDisposition::ReduceScope),
        "repository permission missing".to_string(),
    );
    apply_resource_status_patch(&convoys, "resume-staging", &patch).await.expect("declare stall");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = convoys.get("resume-staging").await.expect("source").status.expect("status");
            if status.stalled.as_ref().is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Governor) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("governor rung");
    let stalled = convoys.get("resume-staging").await.expect("source").status.expect("status");
    let condition = stalled.stalled.expect("stall condition");
    assert_eq!(condition.reason, Some(flotilla_resources::StallReason::Access));
    assert_eq!(condition.proposed_disposition, Some(flotilla_resources::StallProposedDisposition::ReduceScope));
    assert_eq!(condition.supervisor.expect("governor").convoy, "governor");
    assert_eq!(stalled.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
    {
        let requests = supervision.requests.lock().expect("supervision requests");
        let escalation = requests.iter().find(|request| request.vessel == "watch").expect("governor escalation");
        // #2592: the supervisor must identify the convoy and act on the exact stalled crew.
        assert_eq!(escalation.sender, "project/resume-staging/work/coder");
        assert_eq!(escalation.relation, flotilla_resources::MessageRelation::Supervisor);
        let framed = &escalation.brief;
        assert!(framed.starts_with("Escalated from coder@work in graphql-budget@project:"));
        assert!(framed.contains("convoy graphql-budget@project (resource ref: resume-staging)"), "{framed}");
        assert!(framed.contains("Reason: access. Evidence:"), "{framed}");
        assert!(framed.contains("repository permission missing"), "{framed}");
        for action in ["resume", "convert-to-failed", "escalate"] {
            assert!(
                framed.contains(&format!("flotilla crew supervise --convoy 'resume-staging' --vessel 'work' --role 'coder' {action}")),
                "{framed}"
            );
        }
        assert!(framed.contains("Proposed disposition: reduce-scope."), "{framed}");
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.crew_ops.subscription_rows().await.iter().any(|row| {
                matches!(&row.maker,
                flotilla_resources::LeafMaker::Supervisor { convoy, role, .. } if convoy == "governor" && role == "governor")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("governor row");
    // #2488: restarting the daemon rebuilds its leaf rows from stored status.
    // The already assigned governor retains authority to resume this same stall.
    task.abort();
    let _ = task.await;
    drop(daemon);
    backend = ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open(&database_path).expect("reopen stored resources"));
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let restart_config = tempfile::tempdir().expect("restart config");
    std::fs::write(restart_config.path().join("daemon.toml"), "machine_id = \"resume-staging-test\"\n").expect("daemon config");
    daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(restart_config.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_work_credential_reconciler(probe.clone()).await;
    daemon.crew_ops.set_turn_delivery_actuator(supervision.clone()).await;
    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.crew_ops.subscription_rows().await.iter().any(|row| {
                matches!(&row.maker, flotilla_resources::LeafMaker::Supervisor { convoy, role, .. }
                    if convoy == "governor" && role == "governor")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervisor row restored after restart");
    daemon
        .crew_supervise_internal(
            CrewSupervisionRequest::builder()
                .namespace("flotilla")
                .convoy_name("resume-staging")
                .vessel("work")
                .role("coder")
                .operation(flotilla_protocol::CrewSupervisionAction::Resume)
                .message("continue with access")
                .actor_crew_id("governor-crew")
                .build(),
        )
        .await
        .expect("governor resume");
    let resumed = convoys.get("resume-staging").await.expect("source").status.expect("status");
    assert_eq!(resumed.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert!(resumed.stalled.is_none());
    let source_session = sessions.get("resume-staging-session").await.expect("resumed source session");
    assert!(matches!(source_session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
    let guidance = backend
        .using::<flotilla_resources::Message>("flotilla")
        .list()
        .await
        .expect("guidance inbox")
        .items
        .into_iter()
        .find(|message| message.spec.body.contains("continue with access"))
        .expect("guidance Message");
    assert_eq!(guidance.spec.sender, "project/governor/watch/governor");
    assert_eq!(guidance.spec.relation, flotilla_resources::MessageRelation::Supervisor);
    let governor_session = sessions.get("governor-session").await.expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("governor idle");
    let patch = convoy_external_patches::mark_crew_stalled(
        "resume-staging".to_string(),
        "work".to_string(),
        "coder".to_string(),
        chrono::Utc::now(),
        flotilla_resources::StallReason::Access,
        None,
        "repository permission still missing".to_string(),
    );
    apply_resource_status_patch(&convoys, "resume-staging", &patch).await.expect("stall again");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status = convoys.get("resume-staging").await.expect("source").status.expect("status");
            if status
                .stalled
                .as_ref()
                .is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Operator && stalled.supervision_exhausted)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("idle governor escalates to operator");
    apply_resource_status_patch(
        &convoys,
        "resume-staging",
        &convoy_external_patches::resume_crew_work(
            "work".to_string(),
            "coder".to_string(),
            chrono::Utc::now(),
            "implicit stall test".to_string(),
            None,
        ),
    )
    .await
    .expect("reset crew to working");
    let governor_session = sessions.get("governor-session").await.expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("governor working");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    sessions
        .update_status(
            "resume-staging-session",
            &source_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::NeedsInput,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("source needs input");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = convoys.get("resume-staging").await.expect("source").status.expect("status");
            if status
                .stalled
                .as_ref()
                .is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Governor && stalled.evidence == "NeedsInput")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("NeedsInput routes to governor");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    sessions
        .update_status(
            "resume-staging-session",
            &source_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("source resumes work");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if convoys.get("resume-staging").await.expect("source").status.expect("status").stalled.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("implicit stall cleared");
    task.abort();
}

#[tokio::test]
async fn turn_delivery_accepts_intent_when_terminal_changes_during_staging() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    probe.invalidate_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("continue".to_string())
        .subject_revision("new-head".to_string())
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("durable intent accepted after concurrent terminal edit");
    let status = backend.using::<ResourceConvoy>("flotilla").get("resume-staging").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "continue");
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("resume-staging-session").await.expect("session");
    assert!(matches!(&session.spec.source, TerminalSessionSource::Agent { brief, .. } if brief.content.ends_with(" (concurrent edit)")));
}

// A fresh turn is durable intent independent of the boot brief; it waits for
// the replacement holder's readiness and transport acceptance evidence.
#[tokio::test]
async fn fresh_turn_stores_intent_independently_of_boot_brief() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("resume-staging-session").await.expect("session");
    let mut spec = session.spec.clone();
    let TerminalSessionSource::Agent { brief, .. } = &mut spec.source else { panic!("agent session") };
    brief.artifact_digest = Some("a".repeat(64));
    brief.content.clear();
    let session = sessions
        .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec)
        .await
        .expect("digest-backed session");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Stopped, ..Default::default() },
        )
        .await
        .expect("stopped session");
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("fresh turn".to_string())
        .subject_revision("next-head".to_string())
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("deliver fresh turn");
    let session = sessions.get("resume-staging-session").await.expect("updated session");
    let TerminalSessionSource::Agent { brief, message, .. } = session.spec.source else { panic!("agent session") };
    assert!(brief.content.is_empty());
    assert_eq!(brief.artifact_digest, Some("a".repeat(64)));
    assert!(message.is_none());
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "fresh turn");
    assert_eq!(messages[0].spec.sender, "system:turn-rules");
    assert_eq!(messages[0].spec.receiver, "flotilla/resume-staging/work/coder");
    assert_eq!(session.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
}

#[tokio::test]
async fn repeated_standing_turn_does_not_restart_a_lost_session_after_delivery() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("same turn".to_string())
        .subject_revision("same-head".to_string())
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("first intent");
    let messages = backend.using::<flotilla_resources::Message>("flotilla");
    let message = messages.list().await.expect("messages").items.remove(0);
    flotilla_resources::apply_status_patch(
        &messages,
        &message.metadata.name,
        &flotilla_resources::MessageStatusPatch::Delivered {
            receiver: flotilla_resources::ResolvedMessageReceiver::builder()
                .crew_id("crew".into())
                .session("old-session".into())
                .delivered_at(Utc::now())
                .evidence("holder accepted input".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .expect("receipt");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("resume-staging-session").await.expect("session");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Lost, ..Default::default() },
        )
        .await
        .expect("lost after delivery");
    assert_eq!(daemon.deliver_standing_turn(&request).await.expect("duplicate already accepted"), TurnDeliveryRung::FreshAgent);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(messages.list().await.expect("messages").items.len(), 1);
    assert_eq!(
        sessions.get("resume-staging-session").await.expect("session").status.expect("status").phase,
        ResourceTerminalSessionPhase::Lost
    );
}

#[tokio::test]
async fn turn_delivery_reopens_work_and_stages_credentials_before_queuing_every_rung() {
    for phase in [ResourceTerminalSessionPhase::Running, ResourceTerminalSessionPhase::Starting, ResourceTerminalSessionPhase::Stopped] {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"turn-credential-test\"\n").expect("daemon config");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            backend.clone(),
        )
        .await;
        let credentials = Arc::new(RecordingWorkCredentials {
            backend: backend.clone(),
            delivered: tokio::sync::Mutex::new(BTreeSet::new()),
            fail_next: std::sync::atomic::AtomicBool::new(true),
        });
        daemon.set_work_credential_reconciler(credentials.clone()).await;
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys
            .create(&test_meta("turn-credential-work"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
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
                        vessels: vec![VesselRequirement::builder()
                            .name("work".to_string())
                            .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
                            .crew(Vec::new())
                            .build()],
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
            .expect("admitted claim");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("turn-credential-session".to_string())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.to_string(), "turn-credential-work".to_string()),
                        (VESSEL_LABEL.to_string(), "work".to_string()),
                        (ROLE_LABEL.to_string(), "coder".to_string()),
                    ]))
                    .build(),
                &ResourceTerminalSessionSpec {
                    env_ref: "credential-env".to_string(),
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
                            convoy: "turn-credential-work".to_string(),
                            vessel_ref: "turn-credential-vessel".to_string(),
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
        sessions
            .update_status(
                &session.metadata.name,
                &session.metadata.resource_version,
                &ResourceTerminalSessionStatus { phase, ..Default::default() },
            )
            .await
            .expect("session phase");
        let request = crate::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".to_string())
            .convoy("turn-credential-work".to_string())
            .source("conflicting".to_string())
            .vessel("work".to_string())
            .role("coder".to_string())
            .brief("rebase the PR".to_string())
            .subject_revision("new-head".to_string())
            .sender("system:turn-rules".into())
            .build();
        assert!(daemon.deliver_standing_turn(&request).await.is_err(), "failed staging must prevent delivery");
        let after_failure = convoys.get("turn-credential-work").await.expect("convoy after failed staging").status.expect("status");
        assert_eq!(after_failure.phase, flotilla_resources::ConvoyPhase::Landing);
        assert_eq!(after_failure.work["work"].phase, flotilla_resources::WorkPhase::Complete);
        assert_eq!(after_failure.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        assert!(credentials.delivered.lock().await.is_empty());
        let undelivered = sessions.get("turn-credential-session").await.expect("undelivered session");
        let TerminalSessionSource::Agent { message, .. } = undelivered.spec.source else { panic!("agent session expected") };
        assert!(message.is_none());
        daemon.deliver_standing_turn(&request).await.expect("retry conflicting turn");
        let delivered = sessions.get("turn-credential-session").await.expect("delivered session");
        assert_eq!(*credentials.delivered.lock().await, BTreeSet::from(["github-crew-pr".to_string()]));
        let TerminalSessionSource::Agent { message, .. } = delivered.spec.source else { panic!("agent session expected") };
        assert!(message.is_none());
        let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("accepted messages").items;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].spec.body, "rebase the PR");
        assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
        if phase == ResourceTerminalSessionPhase::Stopped {
            assert_eq!(delivered.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
        }
    }
}

#[tokio::test]
async fn idle_crew_nudges_are_bounded_and_credential_staged() {
    use flotilla_resources::StallRung;

    for limit in [0, 2] {
        let (daemon, backend, probe) = resume_staging_fixture().await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Active;
        status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
        status.workflow_snapshot.as_mut().expect("workflow").stall_nudges.insert(
            "work/coder".to_string(),
            flotilla_resources::StallNudgePolicy { max_per_episode: limit, max_refusals: None, idle_grace_seconds: Some(0) },
        );
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("active convoy");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("resume-staging-session").await.expect("session");
        let mut session_status = ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() };
        session_status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: chrono::Utc::now(),
            source: TerminalAttentionSource::Hook,
        });
        sessions.update_status("resume-staging-session", &session.metadata.resource_version, &session_status).await.expect("idle session");
        let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
        let watcher = daemon.reconciler_wake_watch();
        let task = tokio::spawn(watcher.spawn(backend.clone(), "flotilla".to_string(), tx));
        let expected_rung = if limit == 0 { StallRung::Operator } else { StallRung::Nudge };
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let stalled = convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled;
                if stalled.as_ref().is_some_and(|stall| stall.rung == expected_rung && stall.nudge_history.len() == limit.min(1) as usize) {
                    break stalled.expect("stalled");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first stall judgement");
        assert_eq!(first.nudge_history.len(), limit.min(1) as usize);
        let session = sessions.get("resume-staging-session").await.expect("session");
        let TerminalSessionSource::Agent { message, .. } = session.spec.source else { panic!("agent") };
        if limit == 0 {
            assert!(message.is_none());
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
        } else {
            assert!(message.is_none());
            let messages = backend.using::<flotilla_resources::Message>("flotilla");
            let nudge = messages.list().await.expect("nudges").items.remove(0);
            assert!(nudge.spec.body.contains("You owe a settlement claim for work/coder"));
            assert!(matches!(nudge.spec.expectation, flotilla_resources::MessageExpectation::Outcome { .. }));
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
            for (offset, desired_rung) in [(1, StallRung::Nudge), (2, StallRung::Operator)] {
                for nudge in messages.list().await.expect("nudges").items {
                    if nudge.status.as_ref().is_some_and(|status| status.phase.is_waiting()) {
                        flotilla_resources::apply_status_patch(
                            &messages,
                            &nudge.metadata.name,
                            &flotilla_resources::MessageStatusPatch::Delivered {
                                receiver: flotilla_resources::ResolvedMessageReceiver::builder()
                                    .crew_id("crew".into())
                                    .session("resume-staging-session".into())
                                    .delivered_at(Utc::now())
                                    .evidence("agent accepted nudge".into())
                                    .build(),
                                at: Utc::now(),
                            },
                        )
                        .await
                        .expect("consume nudge");
                    }
                }
                let session = sessions.get("resume-staging-session").await.expect("session");
                session_status.attention.as_mut().expect("attention").as_of = chrono::Utc::now() + chrono::Duration::seconds(offset);
                sessions
                    .update_status("resume-staging-session", &session.metadata.resource_version, &session_status)
                    .await
                    .expect("idle again");
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let stalled =
                            convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.expect("stalled");
                        if stalled.nudge_history.len() == 2 && stalled.rung == desired_rung {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("next idle episode judgement");
            }
            let stalled = convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.expect("stalled");
            assert_eq!(stalled.nudge_history.len(), 2);
        }
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Landed;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Done;
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("complete");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stall cleared");
        task.abort();
    }
}

struct ForgeAwareTestChangeRequestFactory(Arc<dyn ChangeRequestTracker>);

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

#[tokio::test]
async fn convoy_change_request_resolution_uses_forge_aware_factory_and_credential() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("daemon.toml"),
        "machine_id = \"forgejo-cr-test\"\n[credentials.forgejo]\nlab = \"lab-forgejo-daemon\"\n",
    )
    .expect("daemon config");
    let token_path = temp.path().join("forgejo-token");
    std::fs::write(&token_path, "test-token").expect("token file");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let forge = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".to_string())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.lab.flotilla.work".to_string()]))
        .https_url("https://forgejo.lab.flotilla.work".to_string())
        .git_ssh_host("forgejo.lab.flotilla.work".to_string())
        .build();
    backend.definitions::<flotilla_resources::Forge>("flotilla").create(&test_meta("lab"), &forge).await.expect("Forge");
    let repository = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops")
        .expect("repository")
        .on_forge(&forge)
        .expect("forge identity");
    let repository_key = repository.key();
    backend.clone().using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
    let remote_repository = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops").expect("remote repository");
    let remote_key = remote_repository.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&test_meta(&remote_key.to_string()), &remote_repository)
        .await
        .expect("repository awaiting forge identity migration");
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("lab-forgejo-daemon"),
            &CredentialSpecSpec::builder()
                .consumer(CredentialConsumer::Forgejo { forge_ref: "lab".to_string(), username: "crew".to_string() })
                .source(CredentialSource::File { path: token_path.to_string_lossy().into_owned() })
                .lifecycle(CredentialLifecycle::Static)
                .build(),
        )
        .await
        .expect("credential");
    let provider = Arc::new(FakeChangeRequest::new());
    provider
        .add_change_requests(vec![(
            "17".to_string(),
            ChangeRequest {
                title: "Fix ghostty".to_string(),
                branch: "governor".to_string(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: None,
                provider_name: "forgejo".to_string(),
                provider_display_name: "Forgejo".to_string(),
            },
        )])
        .await;
    let mut discovery = fake_discovery(false);
    discovery.factories.change_requests = vec![Box::new(ForgeAwareTestChangeRequestFactory(provider))];
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let resolved =
        daemon.resolve_convoy_change_request(std::slice::from_ref(&repository_key), "governor", None).await.expect("resolve Forgejo PR");
    assert_eq!(
        resolved,
        Some(ConvoyChangeRequest {
            id: "17".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: repository_key.clone(),
        })
    );
    let resolved =
        daemon.resolve_convoy_change_request(std::slice::from_ref(&remote_key), "governor", None).await.expect("resolve remote by host");
    assert_eq!(
        resolved,
        Some(ConvoyChangeRequest {
            id: "17".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: remote_key
        })
    );

    let second_url = "https://forgejo.lab.flotilla.work/robert/other";
    let second_repository = RepositorySpec::remote(second_url).expect("second repository").on_forge(&forge).expect("forge identity");
    let second_key = second_repository.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&test_meta(&second_key.to_string()), &second_repository)
        .await
        .expect("second repository");
    let convoy_spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![
            ConvoyRepositorySpec {
                url: "https://forgejo.lab.flotilla.work/robert/ghostty-ops".into(),
                repo_ref: repository_key.clone(),
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "ghostty-ops".into(),
                subpaths: Vec::new(),
            },
            ConvoyRepositorySpec {
                url: second_url.into(),
                repo_ref: second_key,
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "other".into(),
                subpaths: Vec::new(),
            },
        ])
        .r#ref("governor".to_string())
        .build();
    let convoys = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    convoys.create(&test_meta("multi-repo"), &convoy_spec).await.expect("convoy");
    daemon.discover_convoy_branch_subjects("flotilla", "multi-repo", "governor").await.expect("branch discovery");
    let convoy = convoys.get("multi-repo").await.expect("convoy after discovery");
    assert_eq!(convoy.status.as_ref().expect("status").subjects.len(), 2);
    assert!(convoy
        .status
        .as_ref()
        .expect("status")
        .subjects
        .iter()
        .all(|entry| entry.relationship == flotilla_protocol::Relationship::Produces));

    let claim = crate::checkout_integration::change_request_subjects_from_claim(
        "https://forgejo.lab.flotilla.work/robert/ghostty-ops/pulls/18",
        &convoy.spec.repositories,
        &[forge],
    );
    assert_eq!(claim.len(), 1);
    flotilla_resources::apply_status_patch(
        &convoys,
        "multi-repo",
        &ConvoyStatusPatch::DiscoverSubjects {
            subjects: vec![(claim[0].clone(), flotilla_protocol::Relationship::Produces)],
            source: flotilla_resources::SubjectDiscoverySource::Claim,
            at: Utc::now(),
        },
    )
    .await
    .expect("claim discovery");
    let with_followup = convoys.get("multi-repo").await.expect("convoy with follow-up");
    let leaves = flotilla_resources::expected_change_request_leaves(&with_followup, &BTreeMap::new()).expect("subject leaves");
    assert_eq!(leaves.len(), 6, "all three produced change requests require terminal observations");
    assert!(daemon
        .link_convoy_subject("flotilla", "multi-repo", "wheelhouze/cleat!12", Some(flotilla_protocol::Relationship::Produces))
        .await
        .expect_err("unknown GitHub repository must be rejected")
        .contains("outside this convoy's repositories"));
    assert!(daemon
        .link_convoy_subject(
            "flotilla",
            "multi-repo",
            "https://forgejo.lab.flotilla.work/robert/foreign/pulls/12",
            Some(flotilla_protocol::Relationship::Produces)
        )
        .await
        .expect_err("foreign Forgejo repository must be rejected")
        .contains("outside this convoy's repositories"));
    daemon
        .link_convoy_subject("flotilla", "multi-repo", "lab:robert/ghostty-ops!18", Some(flotilla_protocol::Relationship::Supersedes))
        .await
        .expect("operator resolution");
    let superseded = convoys.get("multi-repo").await.expect("convoy with superseded request");
    let leaves = flotilla_resources::expected_change_request_leaves(&superseded, &BTreeMap::new()).expect("subject leaves");
    assert_eq!(leaves.len(), 4, "supersedes releases only the replaced change request");
}

struct ConcurrentCreateRepositoryInspector;

#[async_trait]
impl RepositoryInspector for ConcurrentCreateRepositoryInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote("https://github.com/owner/repo")?,
            checkout: LocalCheckoutInspection::builder()
                .path(path.to_path_buf())
                .host_ref("host-test".to_string())
                .git_ref("main".to_string())
                .is_main(true)
                .build(),
            transport_url: Some("https://github.com/owner/repo".to_string()),
            replaces_prior_repository: false,
        })
    }

    async fn verify_continuity(&self, _path: &Path, _previous: &RepositorySpec) -> RepositoryContinuity {
        RepositoryContinuity::Continuous { evidence: "test".to_string() }
    }
}

/// Hold both free-space probes until both create requests have reached admission.
/// This exercises duplicate requests without sleeps or real Git subprocesses.
/// The probe runs under `spawn_blocking`, so this barrier never blocks an async
/// executor thread. Keep that boundary when changing the admission probe.
struct ConcurrentCreateSpaceProbe {
    arrivals: std::sync::Barrier,
    calls: AtomicUsize,
}

impl AvailableSpaceProbe for ConcurrentCreateSpaceProbe {
    fn measure(&self, _path: &Path) -> Option<u64> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
            self.arrivals.wait();
        }
        Some(100 * 1024 * 1024 * 1024)
    }
}

#[tokio::test]
async fn concurrent_duplicate_adopted_convoy_creates_leave_only_the_winning_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).expect("checkout directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"duplicate-create\"\n").expect("daemon config");
    let mut discovery = fake_discovery(false);
    discovery.available_space_probe =
        Arc::new(ConcurrentCreateSpaceProbe { arrivals: std::sync::Barrier::new(2), calls: AtomicUsize::new(0) });
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::local(),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    daemon.set_repository_inspector(Arc::new(ConcurrentCreateRepositoryInspector)).await;
    let backend = daemon.resource_backend();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder().name("work".to_string()).build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Tool { command: "true".to_string() })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow");
    let command = Command::builder()
        .action(CommandAction::ConvoyCreate {
            name: "duplicate".to_string(),
            workflow_ref: "work".to_string(),
            inputs: Vec::new(),
            repository_url: None,
            r#ref: None,
            project_ref: None,
            placement_policy: None,
            adopted_checkout: Some(Box::new(repo)),
        })
        .build();
    let mut events = daemon.subscribe();
    let (first, second) = tokio::join!(daemon.execute(command.clone()), daemon.execute(command));
    let ids = [first.expect("first command"), second.expect("second command")];
    let mut results = Vec::new();
    while results.len() < 2 {
        if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("command event") {
            if ids.contains(&command_id) {
                results.push(result);
            }
        }
    }
    assert_eq!(results.iter().filter(|result| matches!(result, CommandValue::ConvoyCreated { .. })).count(), 1, "{results:?}");
    assert_eq!(
        results.iter().filter(|result| matches!(result, CommandValue::Error { message } if message.contains("already exists"))).count(),
        1,
        "{results:?}"
    );
    let convoys = backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items;
    assert_eq!(convoys.len(), 1);
    let owned = convoys[0].spec.adopted_checkout_refs.values().cloned().collect::<BTreeSet<_>>();
    let durable = backend.using::<ResourceCheckout>("flotilla").list().await.expect("durable checkouts").items;
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>("flotilla").list().await.expect("observed checkouts").items;
    assert_eq!(durable.len(), 1, "loser must not author a durable checkout");
    assert_eq!(observed.len(), 1, "loser must not publish an orphan observed checkout");
    assert!(durable.iter().chain(&observed).all(|checkout| owned.contains(&checkout.metadata.name)));
}

#[tokio::test]
async fn prepared_workflow_snapshot_reuses_an_identical_replica() {
    let home_root = NodeId::new("snapshot-home");
    let driver_root = NodeId::new("snapshot-driver");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver_root);
    let spec = flotilla_resources::single_agent_workflow_spec();
    let name = prepared_snapshot_name("workflow", &serde_json::to_value(&spec).expect("serialize workflow")).expect("snapshot name");

    ensure_prepared_workflow_snapshot(&home, "flotilla", &name, &spec).await.expect("author snapshot on home");
    driver
        .replica_writer::<WorkflowTemplate>(home_root, "flotilla")
        .replace(&home.using::<WorkflowTemplate>("flotilla").list().await.expect("home workflow log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    ensure_prepared_workflow_snapshot(&driver, "flotilla", &name, &spec).await.expect("reuse identical replicated snapshot");
    assert!(driver.using::<WorkflowTemplate>("flotilla").list().await.expect("driver local workflow log").items.is_empty());
}

#[tokio::test]
async fn prepared_placement_snapshot_reuses_an_identical_replica() {
    let home_root = NodeId::new("snapshot-home");
    let driver_root = NodeId::new("snapshot-driver");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver_root);
    let spec = PlacementPolicySpec::builder().pool("passthrough".to_string()).build();
    let name = prepared_snapshot_name("placement", &serde_json::to_value(&spec).expect("serialize placement")).expect("snapshot name");

    ensure_prepared_placement_snapshot(&home, "flotilla", &name, &spec).await.expect("author snapshot on home");
    driver
        .replica_writer::<PlacementPolicy>(home_root, "flotilla")
        .replace(&home.using::<PlacementPolicy>("flotilla").list().await.expect("home placement log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    ensure_prepared_placement_snapshot(&driver, "flotilla", &name, &spec).await.expect("reuse identical replicated snapshot");
    assert!(driver.using::<PlacementPolicy>("flotilla").list().await.expect("driver local placement log").items.is_empty());
}

#[tokio::test]
async fn prepared_placement_snapshot_rejects_a_different_replica_spec() {
    let home_root = NodeId::new("snapshot-home");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("snapshot-driver"));
    let name = "placement-snapshot-collision";
    let authored = PlacementPolicySpec::builder().pool("passthrough".to_string()).build();
    let requested = PlacementPolicySpec::builder().pool("cleat".to_string()).build();
    home.using::<PlacementPolicy>("flotilla").create(&test_meta(name), &authored).await.expect("author snapshot on home");
    driver
        .replica_writer::<PlacementPolicy>(home_root, "flotilla")
        .replace(&home.using::<PlacementPolicy>("flotilla").list().await.expect("home placement log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    let error =
        ensure_prepared_placement_snapshot(&driver, "flotilla", name, &requested).await.expect_err("mismatched replica must be refused");
    assert_eq!(error, format!("prepared placement snapshot {name} already exists with different contents"));
    assert!(driver.using::<PlacementPolicy>("flotilla").list().await.expect("driver local placement log").items.is_empty());
}

#[tokio::test]
async fn abandon_archive_skips_pushed_head_pushes_unpushed_head_and_reports_push_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"archive-test\"\n").expect("daemon config");
    let runner = Arc::new(MockRunner::new(vec![
        Ok("git version 2.43.0".to_string()),
        Ok("git version 2.43.0".to_string()),
        Ok("archived".to_string()),
        Err("remote rejected".to_string()),
        Ok("archived stale head".to_string()),
    ]));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let mut discovery = fake_discovery_with_runner(false, runner.clone());
    discovery.repo_detectors.clear();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::local(),
        backend.clone(),
    )
    .await;
    let repository = RepositorySpec::remote("https://github.com/acme/archive").expect("repository spec").key();
    let checkouts = backend.using::<ResourceCheckout>("flotilla");
    let fresh = Utc::now().to_rfc3339();
    for (name, pushed, observed_at) in [
        ("already-pushed", ConditionValue::True, fresh.as_str()),
        ("needs-push", ConditionValue::False, fresh.as_str()),
        ("push-fails", ConditionValue::False, fresh.as_str()),
        ("stale-pushed", ConditionValue::True, "2020-01-01T00:00:00Z"),
    ] {
        let checkout = checkouts
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "archive-convoy".to_string())]))
                    .build(),
                &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                    r#ref: name.to_string(),
                    path: format!("/checkouts/{name}"),
                    repo_ref: repository.clone(),
                    host_ref: "archive-test".to_string(),
                    is_main: false,
                }),
            )
            .await
            .expect("checkout");
        checkouts
            .update_status(
                name,
                &checkout.metadata.resource_version,
                &ResourceCheckoutStatus::builder()
                    .phase(ResourceCheckoutPhase::Ready)
                    .path(format!("/checkouts/{name}"))
                    .integration(CheckoutIntegrationStatus {
                        head_revision: None,
                        pushed: IntegrationCondition::builder().value(pushed).observed_at(observed_at.to_string()).build(),
                        ..CheckoutIntegrationStatus::default()
                    })
                    .build(),
            )
            .await
            .expect("checkout status");
    }

    let outcomes = daemon.crew_ops.archive_convoy_checkouts_best_effort("flotilla", "archive-convoy").await.expect("best-effort archive");

    assert_eq!(
        outcomes.iter().map(|outcome| (outcome.checkout.as_str(), outcome.status)).collect::<Vec<_>>(),
        vec![
            ("already-pushed", CheckoutArchiveStatus::NothingToArchive),
            ("needs-push", CheckoutArchiveStatus::Archived),
            ("push-fails", CheckoutArchiveStatus::Failed),
            ("stale-pushed", CheckoutArchiveStatus::Archived),
        ]
    );
    assert_eq!(outcomes[2].detail.as_deref(), Some("remote rejected"));
    assert_eq!(
        runner.calls().iter().filter(|(command, args)| command == "git" && args.first().is_some_and(|arg| arg == "push")).count(),
        3
    );
}

#[tokio::test]
async fn bound_change_request_resolution_uses_durable_observation_for_a_mirror_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"bound-pr-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla")
        .expect("repository spec")
        .with_remotes(["https://github.com/flotilla-org/flotilla".to_string(), "https://forgejo.example/flotilla/flotilla".to_string()])
        .expect("mirror declaration");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("repository");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let change_requests = daemon.resource_backend().using::<ResourceChangeRequest>("flotilla");
    let change_request_name = change_request_record_name("github.com", "flotilla-org/flotilla", 1696);
    let observation = change_requests
        .create(
            &test_meta(&change_request_name),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(1696)
                .observing_authority("github-observer".to_string())
                .build(),
        )
        .await
        .expect("change request observation");
    let observed_at = Utc::now();
    change_requests
        .update_status(
            &observation.metadata.name,
            &observation.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(ObservedChangeRequestState::Open, observed_at),
                head_sha: flotilla_resources::Observation::unknown(observed_at),
                checks: flotilla_resources::Observation::unknown(observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
                },
                mergeable: flotilla_resources::Observation::unknown(observed_at),
            },
        )
        .await
        .expect("change request status");

    let resolved = daemon
        .resolve_convoy_change_request(std::slice::from_ref(&repository_key), "fix/convoy-pr-linkage", Some("1696"))
        .await
        .expect("bound change request lookup")
        .expect("durable observation should resolve the bound change request");

    assert_eq!(resolved.id, "1696");
    assert_eq!(resolved.repository_key, repository_key);
    assert_eq!(resolved.status, flotilla_protocol::ChangeRequestStatus::Open);

    // #2202: a bound ID remains authoritative for both the row and discovery;
    // duplicate repository keys reuse the durable observation without a scan.
    let binding = BoundChangeRequest { id: "1696".into(), repository_ref: repository_key.clone(), title: "Existing PR".into() };
    let refresh =
        daemon.refresh_convoy_branch(&[repository_key.clone(), repository_key.clone()], "fix/convoy-pr-linkage", Some(&binding)).await;
    assert_eq!(refresh.primary.expect("primary"), Some(resolved.clone()));
    assert_eq!(refresh.repositories, vec![(repository_key, Ok(Some(resolved)))]);
}

struct BatchedObservationRunner {
    calls: std::sync::Mutex<Vec<String>>,
    rate_limit_two: std::sync::atomic::AtomicBool,
    hard_error_one: std::sync::atomic::AtomicBool,
    rate_limit_all: std::sync::atomic::AtomicBool,
    mixed_history_errors: std::sync::atomic::AtomicBool,
    block_one: std::sync::atomic::AtomicBool,
    one_started: tokio::sync::Notify,
    release_one: tokio::sync::Notify,
    conflicting: std::sync::atomic::AtomicBool,
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

#[derive(Default)]
struct BusyObservationRunner {
    pages: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl CommandRunner for BusyObservationRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        Ok("test version".into())
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<crate::providers::CommandOutput, String> {
        if cmd == "gh" && args.first() == Some(&"api") && args.get(1) == Some(&"--include") {
            let number =
                args.get(2).ok_or("REST endpoint")?.rsplit('/').next().ok_or("PR number")?.parse::<u64>().map_err(|e| e.to_string())?;
            return Ok(CommandOutput {
                stdout: format!(
                    "HTTP/2 200 OK\r\nETag: busy\r\n\r\n{}",
                    serde_json::json!({"number":number,"updated_at":"2026-10-07T00:00:00Z"})
                ),
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
        let query = args.iter().find_map(|arg| arg.strip_prefix("query=")).ok_or("query")?;
        let comments = serde_json::json!({"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []});
        let document = if query.contains("pr:pullRequest") {
            self.pages.lock().expect("pages").push(query.into());
            serde_json::json!({"data": {"repository": {"pr": {"comments": comments}}}})
        } else {
            let busy = serde_json::json!({"state": "OPEN", "reviewDecision": null, "comments": comments,
                "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}});
            serde_json::json!({"data": {"repository": {"pr1": busy, "pr2": busy, "pr3": busy}}})
        };
        Ok(crate::providers::CommandOutput {
            stdout: format!("HTTP/2 200 OK\r\n\r\n{document}"),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
}

#[tokio::test(start_paused = true)]
async fn source_pagination_fairness_survives_provider_rediscovery() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), r#"machine_id = "fair-pagination-test""#).expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository = RepositorySpec::remote("https://github.com/team/one").expect("repository");
    backend.using::<Repository>("flotilla").create(&test_meta(&repository.key().to_string()), &repository).await.expect("repository");
    let runner = Arc::new(BusyObservationRunner::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("capability");
    let subjects = [1, 2, 3].map(|number| ChangeRequestRef {
        namespace: "flotilla".into(),
        service: "github.com".into(),
        scope: "team/one".into(),
        number,
    });
    for _ in 0..4 {
        let error = daemon.change_request_observation_source.observe_group(&subjects, &subjects[0]).await.expect_err("incomplete history");
        assert!(error.to_string().contains("pagination budget"));
        tokio::time::advance(Duration::from_secs(10)).await;
    }
    let pages = runner.pages.lock().expect("pages");
    assert_eq!(pages.len(), 32, "all four cycles retain the eight-page shared budget");
    for (cycle, number) in [1, 2, 3, 1].into_iter().enumerate() {
        assert!(pages[cycle * 8].contains(&format!("number:{number}")), "priority rotates through every PR and wraps across rediscovery");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestAdmissionReply {
    Ordinary,
    Limited,
    NoDeadline,
    Secondary,
    Absent,
    MissingBase,
    Success,
}

#[derive(Clone, Copy)]
enum RestAdmissionLookup {
    Id,
    Branch,
}

enum RestAdmissionSelection {
    Failure(RestAdmissionReply),
    Absent,
    Found(usize),
    Ambiguous,
}

// GitHub subprocess boundary: retain failed stdout headers as the real CLI does.
struct AdmissionRestRunner {
    responses: BTreeMap<String, CommandOutput>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl CommandRunner for AdmissionRestRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &crate::providers::ChannelLabel) -> Result<String, String> {
        panic!("admission REST reads use run_output")
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        _cwd: &Path,
        _label: &crate::providers::ChannelLabel,
    ) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "gh");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let endpoint = args.iter().find(|arg| arg.starts_with("repos/")).expect("REST endpoint");
        let scope = endpoint.strip_prefix("repos/").expect("repository path").split("/pulls").next().expect("scope");
        let response = self.responses.get(scope).expect("configured repository");
        Ok(CommandOutput { stdout: response.stdout.clone(), stderr: response.stderr.clone(), exit_code: response.exit_code })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

fn rest_admission_response(reply: RestAdmissionReply, lookup: RestAdmissionLookup) -> CommandOutput {
    use RestAdmissionReply::*;
    let (status, headers, body, success) = match reply {
        Limited => (
            403,
            "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1893456000\r\n",
            serde_json::json!({"message":"API rate limit exceeded"}),
            false,
        ),
        NoDeadline => (403, "X-RateLimit-Remaining: 0\r\n", serde_json::json!({"message":"API rate limit exceeded"}), false),
        Secondary => {
            (403, "X-RateLimit-Remaining: 4989\r\nRetry-After: 30\r\n", serde_json::json!({"message":"secondary rate limit"}), false)
        }
        Ordinary => (
            403,
            "X-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1893456000\r\n",
            serde_json::json!({"message":"Resource not accessible by integration"}),
            false,
        ),
        Absent => match lookup {
            RestAdmissionLookup::Branch => (200, "", serde_json::json!([]), true),
            RestAdmissionLookup::Id => (404, "", serde_json::json!({"message":"Not Found"}), false),
        },
        MissingBase | Success => {
            let pr = serde_json::json!({"number":7,"title":"Wanted","head":{"ref":"feature/wanted"},"base":{"ref": if reply == MissingBase { serde_json::Value::Null } else { serde_json::json!("main") }},"state":"open"});
            (
                200,
                "",
                match lookup {
                    RestAdmissionLookup::Branch => serde_json::json!([pr]),
                    RestAdmissionLookup::Id => pr,
                },
                true,
            )
        }
    };
    CommandOutput {
        stdout: format!("HTTP/2 {status}\r\n{headers}\r\n{body}"),
        stderr: if reply == Ordinary { "rate limited diagnostics unavailable".into() } else { "gh: Not Found".into() },
        exit_code: Some(if success { 0 } else { 1 }),
    }
}

struct RestAdmissionFixture {
    calls: Arc<AtomicUsize>,
    daemon: Arc<InProcessDaemon>,
    keys: Vec<RepositoryKey>,
    _config: tempfile::TempDir,
}

async fn rest_admission_fixture(outcomes: [RestAdmissionReply; 2], lookup: RestAdmissionLookup) -> RestAdmissionFixture {
    use crate::providers::{change_request::github::GitHubChangeRequest, github_api::GhApiClient};
    let config = tempfile::tempdir().expect("tempdir");
    std::fs::write(config.path().join("daemon.toml"), "machine_id = \"rest-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let responses = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, reply)| (format!("team/repo{index}"), rest_admission_response(reply, lookup)))
        .collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(AdmissionRestRunner { responses, calls: Arc::clone(&calls) });
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(config.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".into()).await;
    let mut keys = Vec::new();
    for index in 0..2 {
        let scope = format!("team/repo{index}");
        let repository = RepositorySpec::remote(format!("https://github.com/{scope}")).expect("repository");
        let key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository");
        daemon.convoy_admission.repository_change_requests.write().await.insert(
            key.clone(),
            RepositoryChangeRequestProvider {
                service_url: repository.forge().expect("forge").service_url.clone(),
                repository: scope.clone(),
                provider: Arc::new(GitHubChangeRequest::new(
                    "github".into(),
                    scope,
                    Arc::new(GhApiClient::new(runner.clone())),
                    runner.clone(),
                )),
            },
        );
        keys.push(key);
    }
    RestAdmissionFixture { calls, daemon, keys, _config: config }
}

// #2585: the real convoy resolver shares proven branch absence across callers
// while preserving ordered repository selection and refreshing after five minutes.
#[tokio::test(start_paused = true)]
async fn convoy_resolver_reuses_absence_observations() {
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    for _ in 0..3 {
        assert!(fixture.daemon.resolve_convoy_change_request(&fixture.keys, "feature/wanted", None).await.expect("absence").is_none());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    }
    assert!(fixture.daemon.resolve_convoy_change_request(&fixture.keys, "feature/other", None).await.expect("another branch").is_none());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
    tokio::time::advance(Duration::from_secs(300)).await;
    assert!(fixture.daemon.resolve_convoy_change_request(&fixture.keys, "feature/wanted", None).await.expect("expired absence").is_none());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 6);
}

// #2541: both REST lookup paths choose classified limits over ordinary text,
// preserve ordinary diagnostics, and retain successful/ambiguous repo selection.
// This finite matrix exhausts ordinary, absent, primary (with/without deadline),
// secondary, missing-base and successful reads; multiple limits retain the first
// branch failure and all ID diagnostics, preserving the existing lookup policy.
#[tokio::test]
async fn rest_admission_lookup_selection_matrix() {
    use RestAdmissionReply::*;
    use RestAdmissionSelection::{Ambiguous, Failure, Found};
    let cases = [
        ([Ordinary, Limited], Failure(Limited), Failure(Limited)),
        ([Limited, Ordinary], Failure(Limited), Failure(Limited)),
        ([Ordinary, NoDeadline], Failure(NoDeadline), Failure(NoDeadline)),
        ([Ordinary, Secondary], Failure(Secondary), Failure(Secondary)),
        ([Limited, Secondary], Failure(Limited), Failure(Limited)),
        ([Secondary, Limited], Failure(Secondary), Failure(Secondary)),
        ([Ordinary, Ordinary], Failure(Ordinary), Failure(Ordinary)),
        ([Absent, Ordinary], Failure(Ordinary), Failure(Ordinary)),
        ([Absent, Absent], Failure(Absent), RestAdmissionSelection::Absent),
        ([MissingBase, Ordinary], Failure(MissingBase), Found(0)),
        ([Limited, Success], Found(1), Found(1)),
        ([Success, Limited], Found(0), Found(0)),
        ([Success, Success], Ambiguous, Found(0)),
    ];
    for (outcomes, id_selection, branch_selection) in cases {
        for (lookup, expected) in [(RestAdmissionLookup::Id, id_selection), (RestAdmissionLookup::Branch, branch_selection)] {
            let fixture = rest_admission_fixture(outcomes, lookup).await;
            let result = match lookup {
                RestAdmissionLookup::Branch => fixture
                    .daemon
                    .resolve_convoy_change_request(&fixture.keys, "feature/wanted", None)
                    .await
                    .map(|found| found.map(|found| found.repository_key)),
                RestAdmissionLookup::Id => {
                    fixture.daemon.convoy_admission.resolve_convoy_change_request_admission(&fixture.keys, "7").await.map(|found| {
                        assert_eq!(found.branch, "feature/wanted");
                        assert_eq!(found.base_ref, "main");
                        Some(found.binding.repository_ref)
                    })
                }
            };
            match expected {
                Found(index) => assert_eq!(result.expect("successful lookup"), Some(fixture.keys[index].clone())),
                RestAdmissionSelection::Absent => assert!(result.expect("no matching request").is_none()),
                Ambiguous => assert_eq!(
                    result.expect_err("ambiguous lookup"),
                    "change request 7 is ambiguous across 2 consulted repositories [team/repo0, team/repo1]"
                ),
                Failure(reply) => {
                    let error = result.expect_err("lookup refused");
                    match reply {
                        Limited | NoDeadline | Secondary => {
                            assert!(error.contains("budget=REST core"), "{outcomes:?}: {error}");
                            let kind = if reply == Secondary { "kind=secondary" } else { "kind=primary" };
                            assert!(error.contains(kind), "{error}");
                            if reply == NoDeadline {
                                assert!(error.contains("retry_at=unavailable"), "{error}");
                            }
                            if reply == Limited {
                                assert!(error.contains("retry_source=x-ratelimit-reset, retry_at=2030-01-01T00:00:00+00:00"), "{error}");
                            }
                            match lookup {
                                RestAdmissionLookup::Id => {
                                    for (index, outcome) in outcomes.iter().enumerate() {
                                        assert!(error.contains(&format!("repository team/repo{index}:")), "{error}");
                                        if *outcome == Ordinary {
                                            assert!(
                                                error.contains(&format!(
                                                    "repository team/repo{index}: rate limited diagnostics unavailable"
                                                )),
                                                "{error}"
                                            );
                                        }
                                    }
                                }
                                RestAdmissionLookup::Branch => {
                                    assert!(!error.contains("diagnostics unavailable"), "{error}");
                                    assert!(!error.contains(if reply == Secondary { "kind=primary" } else { "kind=secondary" }), "{error}");
                                }
                            }
                        }
                        Ordinary => {
                            match lookup {
                                RestAdmissionLookup::Branch => assert_eq!(error, "rate limited diagnostics unavailable"),
                                RestAdmissionLookup::Id => {
                                    let diagnostics = outcomes
                                        .iter()
                                        .enumerate()
                                        .map(|(index, outcome)| {
                                            let message =
                                                if *outcome == Absent { "gh: Not Found" } else { "rate limited diagnostics unavailable" };
                                            format!("repository team/repo{index}: {message}")
                                        })
                                        .collect::<Vec<_>>()
                                        .join("; ");
                                    assert_eq!(error, format!("change request 7 was not found in consulted repositories [team/repo0, team/repo1]: {diagnostics}"));
                                }
                            }
                        }
                        MissingBase => {
                            assert!(error.contains("repository team/repo0: change request 7 did not report a base ref"), "{error}")
                        }
                        Absent => assert!(error.contains("repository team/repo0: gh: Not Found"), "{error}"),
                        Success => panic!("success is not a refusal"),
                    }
                }
            }
        }
    }
}

// #2510: admission must prioritize a classified limit over an ordinary error
// whose diagnostic happens to mention rate limiting. Exercise real discovery,
// provider classification and admission; fake only the GitHub subprocess boundary.
#[tokio::test]
async fn bound_admission_prioritizes_typed_limit_over_misleading_diagnostic() {
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(true),
        hard_error_one: std::sync::atomic::AtomicBool::new(true),
        rate_limit_all: std::sync::atomic::AtomicBool::new(false),
        mixed_history_errors: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        conflicting: std::sync::atomic::AtomicBool::new(false),
    });
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"typed-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".into()).await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let mut keys = Vec::new();
    for scope in ["team/one", "team/two"] {
        let repository = RepositorySpec::remote(format!("https://github.com/{scope}")).expect("repository");
        let key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository");
        keys.push(key);
    }
    let error = daemon.resolve_convoy_change_request(&keys, "main", Some("1")).await.expect_err("no usable observation");
    assert!(error.contains("budget=GraphQL") && error.contains("kind=primary"), "return the classified limit: {error}");
    assert!(!error.contains("diagnostics unavailable"), "an ordinary error's words cannot override classification");
    assert_eq!(runner.calls.lock().expect("calls").len(), 2, "consult both repositories");
}

#[tokio::test]
async fn live_bound_observation_batches_two_repositories_and_caches_rate_limit() {
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(false),
        hard_error_one: std::sync::atomic::AtomicBool::new(false),
        rate_limit_all: std::sync::atomic::AtomicBool::new(false),
        mixed_history_errors: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        conflicting: std::sync::atomic::AtomicBool::new(false),
    });
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"batch-observation-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let mut subjects = Vec::new();
    for (scope, ids) in [("team/one", vec![1, 2, 3]), ("team/two", vec![4, 5])] {
        let scheme = if scope == "team/one" { "http" } else { "https" };
        let repository = RepositorySpec::remote(format!("{scheme}://github.com/{scope}")).expect("repository");
        let repository_key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
        for id in ids {
            let mut spec = ConvoySpec::builder()
                .workflow_ref("test".to_string())
                .repositories(vec![ConvoyRepositorySpec {
                    url: format!("{scheme}://github.com/{scope}"),
                    repo_ref: repository_key.clone(),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: scope.replace('/', "-"),
                    subpaths: Vec::new(),
                }])
                .build();
            spec.change_request =
                Some(BoundChangeRequest { id: id.to_string(), repository_ref: repository_key.clone(), title: format!("PR {id}") });
            backend.using::<ResourceConvoy>("flotilla").create(&test_meta(&format!("convoy-{id}")), &spec).await.expect("convoy");
            subjects.push(ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: scope.into(), number: id });
        }
    }
    for subject in &subjects {
        daemon.change_request_observation_source.observe(subject).await.expect("live observation");
    }
    assert_eq!(runner.calls.lock().expect("calls").len(), 2, "one GitHub request per repository for five bound CRs");

    let first = &subjects[0];
    let repository_key = backend
        .using::<Repository>("flotilla")
        .list()
        .await
        .expect("repositories")
        .items
        .into_iter()
        .find(|repository| repository.spec.forge().is_some_and(|forge| forge.repository == "team/one"))
        .expect("first repository")
        .spec
        .key();
    let mut spec = ConvoySpec::builder()
        .workflow_ref("test".to_string())
        .repositories(vec![ConvoyRepositorySpec {
            url: "http://github.com/team/one".into(),
            repo_ref: repository_key.clone(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "team-one".into(),
            subpaths: Vec::new(),
        }])
        .build();
    spec.change_request = Some(BoundChangeRequest { id: "6".into(), repository_ref: repository_key, title: "PR 6".into() });
    backend.using::<ResourceConvoy>("flotilla").create(&test_meta("convoy-6"), &spec).await.expect("new convoy");
    daemon.change_request_observation_source.observe(first).await.expect("expanded batch");
    assert_eq!(runner.calls.lock().expect("calls").len(), 3, "a new bound CR invalidates its repository batch");

    runner.rate_limit_two.store(true, std::sync::atomic::Ordering::SeqCst);
    let limited = &subjects[3];
    let error = daemon.change_request_observation_source.observe_for_completion(limited).await.expect_err("fresh read is rate limited");
    assert!(
        error.to_string().contains("budget=GraphQL, identity=host gh login, kind=primary, retry_source=x-ratelimit-reset, retry_at="),
        "{error}"
    );
    assert_eq!(runner.calls.lock().expect("calls").len(), 4);
    assert_eq!(daemon.change_request_observation_source.observe(limited).await.expect_err("cached rate limit"), error);
    assert_eq!(runner.calls.lock().expect("calls").len(), 4, "rate-limited repository waits for reset");

    let before = runner.calls.lock().expect("calls").len();
    let first_error = daemon
        .change_request_observation_source
        .observe_for_completion(first)
        .await
        .expect_err("identity cooldown covers the other repository too");
    assert!(first_error.to_string().contains("budget=GraphQL"));
    tokio::time::timeout(Duration::from_secs(1), daemon.change_request_observation_source.observe_for_completion(limited))
        .await
        .expect("cached second repository must not block")
        .expect_err("second repository remains rate limited");
    assert_eq!(runner.calls.lock().expect("calls").len(), before, "no repository spends the exhausted identity budget");
}

#[tokio::test]
async fn claim_message_pr_is_observed_and_repeated_conflicting_refusal_escalates() {
    completion_claim_observation_case(false, false, false).await;
}

// #2499: a timed observation limit waits without increasing refusal strikes,
// then admits the same claim after fresh readiness is actually observed.
#[tokio::test(start_paused = true)]
async fn rate_limited_completion_waits_then_requires_fresh_ready_observation() {
    completion_claim_observation_case(true, false, false).await;
}

#[tokio::test(start_paused = true)]
async fn rate_limited_completion_defers_but_preserves_non_forge_gate() {
    completion_claim_observation_case(true, true, false).await;
}

// #2510: a limited PR never masks another PR's hard error, even on a cached
// completion read; a refusal records a strike and recovery needs fresh evidence.
#[tokio::test(start_paused = true)]
async fn mixed_observation_completion_refuses_and_recovers() {
    completion_claim_observation_case(false, false, true).await;
}

async fn completion_claim_observation_case(rate_limited: bool, missing_artifact: bool, mixed: bool) {
    #[derive(Default)]
    struct DeliveredTurns(std::sync::Mutex<Vec<crate::leaf_engine::CrewTurnIntent>>);
    #[async_trait]
    impl crate::leaf_engine::TurnDeliveryActuator for DeliveredTurns {
        async fn deliver(&self, request: &crate::leaf_engine::CrewTurnIntent) -> Result<crate::leaf_engine::CrewTurnAdmission, String> {
            self.0.lock().expect("turns").push(request.clone());
            Ok(crate::leaf_engine::CrewTurnAdmission {
                new_turn: true,
                rung: flotilla_resources::TurnDeliveryRung::WarmSession,
                message: flotilla_protocol::ResourceRef::new(
                    "flotilla.work/v1",
                    "Message",
                    &request.namespace,
                    format!("fake-{}-{}", request.source, request.subject_revision),
                ),
            })
        }

        async fn hold(&self, _: &crate::leaf_engine::CrewTurnIntent, _: &flotilla_resources::HoldAct, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"refused-claim-test\"\n").expect("daemon config");
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(false),
        hard_error_one: std::sync::atomic::AtomicBool::new(false),
        rate_limit_all: std::sync::atomic::AtomicBool::new(false),
        mixed_history_errors: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        conflicting: std::sync::atomic::AtomicBool::new(true),
    });
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let turns = Arc::new(DeliveredTurns::default());
    daemon.crew_ops.set_turn_delivery_actuator(turns.clone()).await;
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
    let repository_key = repository.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &test_meta("refused-claim"),
            &ConvoySpec::builder()
                .workflow_ref("implement-review".to_string())
                .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
                    .url("https://github.com/flotilla-org/flotilla".to_string())
                    .repo_ref(repository_key)
                    .source_ref("2205/escalate".to_string())
                    .target_ref("main".to_string())
                    .workspace_slug("flotilla".to_string())
                    .subpaths(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("convoy");
    let ready = flotilla_resources::CrewCompletionExpectation::Condition(flotilla_resources::CompletionCondition::ChangeRequest {
        field_path: ".ready".to_string(),
        operator: flotilla_protocol::LeafOperator::Equal,
        literal: "true".to_string(),
        optional_when_absent: false,
    });
    let mut conditions = vec![ready];
    if missing_artifact {
        conditions.push(flotilla_resources::CrewCompletionExpectation::artifact_exists(
            "coder",
            "review-bundle",
            flotilla_resources::ArtifactSubjectBinding::Convoy,
        ));
    }
    let coder = CrewSpec::builder()
        .role("coder".to_string())
        .source(CrewSource::Tool { command: "test".to_string() })
        .completion_conditions(conditions)
        .build();
    let bosun = CrewSpec::builder().role("bosun".to_string()).source(CrewSource::Tool { command: "test".to_string() }).build();
    convoys
        .update_status(
            "refused-claim",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: indexmap::IndexMap::from([(
                        "work/coder".to_string(),
                        flotilla_resources::StallNudgePolicy { max_per_episode: 2, max_refusals: None, idle_grace_seconds: Some(0) },
                    )]),
                    supervision: Some(vec![flotilla_resources::SupervisionTarget::ConvoyCrew {
                        vessel: "work".to_string(),
                        role: "bosun".to_string(),
                    }]),
                    exit: None,
                    turn_delivery: indexmap::IndexMap::from([(
                        "conflicting".to_string(),
                        flotilla_resources::TurnDeliveryRule::builder()
                            .on("$cr.mergeable == conflicting".parse().expect("conflict leaf"))
                            .to(flotilla_resources::TurnDeliveryTarget::builder()
                                .vessel("work".to_string())
                                .role("coder".to_string())
                                .build())
                            .brief(
                                "Rebase onto the current base branch and file a fresh settlement claim; the previous claim is superseded."
                                    .to_string(),
                            )
                            .hold(flotilla_resources::HoldAct::State)
                            .build(),
                    )]),
                    vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![coder, bosun]).build()],
                }),
                work: BTreeMap::from([(
                    "work".to_string(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([
                        ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                        ("bosun".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    backend
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("refused-claim-vessel"),
            &VesselSpec {
                convoy_ref: "refused-claim".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    let session = backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("refused-claim-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "refused-claim".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief.md".to_string(),
                        content: "work".to_string(),
                        copies: vec![],
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "refused-claim".to_string(),
                        vessel_ref: "refused-claim-vessel".to_string(),
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
    backend
        .using::<ResourceTerminalSession>("flotilla")
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("idle session");
    let context = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("refused-claim".to_string()),
        vessel_ref: Some("refused-claim-vessel".to_string()),
        role: Some("coder".to_string()),
    };
    let claim = || {
        daemon.crew_complete_with_disposition_internal(
            &context,
            Some("https://github.com/flotilla-org/flotilla/pull/2200".to_string()),
            None,
            Some("https://github.com/flotilla-org/flotilla/pull/2200#issuecomment-1".to_string()),
        )
    };
    if mixed {
        let subjects = crate::checkout_integration::change_request_subjects_from_claim(
            "https://github.com/flotilla-org/flotilla/pull/2201",
            &convoys.get("refused-claim").await.expect("convoy").spec.repositories,
            &[],
        );
        assert_eq!(subjects.len(), 1);
        flotilla_resources::apply_status_patch(
            &convoys,
            "refused-claim",
            &ConvoyStatusPatch::DiscoverSubjects {
                subjects: vec![(subjects[0].clone(), flotilla_protocol::Relationship::Produces)],
                source: flotilla_resources::SubjectDiscoverySource::Claim,
                at: Utc::now(),
            },
        )
        .await
        .expect("second PR discovery");
        runner.mixed_history_errors.store(true, std::sync::atomic::Ordering::SeqCst);
        let first = claim().await.expect_err("mixed observations must refuse");
        assert!(first.contains("history access denied"), "{first}");
        let calls = runner.calls.lock().expect("calls").len();
        assert_eq!(calls, 3, "one shared batch and two history outcomes");
        // #2211: a hard observation failure retains its subject even if other PRs have evidence.
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert!(status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal").causes.contains(
            &CrewCompletionRefusalCause::MissingChangeRequestObservation {
                service: "github.com".into(),
                scope: "flotilla-org/flotilla".into(),
                number: 2200,
            }
        ));

        let second = claim().await.expect_err("cached hard error must still refuse");
        assert!(second.contains("history access denied"), "{second}");
        assert_eq!(runner.calls.lock().expect("calls").len(), calls, "cooldown prevents forge calls");
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        assert_eq!(status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal strike").consecutive_count, 2);
        runner.mixed_history_errors.store(false, std::sync::atomic::Ordering::SeqCst);
        runner.conflicting.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(claim().await.expect("recovered claim"), flotilla_protocol::CommandValue::Ok);
        assert_eq!(
            convoys.get("refused-claim").await.expect("convoy").status.expect("status").crew_work["work"]["coder"].phase,
            CrewWorkPhase::Done
        );
        for number in [2200, 2201] {
            let subject = ChangeRequestRef {
                namespace: "flotilla".into(),
                service: "github.com".into(),
                scope: "flotilla-org/flotilla".into(),
                number,
            };
            assert!(
                daemon.crew_ops.subscription_diagnostics().change_request_observation_error(&subject).await.is_none(),
                "recovery clears subject errors"
            );
        }
        return;
    }
    if rate_limited {
        runner.rate_limit_all.store(true, std::sync::atomic::Ordering::SeqCst);
        let wait = claim().await.expect("observation wait");
        let flotilla_protocol::CommandValue::CrewCompletionWaiting { reason, retry_at } = wait else { panic!("expected timed wait") };
        assert!(reason.contains("kind=secondary") && reason.contains("retry_source=retry-after"), "{reason}");
        assert!(retry_at < Utc::now() + chrono::Duration::minutes(2), "ignore unrelated primary window");
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert!(status.crew_work["work"]["coder"].completion_refusal.is_none());
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        let calls = runner.calls.lock().expect("calls").len();
        assert!(matches!(claim().await.expect("cached wait"), flotilla_protocol::CommandValue::CrewCompletionWaiting { .. }));
        assert_eq!(runner.calls.lock().expect("calls").len(), calls, "fresh completion must respect cooldown");
        runner.rate_limit_all.store(false, std::sync::atomic::Ordering::SeqCst);
        // Advance only the monotonic cache TTL. The old response's UTC deadline
        // remains future; expiry admits a new forge read, now healthy, before
        // completion is judged. The explicit two-clock test pins the conversion.
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(claim().await.expect_err("fresh conflict still refuses").contains(".ready"));
        runner.conflicting.store(false, std::sync::atomic::Ordering::SeqCst);
        if missing_artifact {
            assert!(claim().await.expect_err("non-forge gate still refuses").contains("review-bundle"));
            let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
            assert_ne!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
            assert!(status.crew_work["work"]["coder"].completion_refusal.is_some());
            return;
        }
        assert_eq!(claim().await.expect("fresh ready claim"), flotilla_protocol::CommandValue::Ok);
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        return;
    }
    let first = claim().await.expect_err("conflicting PR must refuse claim");
    assert!(first.contains("cr/github.com/flotilla-org/flotilla/2200") && first.contains(".ready"), "{first}");
    let refused_status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
    assert!(
        refused_status.subjects.iter().any(|entry| entry.subject.id == "2200"),
        "the rejected completion still discovered a PR in the convoy's repository"
    );
    // #2211: claim admission persists the cause and exact PR identity before nudging.
    assert_eq!(
        refused_status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal").causes,
        vec![CrewCompletionRefusalCause::ConflictingChangeRequest {
            service: "github.com".into(),
            scope: "flotilla-org/flotilla".into(),
            number: 2200,
        }]
    );
    assert_ne!(refused_status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Done);
    let observed = backend
        .using::<ResourceChangeRequest>("flotilla")
        .get(&change_request_record_name("github.com", "flotilla-org/flotilla", 2200))
        .await
        .expect("claim-time observation");
    assert_eq!(observed.status.expect("status").mergeable.value, Some(flotilla_resources::ObservedMergeability::Conflicting));

    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    let task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if turns.0.lock().expect("turns").iter().any(|request| {
                request.source == "stall-nudge-1" && request.brief.contains("PR #2200 is conflicting") && request.brief.contains(".ready")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refusal nudge");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if turns
                .0
                .lock()
                .expect("turns")
                .iter()
                .any(|request| request.source == "conflicting" && request.brief.contains("PR #2200 is conflicting"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active conflict delivery");
    let second = claim().await.expect_err("same conflicting PR must refuse again");
    assert_eq!(second, first);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
            if status.stalled.as_ref().is_some_and(|stall| {
                stall.rung == flotilla_resources::StallRung::Bosun && stall.evidence.contains("settlement claim refused 2 times")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervision escalation");
    assert_eq!(turns.0.lock().expect("turns").iter().filter(|request| request.source.starts_with("stall-nudge")).count(), 1);
    task.abort();
    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    let restarted = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        turns.0.lock().expect("turns").iter().filter(|request| request.source == "conflicting").count(),
        1,
        "a persisted turn episode prevents duplicate Active conflict delivery after restart"
    );
    restarted.abort();
}

#[test]
fn observation_service_matching_preserves_http_and_authority_port() {
    assert!(forge_service_matches("http://forgejo.local:3000", "forgejo.local:3000"));
    assert!(forge_service_matches("https://github.com/", "github.com"));
    assert!(!forge_service_matches("http://forgejo.local:3000", "forgejo.local:3001"));
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
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
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
            },
        )
        .await
        .expect("active convoy");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("convoy-two-crew-work"),
            &flotilla_resources::VesselSpec {
                convoy_ref: "convoy-two-crew".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "contained".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");

    let requested = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("convoy-two-crew".to_string()),
        vessel_ref: Some("convoy-two-crew-work".to_string()),
        role: Some("coder".to_string()),
    };
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("missing anchor");
    assert!(error.contains("no active session to anchor"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);

    let coder_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .vessel_index(0)
        .crew_index(0)
        .build();
    let coder_meta = terminal_meta_with_vessel_credentials(coder_identity.input_meta(), &requirement);
    let coder = backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &coder_meta,
            &ResourceTerminalSessionSpec {
                env_ref: "contained-env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
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
                env: Default::default(),
                pool: "contained".to_string(),
            },
        )
        .await
        .expect("eager coder terminal");

    let reviewer_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("reviewer".to_string())
        .vessel_index(0)
        .crew_index(1)
        .build();
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let failed_reviewer = sessions
        .create(&reviewer_identity.input_meta(), &ResourceTerminalSessionSpec { role: "reviewer".to_string(), ..coder.spec.clone() })
        .await
        .expect("failed reviewer target");
    let failed_reviewer = sessions
        .update_status(
            &failed_reviewer.metadata.name,
            &failed_reviewer.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Failed, ..Default::default() },
        )
        .await
        .expect("failed phase");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("failed target");
    assert!(error.contains("failed provisioning"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    sessions.delete(&failed_reviewer.metadata.name).await.expect("remove failed target");
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        session: coder.metadata.name.clone(),
        environment: "contained-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        invalidate_next: std::sync::atomic::AtomicBool::new(false),
        staged: std::sync::atomic::AtomicUsize::new(0),
    });
    daemon.set_work_credential_reconciler(probe.clone()).await;
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("staging failure");
    assert!(error.contains("credential staging failed"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert!(sessions.get("terminal-convoy-two-crew-work-reviewer").await.is_err());

    daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect("handoff to latent reviewer");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);

    let reviewer = backend
        .using::<ResourceTerminalSession>("flotilla")
        .get("terminal-convoy-two-crew-work-reviewer")
        .await
        .expect("latent reviewer terminal");
    let TerminalSessionSource::Agent { selector, brief, .. } = &reviewer.spec.source else {
        panic!("reviewer must be an agent session");
    };
    assert_eq!(selector.adapter.as_deref(), Some("claude-code"));
    assert!(brief.content.contains("- Minted credential repository scope:"));
    assert!(brief.content.contains("  - `github-crew-pr`:\n    - `github.com-flotilla-org-flotilla`"));
    assert_eq!(reviewer.spec.env_ref, "contained-env");
    assert_eq!(reviewer.metadata.annotations.get(CREDENTIAL_REFS_ANNOTATION), Some(&r#"["claude-max","github-crew-pr"]"#.to_string()));
    assert_eq!(
        reviewer.metadata.annotations.get(CREDENTIAL_SCOPES_ANNOTATION),
        Some(&r#"{"claude-max":["github.com-flotilla-org-flotilla"],"github-crew-pr":["github.com-flotilla-org-flotilla"]}"#.to_string())
    );
    assert_eq!(reviewer.metadata.annotations, coder_meta.annotations);

    let convoy = convoys.get("convoy-two-crew").await.expect("convoy");
    let mut terminal_status = convoy.status.expect("status");
    terminal_status.phase = flotilla_resources::ConvoyPhase::Landed;
    convoys.update_status("convoy-two-crew", &convoy.metadata.resource_version, &terminal_status).await.expect("landed convoy");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Late handoff").await.expect_err("terminal handoff");
    assert!(error.contains("terminal"), "{error}");
    assert_eq!(convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status"), terminal_status);
    let reviewer_after = sessions.get(&reviewer.metadata.name).await.expect("reviewer");
    assert_eq!(reviewer_after.metadata.resource_version, reviewer.metadata.resource_version);
}

pub(super) async fn create_identity_convoy(backend: &ResourceBackend, record: &str, role: &str, project: Option<&str>) {
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
    // Startup records the physical root even when observation configured an alias.
    assert_eq!(deltas[0].repo_identity, fallback_repo_identity(&canonical_or_original(&configured_inner)));
    assert!(matches!(
        deltas[0].changes.as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Added(terminal) }]
            if key == &terminal_id && terminal.attention == Some(flotilla_protocol::PaneExitAttention { exit_code: 7 })
    ));
}

pub(super) fn test_meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
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
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
        )
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
            .update_status(
                &created.metadata.name,
                &created.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
            )
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
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
        )
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "pre-identity-record").await, Ok("pre-identity-record".to_string()));
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

#[tokio::test]
async fn capability_admission_resolves_display_name_kind_and_policy_host_refs() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"canonical-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("udder"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("host id").to_string();
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(&host_id), &HostSpec { display_name: "udder".into(), connection: Default::default(), ..HostSpec::default() })
        .await
        .expect("host");
    hosts
        .update_status(
            &host_id,
            &host.metadata.resource_version,
            &HostStatus {
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([(
                    "udder-kind".into(),
                    FulfilmentFacts { gui_session_logged_in: true, observed_at: Utc::now(), ..Default::default() },
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("host facts");
    for name in ["collision-a", "collision-b"] {
        hosts
            .create(&test_meta(name), &HostSpec { display_name: "collision".into(), connection: Default::default(), ..HostSpec::default() })
            .await
            .expect("ambiguous host");
    }
    placement_policy(&backend, "a-collision", "collision").await;
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("a-collision"),
            &FulfilmentKindSpec::builder()
                .host_ref("collision".to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::gui_session()]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("ambiguous kind");
    placement_policy(&backend, "udder-kind", "udder").await;
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("udder-kind"),
            &FulfilmentKindSpec::builder()
                .host_ref("udder".to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::gui_session()]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("legacy kind");

    let (placement, _) = daemon
        .resolve_capability_placement(
            "flotilla",
            "project",
            &[],
            &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
            &BTreeSet::from([CapabilityNeed::GuiSession]),
            &flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build(),
        )
        .await
        .expect("display-name kind should admit");
    let allocation = placement.allocation.expect("allocation");
    assert_eq!(allocation.candidates[0].host, host_id);
    assert!(allocation.candidates[0].host_ready);
}

#[tokio::test]
async fn fulfilment_list_joins_host_facts_and_fleet_health_shows_local_kinds() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"fulfilment-join\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host ID").to_string();
    let kinds = backend.clone().using::<FulfilmentKind>("flotilla");
    for (name, host_ref) in [("local-kind", host_id.as_str()), ("unmatched-kind", "absent-host")] {
        kinds
            .create(
                &test_meta(name),
                &FulfilmentKindSpec::builder()
                    .host_ref(host_ref.to_string())
                    .pool("cleat".to_string())
                    .grants(BTreeSet::from([FulfilmentGrant::platform("linux".to_string())]))
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("create kind");
    }
    let before = daemon.fulfilment_list_internal().await.expect("list before heartbeat");
    assert_eq!(before.kinds.len(), 2);
    assert!(before.kinds.iter().all(|kind| kind.harnesses.is_empty() && kind.gui_session_logged_in.is_none()));

    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("create host");
    hosts
        .update_status(
            &host_id,
            &host.metadata.resource_version,
            &HostStatus {
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([(
                    "local-kind".to_string(),
                    FulfilmentFacts {
                        harnesses: BTreeMap::from([(
                            "claude-code".to_string(),
                            HarnessFacts { version: "2.1.282".to_string(), ..Default::default() },
                        )]),
                        gui_session_logged_in: true,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("publish facts");
    let listed = daemon.fulfilment_list_internal().await.expect("list facts");
    assert_eq!(listed.kinds.iter().find(|kind| kind.name == "local-kind").expect("local kind").harnesses["claude-code"].version, "2.1.282");
    assert!(listed.kinds.iter().find(|kind| kind.name == "unmatched-kind").expect("unmatched kind").harnesses.is_empty());
    let fleet = daemon.fleet_health_internal().await.expect("fleet health");
    let local = fleet.hosts.iter().find(|host| host.host == HostName::new("local-host")).expect("local host row");
    assert_eq!(local.fulfilments.len(), 1);
    assert_eq!(local.fulfilments[0].name, "local-kind");
    assert_eq!(local.fulfilments[0].gui_session_logged_in, Some(true));
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
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("stale self-origin host");
    backend
        .replica_writer::<ResourceHost>(daemon.node_id.clone(), "flotilla")
        .replace(&stale_source.using::<ResourceHost>("flotilla").list().await.expect("stale host list"), Utc::now())
        .await
        .expect("seed stale self-origin replica");

    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("authoritative local host");
    hosts
        .update_status(
            &host_id,
            &local.metadata.resource_version,
            &HostStatus {
                disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
                ..HostStatus::default()
            },
        )
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
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "self-targeted".to_string(),
                target_host: PlacementTargetHost {
                    reference: flotilla_protocol::CanonicalHostId::resolved(host_id),
                    display_name: "local-host".to_string(),
                },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
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
    let local = hosts
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("authoritative local host");
    hosts
        .update_status(
            &host_id,
            &local.metadata.resource_version,
            &HostStatus {
                disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
                ..HostStatus::default()
            },
        )
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
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "self-targeted".to_string(),
                target_host: target,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

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

async fn create_host_direct_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, agent_adapters: BTreeSet<String>) {
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

fn trusted_codex_workflow() -> WorkflowTemplateSpec {
    flotilla_resources::single_agent_workflow_spec()
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

pub(super) async fn create_test_environment(daemon: &InProcessDaemon, name: &str, host_ref: &str) -> String {
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

pub(super) async fn create_running_session(daemon: &InProcessDaemon, env_ref: &str, name: &str, convoy: &str, role: &str) {
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

async fn create_docker_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, held_credentials: BTreeSet<String>) {
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

#[tokio::test]
async fn docker_placement_refuses_hosts_missing_runtime_or_linux_before_selection() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "docker-kiwi", "kiwi", BTreeSet::new()).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    for (docker, os, missing) in [(false, "macos", "docker capability"), (true, "macos", "Linux host capability")] {
        let host = hosts.get("kiwi").await.expect("host");
        let mut status = host.status.expect("status");
        status.capabilities.insert("docker".to_string(), serde_json::json!(docker));
        status.capabilities.insert("os".to_string(), serde_json::json!(os));
        hosts.update_status("kiwi", &host.metadata.resource_version, &status).await.expect("update capability");

        for policy in [Some("docker-kiwi"), None] {
            let error = daemon
                .resolve_convoy_placement("flotilla", None, &[], &workflow, policy, false)
                .await
                .expect_err("ineligible host must refuse admission");
            assert!(error.contains("host `kiwi`"), "{error}");
            assert!(error.contains(missing), "{error}");
        }
    }
}

#[tokio::test]
async fn grant_resolution_scopes_roles_trust_and_permissions_independently_of_isolation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let own = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("own repository");
    let fork = RepositorySpec::remote("https://github.com/example/flotilla")
        .expect("fork repository")
        .with_upstream("https://github.com/flotilla-org/flotilla", flotilla_resources::RepositoryRelation::Fork)
        .expect("upstream");
    for repository in [&own, &fork] {
        backend
            .using::<Repository>("flotilla")
            .create(&test_meta(&repository.key().to_string()), repository)
            .await
            .expect("create repository");
    }
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("github-app"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp {
                    actor_login: None,
                    installation_id: Some(1),
                    installation_repository: None,
                    permissions: Some(BTreeMap::from([
                        ("contents".to_string(), "write".to_string()),
                        ("actions".to_string(), "read".to_string()),
                    ])),
                },
                source: CredentialSource::GithubApp { app_id_path: "app-id".to_string(), private_key_path: "key".to_string() },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("declaration");
    for (name, roles, trust, permissions) in [
        (
            "coder-contents",
            BTreeSet::from(["coder".to_string()]),
            Some(RepositoryTrust::Own),
            BTreeMap::from([("contents".to_string(), "write".to_string())]),
        ),
        (
            "coder-actions",
            BTreeSet::from(["coder".to_string()]),
            Some(RepositoryTrust::Own),
            BTreeMap::from([("actions".to_string(), "write".to_string())]),
        ),
        ("reviewer", BTreeSet::from(["reviewer".to_string()]), None, BTreeMap::from([("contents".to_string(), "read".to_string())])),
    ] {
        backend
            .definitions::<CredentialGrant>("flotilla")
            .create(
                &test_meta(name),
                &CredentialGrantSpec::builder()
                    .selector(
                        CredentialGrantSelector::builder()
                            .projects(BTreeSet::from(["flotilla".to_string()]))
                            .roles(roles)
                            .maybe_repository_trust(trust)
                            .build(),
                    )
                    .credentials(BTreeSet::from(["github-app".to_string()]))
                    .permissions(BTreeMap::from([("github-app".to_string(), permissions)]))
                    .build(),
            )
            .await
            .expect("grant");
    }
    let resolve = |role: &str, repository: &RepositorySpec| {
        let role = role.to_string();
        let repository = repository.clone();
        let backend = backend.clone();
        async move {
            let mut workflow = WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder().role(role).source(CrewSource::Tool { command: "true".to_string() }).build()])
                    .build()])
                .build();
            let repositories = [ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repository.key())
                .source_ref("main".to_string())
                .target_ref("work".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()];
            resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &repositories, &mut workflow)
                .await
                .expect("resolve grants");
            workflow.vessels.remove(0)
        }
    };
    let contained = resolve("coder", &own).await;
    let direct = resolve("coder", &own).await;
    assert_eq!(contained.credential_refs, direct.credential_refs);
    assert_eq!(contained.credential_permissions, direct.credential_permissions);
    assert_eq!(
        contained.credential_permissions["github-app"],
        BTreeMap::from([("contents".to_string(), "write".to_string()), ("actions".to_string(), "read".to_string()),])
    );
    let reviewer = resolve("reviewer", &own).await;
    assert_eq!(reviewer.credential_permissions["github-app"], BTreeMap::from([("contents".to_string(), "read".to_string())]));
    let fork_coder = resolve("coder", &fork).await;
    assert!(fork_coder.credential_refs.is_empty());
}

// Owner ruling #2491: admission keeps the maximum for unlisted-only grants,
// unions and caps explicit grants, and refuses mixed modes with named evidence.
#[tokio::test]
async fn admission_refuses_mixed_grant_permissions_and_preserves_homogeneous_modes() {
    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Unlisted,
        UnlistedNoCap,
        Explicit,
        Mixed,
        EmptyExplicit,
    }
    for mode in [Mode::Unlisted, Mode::UnlistedNoCap, Mode::Explicit, Mode::Mixed, Mode::EmptyExplicit] {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &test_meta("app"),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(1),
                        installation_repository: None,
                        permissions: match mode {
                            Mode::UnlistedNoCap => None,
                            _ => Some(BTreeMap::from([("contents".into(), "write".into()), ("actions".into(), "read".into())])),
                        },
                    },
                    source: CredentialSource::GithubApp { app_id_path: "app-id".into(), private_key_path: "key".into() },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("spec");
        for name in ["base", "elevation"] {
            let listed = match mode {
                Mode::Unlisted | Mode::UnlistedNoCap => false,
                Mode::Explicit | Mode::EmptyExplicit => true,
                Mode::Mixed => name == "elevation",
            };
            let permissions = if matches!(mode, Mode::EmptyExplicit) {
                BTreeMap::new()
            } else if name == "base" {
                BTreeMap::from([("contents".into(), "read".into())])
            } else {
                BTreeMap::from([("actions".into(), "write".into()), ("workflows".into(), "write".into())])
            };
            backend
                .definitions::<CredentialGrant>("flotilla")
                .create(
                    &test_meta(name),
                    &CredentialGrantSpec::builder()
                        .selector(CredentialGrantSelector::builder().build())
                        .credentials(BTreeSet::from(["app".into()]))
                        .permissions(if listed { BTreeMap::from([("app".into(), permissions)]) } else { BTreeMap::new() })
                        .build(),
                )
                .await
                .expect("grant");
        }
        let mut workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder().name("work".into()).crew(Vec::new()).build()])
            .build();
        let result = resolve_workflow_credentials(&backend, "flotilla", None, &[], &mut workflow).await;
        if matches!(mode, Mode::Mixed) {
            assert_eq!(
                result.expect_err("mixed grant admission refused"),
                concat!(
                    "vessel `work`: credential `app` mixes permissions listed by grant `elevation` ",
                    "with unlisted permissions in grant `base`; make grant `base` explicit for credential `app`"
                )
            );
        } else {
            result.expect("homogeneous grants admitted");
            let expected = match mode {
                Mode::Unlisted => Some(BTreeMap::from([("contents".into(), "write".into()), ("actions".into(), "read".into())])),
                Mode::Explicit => Some(BTreeMap::from([("contents".into(), "read".into()), ("actions".into(), "read".into())])),
                Mode::UnlistedNoCap => None,
                Mode::EmptyExplicit => Some(BTreeMap::new()),
                Mode::Mixed => unreachable!("the mixed case asserts refusal above"),
            };
            assert_eq!(workflow.vessels[0].credential_permissions.get("app"), expected.as_ref(), "{mode:?}");
        }
    }
}

#[tokio::test]
async fn contained_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
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

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("contained Claude must not reach interactive login without OAuth");
    assert_eq!(error, "agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-contained"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
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
async fn docker_placement_selects_credentials_for_the_effective_contained_stance() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("github-crew-pr"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create GitHub credential declaration");
    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("github-contained"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["github-crew-pr".to_string()]))
                .build(),
        )
        .await
        .expect("create contained GitHub grant");
    create_docker_placement(&backend, "docker-crew", "host-a", BTreeSet::from(["github-crew-pr".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-crew").await.expect("get Docker placement");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut workflow)
        .await
        .expect("resolve credentials against effective stance");

    assert_eq!(workflow.vessels[0].credential_refs, BTreeSet::from(["github-crew-pr".to_string()]));
    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("contained grant held by the placement admits dispatch");
}

#[tokio::test]
async fn project_grant_entitlement_is_independent_of_vessel_stance() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("github-crew-pr"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create GitHub credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("github-project"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["github-crew-pr".to_string()]))
                .build(),
        )
        .await
        .expect("create project grant");

    let mut host_direct = workflow.clone();

    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut host_direct).await.expect("resolve host-direct grant");

    let mut contained = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut contained).await.expect("resolve contained grant");
    assert_eq!(host_direct.vessels[0].credential_refs, BTreeSet::from(["github-crew-pr".to_string()]));
    assert_eq!(host_direct.vessels[0].credential_refs, contained.vessels[0].credential_refs);
}

#[tokio::test]
async fn remote_placement_uses_replicated_host_capabilities() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("kiwi-root"));
    let now = Utc::now();
    let feta = ResourceBackend::InMemory(InMemoryBackend::default());
    let feta_hosts = feta.using::<ResourceHost>("flotilla");
    let fresh = feta_hosts
        .create(
            &test_meta("feta-host"),
            &HostSpec { display_name: "feta".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("create fresh feta self-report");
    feta_hosts
        .update_status(
            &fresh.metadata.name,
            &fresh.metadata.resource_version,
            &HostStatus {
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
            },
        )
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
                    memory_policy: Default::default(),
                    host_ref: "feta-host".to_string(),
                    image: "crew:latest".to_string().into(),
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
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
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
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
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

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("trusted Claude must not reach ambient login without delivered OAuth");
    assert_eq!(error, "agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-trusted"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
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
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent { selector: Selector::for_capability("ambient-only"), prompt: None, brief_template: None })
                .build()])
            .build()])
        .build();
    create_host_direct_placement(&backend, "ambient-host", "host-a", BTreeSet::new()).await;
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
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Claude credential declaration");
    let mut workflow = WorkflowTemplateSpec::builder()
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

#[tokio::test]
async fn image_baseline_admission_fails_without_agents_and_pins_resolved_image() {
    use flotilla_resources::{CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource};

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "crew-policy", "host-a", BTreeSet::new()).await;
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let mut policy = policies.get("crew-policy").await.expect("policy");
    policy.spec.docker_per_vessel.as_mut().expect("docker").image =
        DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".to_string() };
    policies.update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec).await.expect("reference baseline");
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let error = daemon
        .resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false)
        .await
        .expect_err("missing baseline must fail");
    assert!(error.contains("image-baseline `fleet-crew` missing/unresolved"), "{error}");
    let error = daemon
        .resolve_convoy_placement("flotilla", None, &[], &workflow, None, false)
        .await
        .expect_err("default selection must reject missing baseline");
    assert!(error.contains("image-baseline `fleet-crew` missing/unresolved"), "{error}");
    let baselines = backend.definitions::<CrewImageBaseline>("flotilla");
    baselines
        .apply(&test_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:v1".to_string(), layers: None })
        .await
        .expect("baseline");
    let admitted = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("admit");
    baselines.apply(&test_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:v2".to_string(), layers: None }).await.expect("bump");
    assert_eq!(admitted.selected.expect("placement").spec.docker_per_vessel.expect("docker").image, DockerImageSource::from("crew:v1"));
    let next = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("next admission");
    assert_eq!(next.selected.expect("placement").spec.docker_per_vessel.expect("docker").image, DockerImageSource::from("crew:v2"));
}

#[tokio::test]
async fn checkout_vcs_discovery_is_cached_per_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"checkout-vcs-cache-test\"\n").expect("daemon config");
    let first_path = temp.path().join("first");
    let second_path = temp.path().join("second");
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![
        Box::new(FakeVcsFactory::new(FakeVcsState::builder(&first_path).build())),
        Box::new(FakeVcsFactory::new(FakeVcsState::builder(&second_path).build())),
    ];
    let daemon =
        InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("local-host")).await;

    // #1770: only observed Checkout lifetimes retain discovered providers.
    for (name, path) in [("first", &first_path), ("second", &second_path)] {
        let repository = RepositorySpec::remote(format!("https://github.com/example/{name}")).expect("repository");
        daemon
            .resource_backend
            .using::<Repository>("flotilla")
            .create(&test_meta(&repository.key().to_string()), &repository)
            .await
            .expect("repository");
        daemon
            .observed_resource_backend
            .clone()
            .using::<ResourceCheckout>("flotilla")
            .create(
                &test_meta(name),
                &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                    r#ref: "main".into(),
                    path: path.to_string_lossy().into(),
                    repo_ref: repository.key(),
                    host_ref: daemon.environment_manager.local_host_id().to_string(),
                    is_main: true,
                }),
            )
            .await
            .expect("observed checkout");
    }

    let first = daemon.local_vcs_for_checkout(&first_path).await.expect("first checkout VCS");
    let first_again = daemon.local_vcs_for_checkout(&first_path).await.expect("cached first checkout VCS");
    let second = daemon.local_vcs_for_checkout(&second_path).await.expect("second checkout VCS");

    assert!(Arc::ptr_eq(&first, &first_again));
    assert!(!Arc::ptr_eq(&first, &second));
}

async fn stall_test_daemon() -> (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir, tokio::task::JoinHandle<()>) {
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

#[tokio::test]
async fn crew_fail_requires_operator_force() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let error = daemon
        .crew_fail_internal(&CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() }, "blocked".into(), false, None)
        .await
        .expect_err("crew failure must be refused");
    assert!(error.contains("crew stall"), "{error}");
    let principal = flotilla_protocol::PrincipalRef::implicit_for_namespace("flotilla");
    let error = daemon
        .crew_fail_internal(
            &CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            "blocked".into(),
            true,
            Some(&principal),
        )
        .await
        .expect_err("crew identity cannot force failure");
    assert!(error.contains("crew stall"), "{error}");

    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy =
        convoys.create(&test_meta("operator-failure"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("crew status");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("operator-failure-vessel"),
            &VesselSpec {
                convoy_ref: "operator-failure".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    daemon
        .crew_fail_internal(
            &CrewCommandContext {
                namespace: Some("flotilla".into()),
                convoy: Some("operator-failure".into()),
                vessel_ref: Some("operator-failure-vessel".into()),
                role: Some("coder".into()),
                ..Default::default()
            },
            "supervisor ruling".into(),
            true,
            Some(&principal),
        )
        .await
        .expect("operator force failure");
    let status = convoys.get("operator-failure").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed);
    watch.abort();
}

async fn wait_for_stall(backend: &ResourceBackend, name: &str, expected: bool) -> ConvoyStatus {
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

fn stall_workflow_snapshot(crew: Vec<flotilla_resources::CrewSpec>) -> flotilla_resources::WorkflowSnapshot {
    flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![flotilla_resources::VesselRequirement::builder().name("work".into()).crew(crew).build()],
    }
}

fn claim_crew(role: &str) -> flotilla_resources::CrewSpec {
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

async fn stall_test_session(backend: &ResourceBackend, convoy: &str, name: &str, role: &str, attention: TerminalAttention) {
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

#[tokio::test]
async fn active_idle_crew_stalls_and_working_crew_clears() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("idle-crew"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "idle-crew",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some({
                    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
                    snapshot.stall_nudges.insert(
                        "work/coder".into(),
                        flotilla_resources::StallNudgePolicy { max_per_episode: 2, max_refusals: None, idle_grace_seconds: Some(3) },
                    );
                    snapshot
                }),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("idle-session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "idle-crew".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Tool { command: "test".into() },
                cwd: "/tmp".into(),
                env: Default::default(),
                pool: "test".into(),
            },
        )
        .await
        .expect("session");
    let mut idle = sessions
        .update_status(
            "idle-session",
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: Utc::now() - chrono::Duration::seconds(3),
                    source: TerminalAttentionSource::Screen,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("idle attention");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(convoys.get("idle-crew").await.expect("convoy").status.expect("status").stalled.is_none());
    let mut refreshed = idle.status.clone().expect("session status");
    refreshed.attention.as_mut().expect("attention").as_of = Utc::now();
    idle = sessions.update_status("idle-session", &idle.metadata.resource_version, &refreshed).await.expect("refreshed idle attention");
    let status = wait_for_stall(&backend, "idle-crew", true).await;
    let stalled = status.stalled.expect("stalled");
    assert!(matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { ref role, .. }) if role == "coder"));
    // The projectless fixture keeps its idle evidence and explains why the
    // default ProjectCrew rung cannot supply a supervisor (#2522).
    assert_eq!(stalled.evidence, "idle; cannot find governor: convoy has no project_ref");
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Screen);
    let mut events = daemon.subscribe();
    let subscription = daemon
        .crew_ops
        .subscribe_wait(
            uuid::Uuid::new_v4(),
            flotilla_protocol::WaitSubscriptionRequest {
                namespace: "flotilla".into(),
                leaves: vec!["convoy/idle-crew .status.stalled == true".parse().expect("stall leaf")],
                freshness_demand: None,
            },
        )
        .await
        .expect("subscribe to stalled condition");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let DaemonEvent::LeafFired(fire) = events.recv().await.expect("leaf event") {
                if fire.subscription_id == subscription {
                    break;
                }
            }
        }
    })
    .await
    .expect("stalled leaf fires");
    let mut working = idle.status.expect("session status");
    working.attention =
        Some(TerminalAttention { state: TerminalAttentionState::Working, as_of: Utc::now(), source: TerminalAttentionSource::Screen });
    sessions.update_status("idle-session", &idle.metadata.resource_version, &working).await.expect("working attention");
    assert!(wait_for_stall(&backend, "idle-crew", false).await.stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn stale_working_attention_does_not_stall_crew() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("stale-crew"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "stale-crew",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(stall_workflow_snapshot(vec![claim_crew("coder")])),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    stall_test_session(
        &backend,
        "stale-crew",
        "stale-session",
        "coder",
        TerminalAttention {
            state: TerminalAttentionState::Working,
            as_of: Utc::now() - chrono::Duration::minutes(3),
            source: TerminalAttentionSource::Hook,
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get("stale-crew").await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn landing_done_crew_with_idle_session_has_no_actor_stall() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let repository = flotilla_resources::RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
        .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".into())
            .repo_ref(repository.clone())
            .source_ref("feature/landing".into())
            .target_ref("main".into())
            .workspace_slug("flotilla".into())
            .subpaths(Vec::new())
            .build()])
        .change_request(
            flotilla_resources::BoundChangeRequest::builder().id("1392".into()).repository_ref(repository).title("test".into()).build(),
        )
        .build();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.create(&test_meta("landed-claim"), &spec).await.expect("convoy");
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::Table(indexmap::IndexMap::from([(
        "shipped".into(),
        "$cr.state == merged".parse().expect("leaf template"),
    )])));
    convoys
        .update_status(
            "landed-claim",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(snapshot),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if daemon
                .crew_ops
                .subscription_rows()
                .await
                .iter()
                .any(|row| matches!(&row.watcher, crate::leaf_engine::LeafWatcher::ReconcilerWake { convoy } if convoy == "landed-claim"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("exit row armed");
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1392);
    let record = records.get(&record_name).await.expect("demanded observation record");
    let fresh = Utc::now();
    records
        .update_status(
            &record_name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, fresh),
                head_sha: flotilla_resources::Observation::known("head".into(), fresh),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, fresh),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, fresh),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, fresh),
            },
        )
        .await
        .expect("fresh observation");
    stall_test_session(
        &backend,
        "landed-claim",
        "idle-landing-session",
        "coder",
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: Utc::now(), source: TerminalAttentionSource::Hook },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get("landed-claim").await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn landing_without_armed_exit_rows_stalls() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("unarmed-landing"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "unarmed-landing",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    let stalled = wait_for_stall(&backend, "unarmed-landing", true).await.stalled.expect("stalled");
    assert_eq!(stalled.maker, None);
    assert_eq!(stalled.evidence, "no armed row with an able maker");
    watch.abort();
}

#[tokio::test]
async fn stale_change_request_observation_stalls_landing_convoy() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let repository = flotilla_resources::RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
        .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".into())
            .repo_ref(repository.clone())
            .source_ref("feature/stall".into())
            .target_ref("main".into())
            .workspace_slug("flotilla".into())
            .subpaths(Vec::new())
            .build()])
        .change_request(
            flotilla_resources::BoundChangeRequest::builder().id("1391".into()).repository_ref(repository).title("test".into()).build(),
        )
        .build();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.create(&test_meta("stale-observation"), &spec).await.expect("convoy");
    convoys
        .update_status(
            "stale-observation",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(flotilla_resources::ExitDeclaration::Table(indexmap::IndexMap::from([(
                        "shipped".into(),
                        "$cr.state == merged".parse().expect("leaf template"),
                    )]))),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1391);
    let record = records
        .create(
            &test_meta(&record_name),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(1391)
                .observing_authority("test".into())
                .build(),
        )
        .await
        .expect("observation record");
    let old = Utc::now() - chrono::Duration::hours(1);
    records
        .update_status(
            &record_name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, old),
                head_sha: flotilla_resources::Observation::known("old".into(), old),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, old),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, old),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, old),
            },
        )
        .await
        .expect("stale observation");
    let stalled = wait_for_stall(&backend, "stale-observation", true).await.stalled.expect("stalled");
    assert!(matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Observed { .. })));
    assert!(stalled.evidence.starts_with("stale"));
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Observation);
    watch.abort();
}

#[tokio::test]
async fn idle_standing_role_without_obligation_never_stalls_convoy() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.create(&test_meta("standing"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "standing",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(stall_workflow_snapshot(vec![flotilla_resources::CrewSpec::builder()
                    .role("governor".into())
                    .source(flotilla_resources::CrewSource::Tool { command: "test".into() })
                    .build()])),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("governor".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    for (name, role, state) in [("governor-session", "governor", TerminalAttentionState::Idle)] {
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name(name.into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "standing".into()),
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
                    attention: Some(TerminalAttention {
                        state,
                        as_of: Utc::now() - chrono::Duration::seconds(10),
                        source: TerminalAttentionSource::Screen,
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("attention");
    }
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get("standing").await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn replica_wake_engine_does_not_write_stalled_condition() {
    let authority = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("authority"));
    let convoys = authority.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("replicated"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "replicated",
            &created.metadata.resource_version,
            &ConvoyStatus { phase: flotilla_resources::ConvoyPhase::Landing, ..Default::default() },
        )
        .await
        .expect("landing status");
    let replica = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("replica"));
    replica
        .replica_writer::<ResourceConvoy>(NodeId::new("authority"), "flotilla")
        .replace(&convoys.list().await.expect("authority list"), Utc::now())
        .await
        .expect("replicate");
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"replica-stall-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("replica"),
        replica.clone(),
    )
    .await;
    let (sender, _receiver) = flotilla_resources::controller::WorkQueueSender::channel();
    let watch = daemon.reconciler_wake_watch();
    let replica_check = replica.clone();
    let task = tokio::spawn(async move { watch.spawn(replica.clone(), "flotilla".into(), sender).await.expect("replica watch") });
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(!task.is_finished(), "replica watch must remain healthy");
    assert!(replica_check.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
    assert!(replica_check
        .including_replicas::<ResourceConvoy>("flotilla")
        .get("replicated")
        .await
        .expect("replicated convoy")
        .object
        .status
        .expect("replica status")
        .stalled
        .is_none());
    assert!(convoys.get("replicated").await.expect("authority convoy").status.expect("status").stalled.is_none());
    task.abort();
}

// Process exit leaves unfinished work resumable in its existing checkout. An
// operator follow-up relaunches a stopped session instead of waiting for a hook
// that an exited agent cannot send; both observed and not-yet-observed exits work.
#[tokio::test]
async fn resume_relaunches_exited_active_and_interrupted_crew() {
    for phase in [CrewWorkPhase::Working, CrewWorkPhase::Interrupted] {
        let (daemon, backend, probe) = resume_staging_fixture().await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = ConvoyPhase::Active;
        status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Interrupted;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = phase;
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("unfinished work");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("resume-staging-session").await.expect("session");
        let original_cwd = session.spec.cwd.clone();
        sessions
            .update_status(
                &session.metadata.name,
                &session.metadata.resource_version,
                &ResourceTerminalSessionStatus {
                    phase: ResourceTerminalSessionPhase::Stopped,
                    inner_command_status: Some(flotilla_resources::InnerCommandStatus::Exited),
                    ..Default::default()
                },
            )
            .await
            .expect("agent exited");
        daemon
            .convoy_resume_internal("flotilla", "resume-staging", "Recover the unfinished review", Some("work"), Some("coder"))
            .await
            .expect("resume exited agent");
        let session = sessions.get("resume-staging-session").await.expect("same session");
        assert_eq!(session.spec.cwd, original_cwd);
        assert_eq!(session.status.expect("session status").phase, ResourceTerminalSessionPhase::Starting);
        assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
        assert!(backend
            .using::<flotilla_resources::Message>("flotilla")
            .list()
            .await
            .expect("inbox")
            .items
            .iter()
            .any(|message| message.spec.body.contains("Recover the unfinished review")));
        let status = convoys.get("resume-staging").await.expect("convoy").status.expect("status");
        assert!(status.pending_brief().is_none(), "an exited process cannot consume a pending brief");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    }
}

// UTC-to-monotonic conversion must preserve a future header deadline and keep
// expired/zero deadlines from causing a burst of concurrent cache misses.
#[tokio::test(start_paused = true)]
async fn observation_cache_deadline_crosses_both_clocks() {
    let wall = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:00:00Z").expect("fixed UTC timestamp").with_timezone(&chrono::Utc);
    let retry = wall + chrono::Duration::seconds(60);
    let expires = tokio::time::Instant::now() + super::observation_cache_delay(Some(retry), wall);
    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    assert!(tokio::time::Instant::now() < expires);
    assert_eq!(super::observation_cache_delay(Some(retry), wall + chrono::Duration::seconds(59)), std::time::Duration::from_secs(1));
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(tokio::time::Instant::now(), expires);
    for now in [retry, retry + chrono::Duration::seconds(1)] {
        assert_eq!(super::observation_cache_delay(Some(retry), now), std::time::Duration::from_secs(9));
    }
}

// #1496 operator follow-up: host provider observations and CLI summaries are
// independent of tracked roots; the published status remains the query authority.
#[hegel::test]
fn host_provider_summary_survives_root_membership_changes(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use crate::providers::discovery::test_support::FakePresentationManager;

    // Cover no roots, multiple roots, both discovery outcomes, repeated removals,
    // and either root removal order. Factories stand in for process providers.
    let root_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let available = tc.draw(gs::booleans());
    let reverse = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("config directory");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"host-provider-observer\"\n").expect("daemon config");
        let mut roots = (0..root_count).map(|index| temp.path().join(format!("repo-{index}"))).collect::<Vec<_>>();
        for root in &roots {
            std::fs::create_dir(root).expect("root directory");
        }
        let mut providers = FakeDiscoveryProviders::new().with_change_request(Arc::new(FakeChangeRequest::new()));
        if available {
            providers = providers.with_presentation_manager(Arc::new(FakePresentationManager::new()));
        }
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            roots.clone(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery_with_provider_set(providers),
            HostName::new("observer"),
            backend.clone(),
        )
        .await;
        let environment = daemon.local_host_identity().environment_id;
        let expected = if available {
            vec![HostProviderStatus {
                category: "workspace_manager".into(),
                name: "Fake Workspaces".into(),
                implementation: "fake-workspaces".into(),
                healthy: true,
                disabled_reason: None,
            }]
        } else {
            vec![]
        };
        let mut description = daemon.local_host_description().await;
        assert_eq!(description.providers, expected, "host discovery cannot depend on a tracked repository");
        assert_eq!(daemon.get_host_providers_internal(&environment).await.expect("bootstrap providers").summary.providers, expected,);

        // A stored health observation can differ from this process's discovery;
        // queries must present that status even while roots are removed.
        let published = vec![HostProviderStatus::disabled("workspace_manager", "published", "offline probe")];
        description.providers = published.clone();
        let hosts = backend.using::<ResourceHost>("flotilla");
        let name = daemon.local_host_id().expect("host id").to_string();
        let host = hosts.create(&test_meta(&name), &HostSpec::default()).await.expect("Host");
        hosts
            .update_status(&name, &host.metadata.resource_version, &HostStatus { description: Some(description), ..Default::default() })
            .await
            .expect("published description");
        if reverse {
            roots.reverse();
        }
        for root in std::iter::once(None).chain(roots.iter().flat_map(|root| [Some(root), Some(root)])) {
            if let Some(root) = root {
                daemon.remove_repo(root).await.expect("remove root or repeat removal");
            }
            assert_eq!(daemon.local_host_description().await.providers, expected, "heartbeat discovery survives root removal");
            assert_eq!(
                daemon.get_host_providers_internal(&environment).await.expect("resource providers").summary.providers,
                published,
                "provider CLI reads stored health, independent of tracked roots",
            );
        }
    });
}

// #1496: resource descriptions survive link changes and observer restart; link
// state alone determines connectivity. Repeated projection must not bump cursors.
#[hegel::test]
fn resource_host_descriptions_survive_transport_changes(tc: hegel::TestCase) {
    use flotilla_protocol::qualified_path::HostId;
    use hegel::generators as gs;
    // Short generated sequences cover duplicate updates, online/offline transitions,
    // empty inventories, and description changes while the peer is disconnected.
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let operations = (0..steps).map(|_| (tc.draw(gs::booleans()), tc.draw(gs::booleans()))).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("config directory");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"host-description-observer\"\n").expect("daemon config");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let make_daemon = || {
            InProcessDaemon::new_with_resource_backend(
                Vec::new(),
                Arc::new(ConfigStore::with_base(temp.path())),
                fake_discovery(false),
                HostName::new("observer"),
                backend.clone(),
            )
        };
        let daemon = make_daemon().await;
        // A published local description is also authoritative: the host environment
        // identity differs from the daemon's direct execution environment id.
        let local_identity = daemon.local_host_identity();
        let local_environment = local_identity.environment_id.clone();
        let local_summary = HostSummary::builder()
            .environment_id(local_environment.clone())
            .node(local_identity.node)
            .system(flotilla_protocol::SystemInfo { os: Some("published-os".into()), ..Default::default() })
            .build();
        let local_hosts = backend.clone().using::<ResourceHost>("flotilla");
        let local_name = daemon.local_host_id().expect("local host").to_string();
        let local = local_hosts.create(&test_meta(&local_name), &HostSpec::default()).await.expect("local Host");
        local_hosts
            .update_status(
                &local_name,
                &local.metadata.resource_version,
                &HostStatus { description: Some(local_summary.clone()), heartbeat_at: Some(Utc::now()), ..Default::default() },
            )
            .await
            .expect("publish local description");
        let local = daemon.get_host_status_internal(&local_environment).await.expect("local resource description");
        assert_eq!(local.summary, Some(local_summary));
        let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("remote-root"));
        let hosts = remote.using::<ResourceHost>("flotilla");
        hosts.create(&test_meta("remote-host"), &HostSpec::default()).await.expect("host");
        let writer = backend.replica_writer::<ResourceHost>(NodeId::new("remote-root"), "flotilla");
        let environment_id = EnvironmentId::host(HostId::new("remote-host"));
        let node = NodeInfo::new(NodeId::new("remote-node"), "remote");
        let mut expected = None;
        for (step, (connected, with_inventory)) in operations.into_iter().enumerate() {
            let summary = HostSummary::builder()
                .environment_id(environment_id.clone())
                .host_name(HostName::new("remote"))
                .node(node.clone())
                .system(flotilla_protocol::SystemInfo { cpu_count: Some(step as u16), ..Default::default() })
                .inventory(flotilla_protocol::ToolInventory {
                    binaries: if with_inventory {
                        vec![flotilla_protocol::DiscoveryFact { name: "git".into(), detail: vec![] }]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                })
                .providers(vec![HostProviderStatus::disabled("vcs", "git", "probe failed")])
                .environments(vec![flotilla_protocol::EnvironmentInfo::Direct {
                    id: environment_id.clone(),
                    host_id: Some(HostId::new("remote-host")),
                    display_name: None,
                    status: flotilla_protocol::EnvironmentStatus::Running,
                }])
                .build();
            let host = hosts.get("remote-host").await.expect("host");
            let status = flotilla_resources::HostStatus {
                description: Some(summary),
                heartbeat_at: Some(Utc::now()),
                blob_sync: Some(flotilla_protocol::BlobSyncStatus { pending_count: step, last_error: None }),
                capabilities: BTreeMap::from([(AGENT_ADAPTERS_CAPABILITY.into(), serde_json::json!(["codex"]))]),
                ..Default::default()
            };
            let expected_environments = status.description.as_ref().expect("description").environments.clone();
            expected = status.host_summary();
            hosts.update_status("remote-host", &host.metadata.resource_version, &status).await.expect("publish description");
            writer.replace(&hosts.list().await.expect("list"), Utc::now()).await.expect("replicate");
            let identity = flotilla_protocol::HostIdentity {
                environment_id: environment_id.clone(),
                host_name: Some(HostName::new("live-name")),
                node: NodeInfo::new(node.node_id.clone(), "live-name"),
            };
            daemon.set_peer_host_identities(HashMap::from([(environment_id.clone(), identity.clone())])).await;
            let connectivity = if connected { PeerConnectionState::Connected } else { PeerConnectionState::Disconnected };
            daemon.publish_peer_connection_status(&node, connectivity.clone()).await;
            if !connected {
                daemon.set_peer_host_identities(HashMap::new()).await;
            }
            let mut presented = expected.clone().expect("description");
            if connected {
                presented.node = identity.node;
                presented.host_name = identity.host_name;
            }
            let response = daemon.get_host_status_internal(&environment_id).await.expect("resource backed host");
            assert_eq!(response.summary, Some(presented.clone()), "live transport identity wins over resource identity");
            assert_eq!(response.connection_status, connectivity);
            assert_eq!(response.blob_sync, status.blob_sync);
            let providers = daemon.get_host_providers_internal(&environment_id).await.expect("providers");
            assert_eq!(providers.summary, presented);
            assert_eq!(providers.visible_environments, expected_environments);
            assert_eq!(response.visible_environments, expected_environments);
            let replay = daemon.replay_since(&HashMap::new()).await.expect("replay");
            let seq = replay
                .iter()
                .find_map(|event| match event {
                    DaemonEvent::HostSnapshot(snapshot) if snapshot.environment_id == environment_id => Some(snapshot.seq),
                    _ => None,
                })
                .expect("snapshot");
            let replay = daemon
                .replay_since(&HashMap::from([(StreamKey::Host { environment_id: environment_id.clone() }, seq)]))
                .await
                .expect("duplicate projection");
            assert!(!replay
                .iter()
                .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.environment_id == environment_id)));
        }
        drop(daemon);
        let restarted = make_daemon().await;
        let response = restarted.get_host_status_internal(&environment_id).await.expect("offline replica after restart");
        assert_eq!(response.summary, expected);
        assert_eq!(response.connection_status, PeerConnectionState::Disconnected);
        let listed = restarted.list_hosts_internal().await.expect("hosts");
        assert!(listed.hosts.iter().any(|host| host.environment_id.as_ref() == Some(&environment_id) && host.has_summary));
        // Deleting one resource must rehome the node's environment mapping to
        // another described environment, rather than leaving a removed mapping.
        let original = hosts.get("remote-host").await.expect("remote host").status.expect("status");
        let alternate_environment = EnvironmentId::host(HostId::new("remote-alternate"));
        let alternate = hosts.create(&test_meta("remote-alternate"), &HostSpec::default()).await.expect("alternate Host");
        let mut alternate_status = original.clone();
        alternate_status.description.as_mut().expect("description").environment_id = alternate_environment.clone();
        hosts.update_status("remote-alternate", &alternate.metadata.resource_version, &alternate_status).await.expect("alternate status");
        writer.replace(&hosts.list().await.expect("two hosts"), Utc::now()).await.expect("replicate alternate");
        restarted.refresh_resource_host_summaries().await.expect("project alternate");
        hosts.delete("remote-host").await.expect("delete host");
        writer.replace(&hosts.list().await.expect("remaining host"), Utc::now()).await.expect("replicate deletion");
        assert!(restarted.get_host_status_internal(&environment_id).await.is_err());
        assert_eq!(restarted.host_registry.environment_id_for_node(&node.node_id).await, Some(alternate_environment.clone()));
        hosts.delete("remote-alternate").await.expect("delete alternate");
        writer.replace(&hosts.list().await.expect("empty list"), Utc::now()).await.expect("replicate final deletion");
        restarted.refresh_resource_host_summaries().await.expect("project deletion");
        assert_eq!(restarted.host_registry.environment_id_for_node(&node.node_id).await, None);

        // A live identity keeps its identity-only presentation when a Host loses
        // its description or is deleted. No description is invented from link state.
        let created = hosts.create(&test_meta("remote-host"), &HostSpec::default()).await.expect("recreate Host");
        hosts.update_status("remote-host", &created.metadata.resource_version, &original).await.expect("restore description");
        writer.replace(&hosts.list().await.expect("restored list"), Utc::now()).await.expect("replicate restored Host");
        let identity = flotilla_protocol::HostIdentity {
            environment_id: environment_id.clone(),
            host_name: Some(HostName::new("live-name")),
            node: NodeInfo::new(node.node_id.clone(), "live-name"),
        };
        restarted.set_peer_host_identities(HashMap::from([(environment_id.clone(), identity.clone())])).await;
        restarted.publish_peer_connection_status(&node, PeerConnectionState::Connected).await;
        let current = hosts.get("remote-host").await.expect("current Host");
        let empty_status =
            HostStatus { blob_sync: Some(flotilla_protocol::BlobSyncStatus { pending_count: 7, last_error: None }), ..Default::default() };
        hosts.update_status("remote-host", &current.metadata.resource_version, &empty_status).await.expect("clear description");
        writer.replace(&hosts.list().await.expect("empty status"), Utc::now()).await.expect("replicate empty status");
        for deleted in [false, true] {
            if deleted {
                hosts.delete("remote-host").await.expect("delete connected Host");
                writer.replace(&hosts.list().await.expect("empty list"), Utc::now()).await.expect("replicate deletion");
            }
            let response = restarted.get_host_status_internal(&environment_id).await.expect("live identity survives missing description");
            assert_eq!(response.summary, Some(identity.clone().into()));
            assert_eq!(response.connection_status, PeerConnectionState::Connected);
            assert!(response.visible_environments.is_empty());
            assert_eq!(response.blob_sync, if deleted { None } else { empty_status.blob_sync.clone() });
        }
        restarted.set_peer_host_identities(HashMap::new()).await;
        assert!(restarted.get_host_status_internal(&environment_id).await.is_err());
    });
}

// Behaviour (#2255): context-free command lifecycle and intervening publications
// reach all subscribers in call order, with their original payloads intact.
// Glue: one call-through sequence covers these synchronous composition-root methods.
#[tokio::test]
async fn event_publication_preserves_context_free_command_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"event-publication-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let mut first = daemon.subscribe();
    let mut second = daemon.subscribe();
    let identity = daemon.start_context_free_command(42, "test publication".into());
    daemon.send_event(DaemonEvent::RepoUntracked { repo_identity: identity.clone(), path: None });
    daemon.finish_context_free_command(42, identity.clone(), CommandValue::Error { message: "expected error".into() });
    for receiver in [&mut first, &mut second] {
        assert!(matches!(receiver.try_recv().expect("started"), DaemonEvent::CommandStarted {
            command_id: 42, repo_identity, repo: None, description, ..
        } if repo_identity == identity && description == "test publication"));
        assert!(matches!(receiver.try_recv().expect("intervening event"), DaemonEvent::RepoUntracked {
            repo_identity, path: None
        } if repo_identity == identity));
        assert!(matches!(receiver.try_recv().expect("finished"), DaemonEvent::CommandFinished {
            command_id: 42, repo_identity, repo: None, result: CommandValue::Error { message }, ..
        } if repo_identity == identity && message == "expected error"));
        assert!(matches!(receiver.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }
}

// Behaviour (#2255): a spawned watch publishes start before initial/progress
// events and cancellation finish after them, on the same event bus.
#[tokio::test]
async fn event_publication_preserves_spawned_watch_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"event-publication-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let mut events = daemon.subscribe();
    let id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceWatch {
                    namespace: "flotilla".into(),
                    kind: "convoy".into(),
                    name: None,
                    include_replicas: false,
                    replica_sources: false,
                    cursor: None,
                })
                .build(),
        )
        .await
        .expect("start watch");
    tokio::time::timeout(Duration::from_secs(2), async {
        assert!(
            matches!(events.recv().await.expect("started"), DaemonEvent::CommandStarted { command_id, repo: None, .. } if command_id == id)
        );
        for _ in 0..2 {
            assert!(matches!(events.recv().await.expect("watch progress"), DaemonEvent::CommandStepUpdate {
                command_id, repo: None, status: flotilla_protocol::StepStatus::Produced { value }, ..
            } if command_id == id && matches!(*value, CommandValue::ResourceWatchEvent(_))));
        }
        daemon.cancel(id).await.expect("cancel watch");
        assert!(matches!(events.recv().await.expect("finished"), DaemonEvent::CommandFinished {
            command_id, repo: None, result: CommandValue::Cancelled, ..
        } if command_id == id));
        assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    })
    .await
    .expect("watch lifecycle completes");
}

// #2543: scope waits use the latest deadline independently of batch ordering;
// hard errors and untimed limits remain substantive refusals for their subjects.
#[hegel::test]
fn observation_cooldown_is_deterministic(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use crate::providers::github_api::{GithubRateLimit, GithubRateLimitKind, GithubRetrySource};
    // Deadlines span expired, current and future resets; duplicates exercise the
    // lowest-subject tie break. Generated permutations, their reversals and fresh HashMaps vary order.
    let now = Utc.timestamp_opt(1800000000, 0).single().expect("now");
    let offsets: Vec<i64> = (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(8)))
        .map(|_| tc.draw(gs::integers::<i64>().min_value(-2).max_value(2)))
        .collect();
    let timed = |number: u64, offset: Option<i64>| ObservationError::RateLimited {
        budget: format!("subject-{number}"),
        limit: GithubRateLimit {
            kind: GithubRateLimitKind::Primary,
            retry_at: offset.map(|offset| now + chrono::Duration::seconds(offset)),
            retry_source: if offset.is_some() { GithubRetrySource::RateLimitReset } else { GithubRetrySource::Unavailable },
        },
    };
    let expected = offsets
        .iter()
        .enumerate()
        .max_by_key(|(index, offset)| (**offset, std::cmp::Reverse(*index)))
        .map(|(index, offset)| timed(index as u64, Some(*offset)));
    for reverse in [false, true] {
        let mut entries: Vec<_> =
            offsets.iter().enumerate().map(|(index, offset)| (index as u64, Err(timed(index as u64, Some(*offset))))).collect();
        entries.push((100, Err(ObservationError::Forge("hard refusal".into()))));
        entries.push((101, Err(timed(101, None))));
        for index in 0..entries.len() {
            let other = tc.draw(gs::integers::<usize>().min_value(0).max_value(entries.len() - 1));
            entries.swap(index, other);
        }
        if reverse {
            entries.reverse();
        }
        let result = Ok(entries.into_iter().collect::<BoundObservations>());
        let actual = observation_rate_limit_error(&result);
        assert_eq!(actual, expected.as_ref());
        if let Some(error) = actual {
            assert_eq!(
                observation_during_cooldown(&result, 100, error).expect_err("hard refusal"),
                ObservationError::Forge("hard refusal".into())
            );
            assert_eq!(observation_during_cooldown(&result, 101, error).expect_err("untimed limit"), timed(101, None));
            for number in 0..offsets.len() as u64 {
                assert_eq!(observation_during_cooldown(&result, number, error).expect_err("scope cooldown"), *error);
            }
            assert_eq!(observation_during_cooldown(&result, 102, error).expect_err("new subject waits"), *error);
            let offset = *offsets.iter().max().expect("timed limits");
            assert_eq!(
                observation_cache_delay(error.retry_at(), now),
                if offset > 0 { Duration::from_secs(offset as u64) } else { OBSERVATION_CACHE_FALLBACK_DELAY }
            );
        }
    }
    assert!(observation_rate_limit_error(&Ok(BoundObservations::new())).is_none());
}

struct NoCooldownForgeReads;
#[async_trait]
impl ChangeRequestQueryPort for NoCooldownForgeReads {
    async fn discover_repository_change_request(
        &self,
        _namespace: &str,
        _repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        panic!("cooldown must prevent forge discovery and reads")
    }
}

// #2543: normal, fresh completion, and Landing/batch reads all respect the same
// cached scope cooldown, including new subjects and prior successful outcomes.
#[tokio::test(start_paused = true)]
async fn distinct_cooldowns_block_all_observation_reads() {
    use crate::providers::github_api::{GithubRateLimit, GithubRateLimitKind, GithubRetrySource};
    let source =
        ProviderChangeRequestObservationSource::new(ResourceBackend::InMemory(InMemoryBackend::default()), Arc::new(NoCooldownForgeReads));
    let now = Utc::now();
    let timed = |seconds| ObservationError::RateLimited {
        budget: format!("reset-{seconds}"),
        limit: GithubRateLimit {
            kind: GithubRateLimitKind::Primary,
            retry_at: Some(now + chrono::Duration::seconds(seconds)),
            retry_source: GithubRetrySource::RateLimitReset,
        },
    };
    let controlling = timed(60);
    let successful = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: Default::default(),
        head_sha: Default::default(),
        checks: Default::default(),
        review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
        mergeable: Default::default(),
    };
    let result = Ok([
        (1, Err(timed(10))),
        (2, Err(controlling.clone())),
        (3, Ok(successful)),
        (4, Err(ObservationError::Forge("hard refusal".into()))),
    ]
    .into_iter()
    .collect());
    let expires_at = tokio::time::Instant::now()
        + observation_cache_delay(observation_rate_limit_error(&result).and_then(ObservationError::retry_at), now);
    source.cache.lock().await.insert(
        ("flotilla".into(), "github.com".into(), "team/repo".into()),
        Arc::new(Mutex::new(Some(
            CachedObservation::builder()
                .expires_at(expires_at)
                .queried([1, 2, 3, 4].into_iter().collect())
                .next_history_start(0)
                .result(result)
                .build(),
        ))),
    );
    for seconds in [0, 11, 48] {
        tokio::time::advance(Duration::from_secs(seconds)).await;
        for number in [1, 2, 3, 4, 5] {
            let subject =
                ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number };
            let expected = if number == 4 { ObservationError::Forge("hard refusal".into()) } else { controlling.clone() };
            assert_eq!(source.observe(&subject).await.expect_err("ordinary read waits"), expected);
            assert_eq!(source.observe_for_completion(&subject).await.expect_err("completion waits"), expected);
            assert_eq!(source.observe_group(std::slice::from_ref(&subject), &subject).await.expect_err("batch waits"), expected);
        }
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    let subject = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number: 1 };
    let expired = source.observe_for_completion(&subject).await.expect_err("no repository after cache expires");
    assert!(matches!(expired, ObservationError::Forge(_)), "cache expires at controlling deadline");
}

// #2202: row publication and subject discovery reuse every branch lookup,
// including misses/errors. Each repository is queried once per refresh, even
// when a duplicate key is supplied; successes persist despite another failure.
#[hegel::test]
fn convoy_branch_refresh_reuses_repository_results(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // All pairs of found, absent, ordinary failure, and classified limit,
    // plus empty input and duplicate keys. Async interleavings are covered by
    // the aggregator's generation/cancellation tests; this seam is sequential.
    let replies = [RestAdmissionReply::Success, RestAdmissionReply::Absent, RestAdmissionReply::Ordinary, RestAdmissionReply::Limited];
    let outcomes = [
        replies[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))],
        replies[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))],
    ];
    let empty = tc.draw(gs::booleans());
    let duplicate = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for (outcomes, empty, duplicate) in [
            (outcomes, empty, duplicate),
            ([RestAdmissionReply::Success, RestAdmissionReply::Success], false, true),
            ([RestAdmissionReply::Absent, RestAdmissionReply::Success], false, false),
            ([RestAdmissionReply::Limited, RestAdmissionReply::Success], false, false),
        ] {
            let fixture = rest_admission_fixture(outcomes, RestAdmissionLookup::Branch).await;
            let snapshots = fixture
                .keys
                .iter()
                .enumerate()
                .map(|(index, key)| ConvoyRepositorySpec {
                    repo_ref: key.clone(),
                    url: format!("https://github.com/team/repo{index}"),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: format!("repo{index}"),
                    subpaths: Vec::new(),
                })
                .collect::<Vec<_>>();
            let spec = ConvoySpec::builder()
                .workflow_ref("workflow".into())
                .repositories(if empty { Vec::new() } else { snapshots })
                .r#ref("feature/wanted".into())
                .build();
            let convoys = fixture.daemon.resource_backend().using::<ResourceConvoy>("flotilla");
            convoys.create(&test_meta("refresh-reuse"), &spec).await.expect("convoy");
            let mut keys = if empty { Vec::new() } else { fixture.keys.clone() };
            if !empty && duplicate {
                keys.push(keys[0].clone());
            }
            let refresh = fixture.daemon.refresh_convoy_branch(&keys, "feature/wanted", None).await;
            let discovery = fixture
                .daemon
                .discover_convoy_branch_subjects_with_resolution("flotilla", "refresh-reuse", "feature/wanted", Some(&refresh))
                .await;
            let expected_count = if empty { 0 } else { 2 };
            assert_eq!(fixture.calls.load(Ordering::SeqCst), expected_count);
            assert_eq!(refresh.repositories.len(), expected_count);
            let expected_found = if empty { 0 } else { outcomes.iter().filter(|reply| **reply == RestAdmissionReply::Success).count() };
            let convoy = convoys.get("refresh-reuse").await.expect("convoy");
            let status = convoy.status.unwrap_or_default();
            assert_eq!(status.subjects.len(), expected_found);
            let failed = !empty && outcomes.iter().any(|reply| matches!(reply, RestAdmissionReply::Ordinary | RestAdmissionReply::Limited));
            assert_eq!(discovery.is_err(), failed);
            assert_eq!(status.branch_subject_scan_error.is_some(), failed);
            if expected_found > 0 {
                let expected_index = outcomes.iter().position(|reply| *reply == RestAdmissionReply::Success).expect("match");
                assert_eq!(refresh.primary.expect("primary success").expect("match").repository_key, fixture.keys[expected_index]);
            } else {
                assert_eq!(refresh.primary.is_err(), failed);
            }
        }
    });
}

// #2677: PR-less claim exits accept the claiming crew's artifact, without any comment pointer,
// and settle. A missing artifact (including one belonging to another crew) still refuses the claim.
#[tokio::test]
async fn prless_decision_ledger_claim_accepts_and_settles() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &test_meta("ledger-claim"),
            &ConvoySpec::builder()
                .workflow_ref("test".into())
                .subjects(vec![flotilla_resources::DeclaredSubject {
                    subject: flotilla_protocol::Subject {
                        kind: flotilla_protocol::SubjectKind::Issue,
                        source: flotilla_protocol::IssueSource { service: "forgejo.example".into(), scope: "owner/repo".into() },
                        id: "42".into(),
                    },
                    relationship: flotilla_protocol::Relationship::WorksOn,
                    issue: None,
                    change_request: None,
                }])
                .build(),
        )
        .await
        .expect("convoy");
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::Claim(flotilla_resources::ClaimExit));
    convoys
        .update_status(
            "ledger-claim",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(snapshot),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("working crew");
    backend
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("ledger-claim-vessel"),
            &VesselSpec {
                convoy_ref: "ledger-claim".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    let context = CrewCommandContext {
        namespace: Some("flotilla".into()),
        convoy: Some("ledger-claim".into()),
        vessel_ref: Some("ledger-claim-vessel".into()),
        role: Some("coder".into()),
        ..Default::default()
    };
    // Review #2681: a legacy Done status alone must not bypass the artifact expectation.
    let current = convoys.get("ledger-claim").await.expect("convoy");
    let mut status = current.status.expect("status");
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("claim").phase = CrewWorkPhase::Done;
    convoys.update_status("ledger-claim", &current.metadata.resource_version, &status).await.expect("legacy Done claim");
    daemon
        .crew_complete_with_disposition_internal(&context, None, None, None)
        .await
        .expect_err("Done without admission evidence must not bypass validation");
    for producer in ["reviewer", "coder"] {
        daemon
            .crew_complete_with_disposition_internal(&context, None, None, None)
            .await
            .expect_err("claim without this crew's artifact must be refused");
        let name = flotilla_resources::artifact_record_name("ledger-claim", producer, "decision-ledger", "ledger-claim");
        backend
            .using::<flotilla_resources::Artifact>("flotilla")
            .create(
                &test_meta(&name),
                &flotilla_resources::ArtifactSpec::builder()
                    .convoy("ledger-claim".into())
                    .producer(producer.into())
                    .kind("decision-ledger".into())
                    .subject("ledger-claim".into())
                    .digest("ledger".into())
                    .size(1)
                    .media_type("text/markdown".into())
                    .expires_at(Utc::now() + chrono::Duration::days(1))
                    .build(),
            )
            .await
            .expect("artifact");
    }
    assert_eq!(
        daemon.crew_complete_with_disposition_internal(&context, None, None, None).await.expect("artifact-backed claim"),
        CommandValue::Ok
    );
    let explanation = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".into()), name: "ledger-claim".into() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain accepted claim");
    let CommandValue::ConvoyExplanation(explanation) = explanation else { panic!("expected convoy explanation") };
    let admitted = convoys.get("ledger-claim").await.expect("admitted convoy");
    assert_eq!(admitted.status.as_ref().expect("status").crew_work["work"]["coder"].decision_ledger_digest.as_deref(), Some("ledger"));
    assert!(!explanation.decision_ledgers[0].missing);
    assert!(!explanation.decision_ledgers[0].projection_missing);
    assert!(explanation.settlement.satisfied, "artifact-backed issue convoy must explain as settled");

    // An accepted claim remains admitted on retry after evidence retention removes its artifact.
    let ledgers = backend.using::<flotilla_resources::Artifact>("flotilla");
    let name = flotilla_resources::artifact_record_name("ledger-claim", "coder", "decision-ledger", "ledger-claim");
    ledgers.delete(&name).await.expect("artifact retention");
    assert_eq!(
        daemon.crew_complete_with_disposition_internal(&context, None, None, None).await.expect("duplicate claim"),
        CommandValue::Ok
    );
    let convoy = convoys.get("ledger-claim").await.expect("convoy");
    let claim = &convoy.status.as_ref().expect("status").crew_work["work"]["coder"];
    assert_eq!(claim.phase, CrewWorkPhase::Done);
    assert!(claim.decision_ledger_ref.is_none());
    let settlement = flotilla_resources::evaluate_landing_settlement(
        &convoy,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(30),
        Utc::now(),
    );
    assert!(settlement.satisfied, "claim exit must settle without a PR comment");
    watch.abort();
}

// #2698: automatic branch discovery must not bind an already terminal PR
// to a newly created convoy. Explicit adoption remains a separate operation.
#[tokio::test]
async fn branch_discovery_ignores_preexisting_terminal_request() {
    for state in [flotilla_protocol::ChangeRequestStatus::Merged, flotilla_protocol::ChangeRequestStatus::Closed] {
        let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
        let provider = Arc::new(FakeChangeRequest::new());
        provider
            .add_change_requests(vec![(
                "7".into(),
                ChangeRequest {
                    title: "Old work".into(),
                    branch: "reused".into(),
                    status: state,
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            )])
            .await;
        fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
            provider;
        let convoys = fixture.daemon.resource_backend().using::<ResourceConvoy>("flotilla");
        convoys
            .create(
                &test_meta("new-work"),
                &ConvoySpec::builder()
                    .workflow_ref("dev".into())
                    .r#ref("reused".into())
                    .repositories(vec![ConvoyRepositorySpec {
                        url: "https://github.com/team/repo0".into(),
                        repo_ref: fixture.keys[0].clone(),
                        source_ref: "main".into(),
                        target_ref: "main".into(),
                        workspace_slug: "repo0".into(),
                        subpaths: vec![],
                    }])
                    .build(),
            )
            .await
            .expect("convoy");
        fixture.daemon.discover_convoy_branch_subjects("flotilla", "new-work", "reused").await.expect("discovery");
        let convoy = convoys.get("new-work").await.expect("convoy");
        assert!(convoy.status.unwrap_or_default().subjects.is_empty(), "old terminal PR must not become produced work");
    }
}

// #2698: a crew can switch branches, or use a differently named upstream.
// Discovery uses real Git facts and an in-memory forge collaborator, then
// stores the new PR as a produced subject and respects operator unlink.
#[tokio::test]
async fn checkout_branch_switch_discovers_actual_request_and_unlink_wins() {
    use crate::vcs::Vcs;
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let root = tempfile::tempdir().expect("git fixture");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").args(args).current_dir(root.path()).output().expect("git");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-b", "requested"]);
    git(&["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
    let provider = Arc::new(FakeChangeRequest::new());
    provider
        .add_change_requests(vec![
            (
                "7".into(),
                ChangeRequest {
                    title: "Old work".into(),
                    branch: "requested".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Merged,
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            ),
            (
                "8".into(),
                ChangeRequest {
                    title: "Real work".into(),
                    branch: "actual".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            ),
        ])
        .await;
    fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
        provider;
    let backend = fixture.daemon.resource_backend();
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    convoys
        .create(
            &test_meta("switch-work"),
            &ConvoySpec::builder()
                .workflow_ref("dev".into())
                .r#ref("requested".into())
                .adopted_checkout_refs(BTreeMap::from([(fixture.keys[0].clone(), "switch-checkout".into())]))
                .repositories(vec![ConvoyRepositorySpec {
                    url: "https://github.com/team/repo0".into(),
                    repo_ref: fixture.keys[0].clone(),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: "repo0".into(),
                    subpaths: vec![],
                }])
                .build(),
        )
        .await
        .expect("convoy");
    let checkouts = backend.using::<ResourceCheckout>("flotilla");
    let checkout = checkouts
        .create(
            &InputMeta::builder()
                .name("switch-checkout".into())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "switch-work".into())]))
                .build(),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "requested".into(),
                path: root.path().to_string_lossy().into_owned(),
                repo_ref: fixture.keys[0].clone(),
                host_ref: "test-host".into(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout");
    let conflict =
        fixture.daemon.validate_new_checkout_branch(&checkout).await.expect("forge lookup").expect("old merged head must refuse creation");
    assert!(conflict.contains("requested") && conflict.contains("#7"), "{conflict}");
    let runner = Arc::new(crate::providers::ProcessCommandRunner);
    let vcs = crate::vcs::FlotillaVcs::new(
        ExecutionEnvironmentPath::new(root.path()),
        runner.clone(),
        GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), runner))),
    );
    assert!(fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("old branch lookup").is_none());
    git(&["checkout", "--detach"]);
    assert!(fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("detached lookup").is_none());
    git(&["switch", "requested"]);
    // Merge evidence must belong to this checkout's lifetime, including the equality boundary.
    for offset in [-1, 0, 1] {
        let mut prior_checkout = checkout.clone();
        let mut prior = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).build();
        prior.integration.landed_evidence = Some(
            flotilla_resources::LandedEvidence::builder()
                .change_request_id("7".into())
                .merged_at((checkout.metadata.creation_timestamp + chrono::Duration::seconds(offset)).to_rfc3339())
                .build(),
        );
        prior_checkout.status = Some(prior);
        assert_eq!(
            fixture.daemon.resolve_live_checkout_change_request(&prior_checkout, &vcs, root.path()).await.expect("dated lookup"),
            (offset >= 0).then(|| "7".to_string())
        );
    }
    git(&["switch", "-c", "actual"]);
    assert_eq!(
        fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("switched lookup"),
        Some("8".into())
    );
    git(&["switch", "-c", "local-name"]);
    git(&["branch", "--set-upstream-to", "actual"]);
    assert_eq!(
        fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("upstream lookup"),
        Some("8".into())
    );
    git(&["remote", "add", "origin", root.path().to_str().expect("root")]);
    git(&["update-ref", "refs/remotes/origin/actual", "HEAD"]);
    git(&["branch", "--set-upstream-to", "origin/actual"]);
    assert_eq!(
        fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("remote upstream lookup"),
        Some("8".into())
    );
    assert_eq!(vcs.current_branch().await.expect("branch").trim(), "local-name");
    let mut status = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).build();
    status.integration.change_request = Some(
        flotilla_resources::ChangeRequestObservation::builder()
            .id("8".into())
            .state(flotilla_resources::ChangeRequestState::Open)
            .mergeability(flotilla_resources::ChangeRequestMergeability::Mergeable)
            .observed_at(Utc::now().to_rfc3339())
            .build(),
    );
    checkouts.update_status("switch-checkout", &checkout.metadata.resource_version, &status).await.expect("observation");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("discovery");
    let convoy = convoys.get("switch-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("subjects").subjects[0].subject.id, "8");
    // Repair an already persisted stale produces link, as in the operator's report.
    fixture
        .daemon
        .link_convoy_subject("flotilla", "switch-work", "repo0!7", Some(flotilla_protocol::Relationship::Produces))
        .await
        .expect("stale link");
    fixture.daemon.link_convoy_subject("flotilla", "switch-work", "repo0!7", None).await.expect("unlink stale merged subject");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("repair discovery");
    let repaired = convoys.get("switch-work").await.expect("convoy").status.expect("subjects");
    assert_eq!(repaired.subjects.len(), 1);
    assert_eq!(repaired.subjects[0].subject.id, "8");
    fixture.daemon.link_convoy_subject("flotilla", "switch-work", "repo0!8", None).await.expect("unlink");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("repeat discovery");
    assert!(convoys.get("switch-work").await.expect("convoy").status.expect("subjects").subjects.is_empty());
}

// #2698: an earlier absence observation cannot authorize creation after a
// closed PR appears for that branch. Creation checks the forge afresh.
#[tokio::test]
async fn checkout_creation_does_not_reuse_cached_branch_absence() {
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let provider = Arc::new(FakeChangeRequest::new());
    let observed = Arc::new(crate::forge_observation::ObservedChangeRequestTracker {
        inner: provider.clone(),
        reads: crate::forge_observation::ForgeReads::new(fixture.daemon.resource_backend(), "flotilla".into()),
        source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "team/repo0".into() },
    });
    fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
        observed.clone();
    assert!(fixture
        .daemon
        .resolve_convoy_change_request(std::slice::from_ref(&fixture.keys[0]), "reused", None)
        .await
        .expect("absence")
        .is_none());
    provider
        .add_change_requests(vec![(
            "7".into(),
            ChangeRequest {
                title: "Old work".into(),
                branch: "reused".into(),
                status: flotilla_protocol::ChangeRequestStatus::Closed,
                body: None,
                provider_name: "github".into(),
                provider_display_name: "GitHub".into(),
            },
        )])
        .await;
    // Ordinary observation may reuse the earlier absence; creation must still
    // inspect the forge anew through the same production observation adapter.
    assert!(observed.find_change_request_by_branch("reused").await.expect("cached absence").is_none());
    let checkout = fixture
        .daemon
        .resource_backend()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &test_meta("new-checkout"),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "reused".into(),
                path: "/new".into(),
                repo_ref: fixture.keys[0].clone(),
                host_ref: "test".into(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout");
    let error = fixture.daemon.validate_new_checkout_branch(&checkout).await.expect("forge lookup").expect("fresh conflict");
    assert!(error.contains("reused") && error.contains("#7"), "{error}");
}

// Behaviour (#2727): admission freezes a need-selected composition while the
// generation-1 baseline stays the running image; a missing need refuses by name.
#[tokio::test]
async fn image_layer_admission_freezes_inputs_and_keeps_baseline_authoritative() {
    use flotilla_resources::{
        CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource, ImageLayer, ImageLayerParent, ImageLayerSelection, ImageLayerSpec,
        ImageLayerStage,
    };
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-image-composition\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "crew-policy", "host-a", BTreeSet::new()).await;
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let mut policy = policies.get("crew-policy").await.expect("policy");
    policy.spec.docker_per_vessel.as_mut().expect("docker").image = DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() };
    policies.update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec).await.expect("baseline policy");
    let base = ImageLayerSpec::builder()
        .stage(ImageLayerStage::Base)
        .parent(ImageLayerParent::Image(format!("debian@sha256:{}", "a".repeat(64))))
        .repository("https://example.test/images".into())
        .revision("1".repeat(40))
        .fragment("Dockerfile.base".into())
        .provides(BTreeSet::from(["os:debian-family".into()]))
        .build();
    let display = ImageLayerSpec::builder()
        .stage(ImageLayerStage::Capability)
        .parent(ImageLayerParent::Rebasable)
        .repository("https://example.test/images".into())
        .revision("2".repeat(40))
        .fragment("Dockerfile.display".into())
        .provides(BTreeSet::from(["display:headless-x11".into()]))
        .requires(BTreeSet::from(["os:debian-family".into()]))
        .build();
    let layers = backend.definitions::<ImageLayer>("flotilla");
    layers.apply(&test_meta("base"), &base).await.expect("base layer");
    layers.apply(&test_meta("display"), &display).await.expect("display layer");
    let baselines = backend.definitions::<CrewImageBaseline>("flotilla");
    baselines
        .apply(
            &test_meta("fleet-crew"),
            &CrewImageBaselineSpec { image: "crew:v1".into(), layers: Some(ImageLayerSelection::builder().base("base".into()).build()) },
        )
        .await
        .expect("baseline alongside layers");
    let crew = CrewSpec::builder()
        .role("tool".into())
        .source(CrewSource::Tool { command: "true".into() })
        .needs(BTreeSet::from(["display:headless-x11".parse().expect("open need")]))
        .build();
    let workflow =
        WorkflowTemplateSpec::builder().vessels(vec![VesselRequirement::builder().name("work".into()).crew(vec![crew]).build()]).build();
    let admitted = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("admission");
    let image = admitted.selected.expect("selected").spec.docker_per_vessel.expect("docker").image;
    let DockerImageSource::Composition { composition } = &image else { panic!("admission must store composition") };
    assert_eq!(composition.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(), ["base", "display"]);
    assert_eq!(composition.layers[0].spec, base);
    assert!(composition.identity.is_none());
    assert_eq!(image.resolve(&baselines).await.expect("running image"), "crew:v1");
    let mut next_base = base;
    next_base.revision = "3".repeat(40);
    layers.apply(&test_meta("base"), &next_base).await.expect("update base");
    assert_eq!(composition.layers[0].spec.revision, "1".repeat(40));
    // A placement-bound composition keeps its concrete identity on readmission.
    let mut bound = composition.clone();
    bound.bind(flotilla_resources::PlacedImageIdentity { local_image_id: "sha256:placed".into(), registry_digest: None }).expect("bind");
    let mut live = policies.get("crew-policy").await.expect("live policy");
    live.spec.docker_per_vessel.as_mut().expect("docker").image = DockerImageSource::Composition { composition: bound.clone() };
    let live = policies.update(&InputMeta::from(&live.metadata), &live.metadata.resource_version, &live.spec).await.expect("bound policy");
    let repeated =
        daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("readmission");
    let DockerImageSource::Composition { composition: repeated } =
        repeated.selected.expect("selected").spec.docker_per_vessel.expect("docker").image
    else {
        panic!("composition")
    };
    assert_eq!(repeated.identity, bound.identity);
    assert_eq!(repeated.layers, bound.layers);
    let mut restored = live.spec;
    restored.docker_per_vessel.as_mut().expect("docker").image = DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() };
    policies
        .update(&InputMeta::from(&live.metadata), &live.metadata.resource_version, &restored)
        .await
        .expect("restore baseline selection");
    let next = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("next admission");
    let DockerImageSource::Composition { composition: next } =
        next.selected.expect("selected").spec.docker_per_vessel.expect("docker").image
    else {
        panic!("composition")
    };
    assert_eq!(next.layers[0].spec.revision, "3".repeat(40));
    let mut missing = workflow;
    missing.vessels[0].crew[0].needs = BTreeSet::from(["display:missing".parse().expect("need")]);
    let error =
        daemon.resolve_convoy_placement("flotilla", None, &[], &missing, Some("crew-policy"), false).await.expect_err("missing need");
    assert!(error.contains("display:missing"));
}

// #2767: address-taking commands share this resolver. Terminal history must
// never displace a live governor; an exact record ID still addresses history.
#[test]
fn convoy_address_prefers_live_over_two_terminal_namesakes() {
    // Exhaustive finite contract: both project-scoped/projectless addresses and
    // all six input orders. Terminal phases share this boolean resolver input.
    for project in [None, Some("p")] {
        let scoped_address = format!("governor@{}", project.unwrap_or_default());
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let names = ["old-a", "old-b", "live"];
            let identities = order.map(|index| ConvoyAddressIdentity {
                record_name: names[index],
                role: Some("governor"),
                project,
                terminal: index < 2,
            });
            for address in [scoped_address.as_str(), "governor"] {
                let indices = resolve_convoy_candidate_indices(&identities, address).expect("unique live governor");
                assert_eq!(indices.iter().map(|index| identities[*index].record_name).collect::<Vec<_>>(), ["live"]);
            }
            let indices = resolve_convoy_candidate_indices(&identities, "old-a").expect("explicit history ID");
            assert_eq!(indices.iter().map(|index| identities[*index].record_name).collect::<Vec<_>>(), ["old-a"]);

            // A second live match must refuse and name both resource IDs.
            let mut ambiguous = Vec::from(identities);
            ambiguous.push(ConvoyAddressIdentity { record_name: "another-live", role: Some("governor"), project, terminal: false });
            let error = resolve_convoy_candidate_indices(&ambiguous, &scoped_address).expect_err("ambiguous live address");
            // Refusal must identify the live candidates, without pinning its prose.
            let named_records = error.split(|c: char| !c.is_ascii_alphanumeric() && c != '-').collect::<BTreeSet<_>>();
            for record in ["another-live", "live"] {
                assert!(named_records.contains(record), "missing {record} in refusal: {error}");
            }
            for record in ["old-a", "old-b"] {
                assert!(!named_records.contains(record), "terminal history is not an ambiguous live candidate: {error}");
            }
        }
    }
    assert!(resolve_convoy_candidate_indices(&[], "governor@p").expect("empty candidates").is_empty());
}

// Message mutations use receiver-side admission, preserving idempotent IDs and
// superseding pending intent rather than appending independent terminal inputs.
#[tokio::test]
async fn resource_message_mutations_use_durable_inbox_admission() {
    use flotilla_resources::{Message, MessagePhase};
    let temp = tempfile::tempdir().expect("config directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-test\"\n").expect("machine identity");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let mut document = serde_json::json!({
        "apiVersion": "flotilla.work/v1", "kind": "Message", "metadata": {"name": "first"},
        "spec": {"sender": "flotilla/checks", "receiver": "flotilla/convoy/work/coder", "relation": "system", "body": "checks settled"}
    });
    let first = daemon.apply_intent_document("flotilla", document.clone()).await.expect("admit");
    let retry = daemon.apply_intent_document("flotilla", document.clone()).await.expect("retry");
    assert_eq!(first.value, retry.value);
    document["metadata"]["name"] = "next".into();
    document["spec"]["supersedes"] = "first".into();
    daemon.apply_intent_document("flotilla", document).await.expect("successor");
    let first = backend.using::<Message>("flotilla").get("first").await.expect("read first");
    assert_eq!(first.status.expect("status").phase, MessagePhase::Superseded);
}

// A receiver's admitted convoy supplies a home before its terminal is present;
// ordinary ResourceApply routing follows that origin rather than the sender or
// an explicitly requested unrelated node.
#[tokio::test]
async fn new_message_mutations_route_to_the_absent_receivers_home() {
    use crate::command_target::TargetHost;
    let temp = tempfile::tempdir().expect("config directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-test\"\n").expect("machine identity");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("sender"),
        backend.clone(),
    )
    .await;
    let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("receiver"));
    let remote_convoys = remote.using::<ResourceConvoy>("flotilla");
    remote_convoys
        .create(&test_meta("convoy"), &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build())
        .await
        .expect("receiver convoy");
    backend
        .replica_writer::<ResourceConvoy>(NodeId::new("receiver"), "flotilla")
        .replace(&remote_convoys.list().await.expect("receiver snapshot"), chrono::Utc::now())
        .await
        .expect("replicate");
    let action = CommandAction::ResourceApply {
        namespace: "flotilla".into(),
        document: serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Message", "metadata": {"name": "first"},
            "spec": {"sender": "flotilla/checks", "receiver": "flotilla/convoy/work/coder", "relation": "system", "body": "checks settled"}
        }),
    };
    let target = daemon.resolve_command_target(&action, Some(&NodeId::new("unrelated"))).await.expect("route");
    assert_eq!(target.host, TargetHost::Node(NodeId::new("receiver")));
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("sender messages").items.is_empty());
}

// Resource admission qualifies a relative receiver from its sender context and
// returns the canonical delivered predecessor without creating the suppressed ID.
#[tokio::test]
async fn message_admission_qualifies_and_exposes_canonical_suppression() {
    use flotilla_resources::{Message, MessageStatusPatch, ResolvedMessageReceiver};
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-review-test\"\n").unwrap();
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let mut document = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"first"},
        "spec":{"sender":"flotilla/convoy/work/reviewer","receiver":"coder","relation":"peer","body":"reply please","expectation":{"kind":"reply"},"references":[{"kind":"change_request","service":"github","scope":"owner/repo","number":1,"revision":"head"}],"subject":{"kind":"change_request","service":"github","scope":"owner/repo","number":1,"revision":"head"}}
    });
    daemon.apply_intent_document("flotilla", document.clone()).await.unwrap();
    let messages = backend.using::<Message>("flotilla");
    assert_eq!(messages.get("first").await.unwrap().spec.receiver, "flotilla/convoy/work/coder");
    flotilla_resources::apply_status_patch(
        &messages,
        "first",
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew".into())
                .session("session".into())
                .delivered_at(chrono::Utc::now())
                .evidence("receipt".into())
                .build(),
            at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    document["metadata"]["name"] = "next".into();
    let canonical = daemon.apply_intent_document("flotilla", document).await.unwrap();
    assert_eq!(canonical.value.pointer("/metadata/name").and_then(serde_json::Value::as_str), Some("first"));
    assert!(matches!(messages.get("next").await, Err(ResourceError::NotFound { .. })));
}

// Unknown homes cannot fall through to the caller's requested host. A finished
// convoy and an undeclared vessel are explicit refusals before holder lookup.
#[tokio::test]
async fn message_routing_refuses_unknown_homes_and_finished_convoys() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-review-test\"\n").unwrap();
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let document =
        |receiver: &str| serde_json::json!({"spec":{"sender":"flotilla/checks","receiver":receiver,"relation":"system","body":"wake"}});
    assert!(daemon.message_creation_origin("flotilla", &document("flotilla/undeclared")).await.unwrap_err().contains("no declared home"));
    assert!(daemon
        .message_creation_origin("flotilla", &document("flotilla/missing/work/coder"))
        .await
        .unwrap_err()
        .contains("no admitted convoy"));
    let declarations = backend.using::<flotilla_resources::ConvoyEnsure>("flotilla");
    declarations
        .create(
            &test_meta("waiting-governor"),
            &flotilla_resources::ConvoyEnsureSpec::builder()
                .project_ref("flotilla".into())
                .role("governor".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("declared role before admission");
    assert_eq!(
        daemon.message_creation_origin("flotilla", &document("flotilla/governor")).await.expect("declared absent holder must wait"),
        Some(daemon.node_id().clone())
    );
    let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("remote-holder-home"));
    remote
        .using::<flotilla_resources::ConvoyEnsure>("flotilla")
        .create(
            &test_meta("waiting-reviewer"),
            &flotilla_resources::ConvoyEnsureSpec::builder()
                .project_ref("flotilla".into())
                .role("reviewer".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("remote absent holder declaration");
    backend
        .replica_writer::<flotilla_resources::ConvoyEnsure>(NodeId::new("remote-holder-home"), "flotilla")
        .replace(&remote.using::<flotilla_resources::ConvoyEnsure>("flotilla").list().await.expect("declaration feed"), Utc::now())
        .await
        .expect("replicated declaration");
    assert_eq!(
        daemon.message_creation_origin("flotilla", &document("flotilla/reviewer")).await.expect("remote declared absent holder"),
        Some(NodeId::new("remote-holder-home"))
    );
    let original = flotilla_resources::MessageSpec::builder()
        .sender("system:checks".into())
        .receiver("flotilla/governor".into())
        .relation(flotilla_resources::MessageRelation::System)
        .body("request".into())
        .build();
    daemon
        .message_inbox("flotilla")
        .await
        .accept(&test_meta("system-request"), &original, Utc::now())
        .await
        .expect("original system request");
    let reply = serde_json::json!({"spec":{"sender":"flotilla/c/work/coder","receiver":"system:checks","relation":"peer","body":"answer","in_reply_to":"system-request"}});
    assert_eq!(daemon.message_creation_origin("flotilla", &reply).await.expect("system reply home"), Some(daemon.node_id().clone()));
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(&test_meta("finished"), &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build())
        .await
        .unwrap();
    let status = ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() };
    convoys.update_status("finished", &convoy.metadata.resource_version, &status).await.unwrap();
    assert!(daemon
        .message_creation_origin("flotilla", &document("flotilla/finished/work/coder"))
        .await
        .unwrap_err()
        .contains("terminal convoy"));
    let convoy = convoys.get("finished").await.unwrap();
    let mut status = status;
    status.phase = ConvoyPhase::Active;
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: Vec::new(),
    });
    convoys.update_status("finished", &convoy.metadata.resource_version, &status).await.unwrap();
    assert!(daemon
        .message_creation_origin("flotilla", &document("flotilla/finished/work/coder"))
        .await
        .unwrap_err()
        .contains("undeclared vessel"));
}

#[tokio::test]
async fn message_apply_retries_only_conflicts_with_a_finite_budget() {
    for kind in ["Message", "Artifact"] {
        let mut attempts = 0;
        let result = retry_resource_apply(kind, || {
            attempts += 1;
            std::future::ready(if attempts < 3 { Err(ResourceError::conflict("message", "concurrent status")) } else { Ok(attempts) })
        })
        .await;
        assert_eq!(result.unwrap(), 3);
        let mut attempts = 0;
        let result: Result<(), _> = retry_resource_apply(kind, || {
            attempts += 1;
            std::future::ready(Err(ResourceError::conflict("message", "persistent conflict")))
        })
        .await;
        assert!(matches!(result, Err(ResourceError::Conflict { .. })));
        assert_eq!(attempts, 16);
        let mut attempts = 0;
        let result: Result<(), _> = retry_resource_apply(kind, || {
            attempts += 1;
            std::future::ready(Err(ResourceError::invalid("invalid intent")))
        })
        .await;
        assert!(matches!(result, Err(ResourceError::Invalid { .. })));
        assert_eq!(attempts, 1);
    }
    let mut attempts = 0;
    let result: Result<(), _> = retry_resource_apply("Convoy", || {
        attempts += 1;
        std::future::ready(Err(ResourceError::conflict("convoy", "no replay contract")))
    })
    .await;
    assert!(result.is_err());
    assert_eq!(attempts, 1);
}

// Tracker boundary: deliberately suspend native forge observations so the board
// scenario can prove interactive reads never wait for remote work.
struct SuspendedBoardProvider {
    calls: AtomicUsize,
    issue_calls: AtomicUsize,
    release: tokio::sync::Semaphore,
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
        Ok(super::dispatch_board::tests::board(source, 400))
    }
}

// #2842: realistic Project status and 400 serving convoys must not turn a
// board read into per-issue forge calls or wait on an unfinished bulk fetch.
#[tokio::test]
async fn large_dispatch_board_reads_projection_without_waiting_for_forge() {
    use flotilla_resources::{DispatchQueueEntry, ProjectStatus};
    let temp = tempfile::tempdir().expect("config");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"dispatch-board-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let source = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "org/large".into() };
    let provider = Arc::new(SuspendedBoardProvider {
        calls: AtomicUsize::new(0),
        issue_calls: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let projects = backend.using::<Project>("flotilla");
    let mut binding = flotilla_resources::IssueSourceBindingSpec::from(source.clone());
    binding.alias = Some("large".into());
    let now = Utc::now();
    for name in ["large", "shared"] {
        projects
            .create(
                &test_meta(name),
                &ProjectSpec::builder().display_name(name.to_string()).issue_source_bindings(vec![binding.clone()]).build(),
            )
            .await
            .expect("project");
        let project = projects.get(name).await.expect("current project");
        projects
            .update_status(
                name,
                &project.metadata.resource_version,
                &ProjectStatus {
                    dispatch_queue: (0..400)
                        .map(|id| DispatchQueueEntry {
                            score: None,
                            issue: flotilla_protocol::IssueRef { source: source.clone(), id: id.to_string() },
                            title: format!("Issue {id}"),
                            issue_as_of: now,
                            ready_observed_at: now,
                            observed_at: now,
                            provenance: "test".into(),
                        })
                        .collect(),
                    ..Default::default()
                },
            )
            .await
            .expect("readiness projection");
    }
    for id in 0..400 {
        backend
            .using::<ResourceConvoy>("flotilla")
            .create(
                &test_meta(&format!("convoy-{id}")),
                &ConvoySpec::builder().workflow_ref("workflow".to_string()).project_ref("large".to_string()).build(),
            )
            .await
            .expect("convoy");
    }
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::default().with_issue_tracker(provider.clone())),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let cold = tokio::time::timeout(Duration::from_secs(1), daemon.dispatch_board_internal(Some("large")))
        .await
        .expect("interactive read must not wait for forge");
    assert!(cold.expect_err("cold facts fail closed").contains("initial observation"));
    // Background warming and another Project must share the same source flight.
    assert!(daemon.refresh_dispatch_boards_internal().await.is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        while provider.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background fetch started");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    provider.release.add_permits(1);
    let board = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Ok(board) = daemon.dispatch_board_internal(Some("large")).await {
                break board;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observation published");
    assert_eq!(board.readiness.entries.len(), 400);
    assert_eq!(board.repositories.len(), 1);
    assert_eq!(board.repositories[0].issues.len(), 400);
    assert_eq!(board.repositories[0].pull_requests.len(), 400);
    assert_eq!(daemon.dispatch_board_internal(None).await.expect("shared board").readiness.entries.len(), 800);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    // A complete board never silently omits a selected cold source, while a
    // healthy Project remains available independently of that other source.
    let mut cold_binding = binding;
    cold_binding.source.scope = "org/cold".into();
    cold_binding.alias = Some("cold".into());
    projects
        .create(
            &test_meta("cold"),
            &ProjectSpec::builder().display_name("Cold".to_string()).issue_source_bindings(vec![cold_binding]).build(),
        )
        .await
        .expect("cold project");
    assert!(daemon.dispatch_board_internal(None).await.expect_err("complete source board").contains("initial observation"));
    assert_eq!(daemon.dispatch_board_internal(Some("large")).await.expect("healthy project").repositories[0].issues.len(), 400);
    // #2860: overlapping Projects read each cached source once per pass,
    // share the immutable observation, and isolate cold/broken bindings.
    let shared_spec = projects.get("large").await.expect("large project").spec;
    for id in 0..20 {
        projects.create(&test_meta(&format!("overlap-{id}")), &shared_spec).await.expect("overlap");
    }
    let mut broken = shared_spec.clone();
    broken.repositories = vec![flotilla_resources::ProjectRepositorySpec {
        charter_store: None,
        repo: flotilla_resources::RepositoryKey("missing".into()),
        alias: None,
        roles: Default::default(),
        subpath: None,
        default_branch: None,
    }];
    projects.create(&test_meta("broken"), &broken).await.expect("broken binding");
    let inventory = projects.list().await.expect("projects").items;
    let before = daemon.dispatch_board_cache.reads.load(Ordering::SeqCst);
    let inputs = daemon.collect_dispatch_board_inputs(&inventory).await.expect("pass inputs");
    assert_eq!(daemon.dispatch_board_cache.reads.load(Ordering::SeqCst) - before, 2);
    assert!(inputs["cold"].is_err());
    assert!(inputs["broken"].is_err());
    let shared = &inputs["large"].as_ref().expect("large scope").1[0].1;
    for id in 0..20 {
        let overlapping = &inputs[&format!("overlap-{id}")].as_ref().expect("overlap scope").1[0].1;
        assert!(Arc::ptr_eq(shared, overlapping));
    }
}

// #2868: three daemons sharing a Project poll its source once per pass.
// Nonowners obtain board facts through real resource replication; explicit
// unready evidence elects one replacement, and recovery restores the home.
struct CountingForgeRequests(AtomicUsize);
#[async_trait]
impl ChangeRequestTracker for CountingForgeRequests {
    async fn list_change_requests(&self, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        unreachable!()
    }
    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok((
            id.into(),
            ChangeRequest {
                title: "Shared PR".into(),
                branch: "work".into(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: None,
                provider_name: "fake".into(),
                provider_display_name: "Fake".into(),
            },
        ))
    }
    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn list_merged_branch_names(&self, _limit: usize) -> Result<Vec<String>, String> {
        unreachable!()
    }
}

#[tokio::test]
async fn three_host_forge_observation_has_one_owner_and_replicates_facts() {
    use flotilla_resources::{ForgeRead, Resource};
    let mut temps = Vec::new();
    let mut daemons = Vec::new();
    let mut providers = Vec::new();
    let mut request_providers = Vec::new();
    for name in ["owner", "second", "third"] {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("daemon.toml"), format!("machine_id = \"forge-{name}\"\n")).unwrap();
        let provider = Arc::new(SuspendedBoardProvider {
            calls: AtomicUsize::new(0),
            issue_calls: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(1000),
        });
        let requests = Arc::new(CountingForgeRequests(AtomicUsize::new(0)));
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery_with_provider_set(
                FakeDiscoveryProviders::default().with_issue_tracker(provider.clone()).with_change_request(requests.clone()),
            ),
            HostName::new(name),
            ResourceBackend::InMemory(InMemoryBackend::default()),
        )
        .await;
        temps.push(temp);
        daemons.push(daemon);
        providers.push(provider);
        request_providers.push(requests);
    }
    let source = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "org/shared".into() };
    let project = ProjectSpec::builder()
        .display_name("Shared".into())
        .issue_source_bindings(vec![flotilla_resources::IssueSourceBindingSpec::builder()
            .source(source.clone())
            .alias("shared".into())
            .build()])
        .build();
    daemons[0].resource_backend().using::<Project>("flotilla").create(&test_meta("shared"), &project).await.unwrap();
    daemons[1].resource_backend().using::<Project>("flotilla").create(&test_meta("also-shared"), &project).await.unwrap();
    async fn replicate<T: Resource>(daemons: &[Arc<InProcessDaemon>]) {
        for source in daemons {
            let backend = source.resource_backend();
            let listed = backend.using::<T>("flotilla").list().await.unwrap();
            for target in daemons {
                if Arc::ptr_eq(source, target) {
                    continue;
                }
                target
                    .resource_backend()
                    .replica_writer::<T>(backend.local_root().unwrap(), "flotilla")
                    .replace(&listed, Utc::now())
                    .await
                    .unwrap();
            }
        }
    }
    replicate::<Project>(&daemons).await;
    let repository = RepositorySpec::remote("https://github.com/org/shared").unwrap();
    daemons[0]
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository.key().to_string()), &repository)
        .await
        .unwrap();
    replicate::<Repository>(&daemons).await;
    let subject = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "org/shared".into(), number: 7 };
    let refreshers = daemons
        .iter()
        .map(|daemon| {
            crate::change_request_observer::ChangeRequestRefresher::new(
                "topology".into(),
                daemon.resource_backend(),
                daemon.resource_backend().local_root().unwrap().to_string(),
                daemon.change_request_observation_source.clone(),
                Default::default(),
            )
        })
        .collect::<Vec<_>>();

    for (index, daemon) in daemons.iter().enumerate() {
        let hosts = daemon.resource_backend().using::<ResourceHost>("flotilla");
        let host = hosts.create(&test_meta(&format!("host-{index}")), &HostSpec::default()).await.unwrap();
        if index == 0 {
            continue;
        }
        hosts
            .update_status(
                &host.metadata.name,
                &host.metadata.resource_version,
                &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
            )
            .await
            .unwrap();
    }
    replicate::<ResourceHost>(&daemons).await;
    // A declared Host whose first heartbeat has not arrived is unknown, not
    // positive unready evidence. Ready replicas must not steal its source.
    for daemon in &daemons {
        assert_eq!(
            crate::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(daemons[0].resource_backend().local_root().unwrap())
        );
    }
    let hosts = daemons[0].resource_backend().using::<ResourceHost>("flotilla");
    let host = hosts.get("host-0").await.unwrap();
    hosts
        .update_status(
            "host-0",
            &host.metadata.resource_version,
            &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .unwrap();
    replicate::<ResourceHost>(&daemons).await;
    for refresher in &refreshers {
        refresher.refresh_once(&subject).await.unwrap();
    }
    assert_eq!(request_providers.iter().map(|p| p.0.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![1, 0, 0]);
    replicate::<flotilla_resources::ChangeRequest>(&daemons).await;
    for daemon in &daemons {
        let record = daemon
            .resource_backend()
            .including_replicas::<flotilla_resources::ChangeRequest>("flotilla")
            .get(&subject.record_name())
            .await
            .unwrap();
        assert_eq!(record.object.status.unwrap().title.value.as_deref(), Some("Shared PR"));
    }

    for daemon in &daemons {
        assert_eq!(
            crate::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(daemons[0].resource_backend().local_root().unwrap()),
            "Project sources: {:?}; Hosts: {:?}",
            daemon.resource_backend().including_replicas::<Project>("flotilla").list_replica_sources().await.unwrap(),
            daemon.resource_backend().including_replicas::<ResourceHost>("flotilla").list_replica_sources().await.unwrap()
        );
    }
    daemons[0].issue_provider_for_source(&source).await.unwrap().dispatch_board(&source).await.expect("owner board read");
    for daemon in &daemons {
        let _ = daemon.issue_provider_for_source(&source).await.unwrap().dispatch_board(&source).await;
    }
    assert_eq!(providers.iter().map(|p| p.calls.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![1, 0, 0]);
    replicate::<ForgeRead>(&daemons).await;
    // Cached nonowner reads get the same hundreds of facts without forge I/O.
    for daemon in &daemons {
        let provider = daemon.issue_provider_for_source(&source).await.unwrap();
        assert_eq!(provider.dispatch_board(&source).await.unwrap().issues.len(), 400);
    }
    assert_eq!(providers.iter().map(|p| p.calls.load(Ordering::SeqCst)).sum::<usize>(), 1);
    // Issue pages, individual fetches and changed-since reads use the same
    // ownership seam, including requests first made on nonowners.
    let reference = flotilla_protocol::IssueRef { source: source.clone(), id: "1".into() };
    for daemon in &daemons {
        let provider = daemon.issue_provider_for_source(&source).await.unwrap();
        let _ = provider.query(&source, &Default::default(), 1, 50).await;
        let _ = provider.fetch_by_id(&reference).await;
        let _ = provider.list_changed_since(&source, "2026-10-01T00:00:00Z", 50).await;
    }
    assert_eq!(providers.iter().map(|p| p.issue_calls.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![3, 0, 0]);
    replicate::<ForgeRead>(&daemons).await;
    for daemon in &daemons {
        let provider = daemon.issue_provider_for_source(&source).await.unwrap();
        assert_eq!(provider.query(&source, &Default::default(), 1, 50).await.unwrap().items.len(), 1);
        assert_eq!(provider.fetch_by_id(&reference).await.unwrap().title, "Shared issue");
        assert_eq!(provider.list_changed_since(&source, "2026-10-01T00:00:00Z", 50).await.unwrap().updated.len(), 1);
    }
    assert_eq!(providers.iter().map(|p| p.issue_calls.load(Ordering::SeqCst)).sum::<usize>(), 3);
    let hosts = daemons[0].resource_backend().using::<ResourceHost>("flotilla");
    let host = hosts.get("host-0").await.unwrap();
    hosts
        .update_status(
            "host-0",
            &host.metadata.resource_version,
            &HostStatus { ready: false, heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .unwrap();
    replicate::<ResourceHost>(&daemons).await;
    let expected = daemons[1..].iter().map(|daemon| daemon.resource_backend().local_root().unwrap()).min().unwrap();
    for daemon in &daemons {
        assert_eq!(
            crate::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(expected.clone())
        );
    }
    // Expire every observation without sleeping; replicas preserve this age.
    for daemon in &daemons {
        let reads = daemon.resource_backend().using::<ForgeRead>("flotilla");
        for record in reads.list().await.unwrap().items {
            if let Some(mut status) = record.status {
                status.attempted_at -= chrono::Duration::seconds(61);
                reads.update_status(&record.metadata.name, &record.metadata.resource_version, &status).await.unwrap();
            }
        }
    }
    replicate::<ForgeRead>(&daemons).await;
    for daemon in &daemons {
        let _ = daemon.refresh_forge_read_demands().await;
    }
    let counts = providers.iter().map(|p| p.calls.load(Ordering::SeqCst)).collect::<Vec<_>>();
    assert_eq!(counts[0], 1);
    assert_eq!(counts[1] + counts[2], 1, "exactly one fallback polls");
    assert_eq!(providers[0].issue_calls.load(Ordering::SeqCst), 3);
    assert_eq!(providers[1..].iter().map(|p| p.issue_calls.load(Ordering::SeqCst)).sum::<usize>(), 3);
    replicate::<ForgeRead>(&daemons).await;
    for daemon in &daemons {
        assert_eq!(daemon.issue_provider_for_source(&source).await.unwrap().dispatch_board(&source).await.unwrap().issues.len(), 400);
    }
    assert_eq!(providers.iter().map(|p| p.calls.load(Ordering::SeqCst)).sum::<usize>(), 2);
    for refresher in &refreshers {
        refresher.refresh_once(&subject).await.unwrap();
    }
    assert_eq!(request_providers[0].0.load(Ordering::SeqCst), 1);
    assert_eq!(request_providers[1..].iter().map(|p| p.0.load(Ordering::SeqCst)).sum::<usize>(), 1);

    // Recovery restores the preferred observer; the fallback stops polling.
    let host = hosts.get("host-0").await.unwrap();
    hosts
        .update_status(
            "host-0",
            &host.metadata.resource_version,
            &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .unwrap();
    replicate::<ResourceHost>(&daemons).await;
    for daemon in &daemons {
        assert_eq!(
            crate::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(daemons[0].resource_backend().local_root().unwrap())
        );
        let reads = daemon.resource_backend().using::<ForgeRead>("flotilla");
        for record in reads.list().await.unwrap().items {
            if let Some(mut status) = record.status {
                status.attempted_at -= chrono::Duration::seconds(61);
                reads.update_status(&record.metadata.name, &record.metadata.resource_version, &status).await.unwrap();
            }
        }
    }
    replicate::<ForgeRead>(&daemons).await;
    for daemon in &daemons {
        daemon.refresh_forge_read_demands().await.unwrap();
    }
    for refresher in &refreshers {
        refresher.refresh_once(&subject).await.unwrap();
    }
    assert_eq!(request_providers[0].0.load(Ordering::SeqCst), 2);
    assert_eq!(request_providers[1..].iter().map(|p| p.0.load(Ordering::SeqCst)).sum::<usize>(), 1);
    assert_eq!(providers[0].calls.load(Ordering::SeqCst), 2);
    assert_eq!(providers[1..].iter().map(|p| p.calls.load(Ordering::SeqCst)).sum::<usize>(), 1);
    assert_eq!(providers[0].issue_calls.load(Ordering::SeqCst), 6);
    assert_eq!(providers[1..].iter().map(|p| p.issue_calls.load(Ordering::SeqCst)).sum::<usize>(), 3);
}

// #2873: fresh forge facts accept only open/draft continuation; a PR-free
// branch is valid, terminal requests require explicit reopening on the forge.
#[tokio::test]
async fn continuation_resolves_branch_without_pr_and_refuses_terminal_pr() {
    use flotilla_protocol::{ChangeRequestStatus, ConvoyContinuation};
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let repository = ConvoyRepositorySpec {
        repo_ref: fixture.keys[0].clone(),
        url: "https://github.com/team/repo0".into(),
        source_ref: "main".into(),
        target_ref: "main".into(),
        workspace_slug: "repo0".into(),
        subpaths: vec![],
    };
    let (key, branch, request) = fixture
        .daemon
        .convoy_admission
        .resolve_continuation(std::slice::from_ref(&repository), &ConvoyContinuation::Branch("wip".into()))
        .await
        .expect("WIP continuation");
    assert_eq!(key, fixture.keys[0]);
    assert_eq!(branch, "wip");
    assert!(request.is_none());
    for status in [ChangeRequestStatus::Merged, ChangeRequestStatus::Closed] {
        let provider = Arc::new(FakeChangeRequest::new());
        provider
            .add_change_requests(vec![(
                "7".into(),
                ChangeRequest {
                    title: "Old PR".into(),
                    branch: "wip".into(),
                    status,
                    body: None,
                    provider_name: "fake".into(),
                    provider_display_name: "Fake".into(),
                },
            )])
            .await;
        fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
            provider;
        for input in [ConvoyContinuation::Branch("wip".into()), ConvoyContinuation::ChangeRequest("7".into())] {
            let error = fixture
                .daemon
                .convoy_admission
                .resolve_continuation(std::slice::from_ref(&repository), &input)
                .await
                .err()
                .expect("terminal refusal");
            assert!(error.contains("reopen"), "{error}");
        }
    }
}
