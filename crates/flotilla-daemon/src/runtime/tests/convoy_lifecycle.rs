use super::*;
use crate::testkits::engine::{admit_convoy, engine};

#[test]
fn live_github_scope_expands_only_project_grants_without_explicit_repositories() {
    let original = RepositoryKey("first".to_string());
    let other = RepositoryKey("other".to_string());
    let repositories = BTreeSet::from([original.clone()]);
    let grant = |projects: BTreeSet<String>, repositories: BTreeSet<RepositoryKey>| {
        flotilla_resources::CredentialGrantSpec::builder()
            .selector(flotilla_resources::CredentialGrantSelector::builder().projects(projects).repositories(repositories).build())
            .credentials(BTreeSet::from(["github-app".to_string()]))
            .build()
    };
    let project_grant = grant(BTreeSet::from(["island".to_string()]), BTreeSet::new());
    let explicit_grant = grant(BTreeSet::from(["island".to_string()]), BTreeSet::from([original.clone(), other]));
    let non_project_grant = grant(BTreeSet::new(), BTreeSet::new());
    let roles = BTreeSet::from(["coder".to_string()]);
    let trust = BTreeMap::from([(original.clone(), RepositoryTrust::Own)]);

    let dynamic = github_app_scope_from_grants(
        "github-app",
        &repositories,
        Some("island"),
        &roles,
        &trust,
        false,
        std::slice::from_ref(&project_grant),
    );
    assert_eq!(dynamic.projects, BTreeSet::from(["island".to_string()]));
    assert!(dynamic.fixed_repositories.is_empty());

    let own_only = flotilla_resources::CredentialGrantSpec::builder()
        .selector(
            flotilla_resources::CredentialGrantSelector::builder()
                .projects(BTreeSet::from(["island".to_string()]))
                .repository_trust(RepositoryTrust::Own)
                .build(),
        )
        .credentials(BTreeSet::from(["github-app".to_string()]))
        .build();
    let trusted = github_app_scope_from_grants("github-app", &repositories, Some("island"), &roles, &trust, false, &[own_only]);
    assert!(trusted.projects.is_empty(), "new project forks must not enter an own-only mint scope");
    assert_eq!(trusted.fixed_repositories, repositories);

    for grant in [explicit_grant, non_project_grant] {
        let scope = github_app_scope_from_grants("github-app", &repositories, Some("island"), &roles, &trust, false, &[grant]);
        assert!(scope.projects.is_empty());
        assert_eq!(scope.fixed_repositories, repositories);
    }
    let pinned = github_app_scope_from_grants("github-app", &repositories, Some("island"), &roles, &trust, true, &[project_grant]);
    assert!(pinned.projects.is_empty());
    assert_eq!(pinned.fixed_repositories, repositories);
}

/// Clones nothing: the clone requested below waits for its environment, which is never ready.
struct UnusedCloneRuntime;

#[async_trait]
impl CloneRuntime for UnusedCloneRuntime {
    async fn clone_and_inspect(&self, _: &str, _: &str) -> Result<Option<String>, String> {
        Err("no environment is ready, so nothing is cloned".into())
    }

    async fn inspect_existing(&self, _: &str) -> Result<Option<String>, String> {
        Err("no environment is ready, so nothing is inspected".into())
    }
}

// Behaviour (#3108): a clone requested of the engine (renewed demand for it) wakes standing
// ensure admission through the engine's clone changes, not an old Clone watch.
#[tokio::test(start_paused = true)]
async fn a_requested_clone_wakes_standing_ensure_admission() {
    let backend = flotilla_store_memory::engine_home_backend();
    let engine = flotilla_provisioning_testkit::clones::RunningClones::start("home", Arc::new(UnusedCloneRuntime));
    let mut wakes = super::super::tasks::convoy_ensure_dependency_watches(&backend, NAMESPACE, &engine.reads).await;
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
    flotilla_resources::clone_records::CloneRecords::request(
        engine.clones.as_ref(),
        &repository,
        &CloneSpec {
            repo_ref: repository.key(),
            url: "https://github.com/flotilla-org/flotilla".to_string(),
            env_ref: "host-direct".to_string(),
            path: "/workspace".to_string(),
        },
        false,
    )
    .expect("request the clone");
    tokio::time::timeout(Duration::from_secs(1), wakes.next())
        .await
        .expect("a clone request should wake promptly")
        .expect("the wakes stay open")
        .expect("a wake");
}

#[tokio::test(start_paused = true)]
async fn standing_ensure_dependency_changes_emit_wakes() {
    let home = flotilla_engine_convoys_testkit::EngineHome::open_in("home", NAMESPACE);
    let backend = home.backend().clone();
    for kind in ["Convoy", "ConvoyEnsure", "WorkflowTemplate"] {
        let mut watch = watch_resource_kind_including_replicas(&backend, NAMESPACE, kind).await.expect("dependency watch").stream;
        match kind {
            "Convoy" => {
                // Admitted, then ended: a launch is never deleted.
                let launch = home
                    .admit(
                        "standing",
                        ConvoySpec::builder().workflow_ref("workflow".to_string()).project_ref(NAMESPACE.into()).build(),
                        flotilla_engine_convoys_testkit::single_agent("coder"),
                    )
                    .await;
                watch.next().await.expect("watch should remain open").expect("convoy admit event");
                let view = home.convoy(&launch).await;
                home.convoys.path().abandon(&view, "operator", "abandoned").await.expect("end the launch");
            }
            "ConvoyEnsure" => {
                backend
                    .using::<ConvoyEnsure>(NAMESPACE)
                    .create(
                        &empty_meta("readmitted"),
                        &ConvoyEnsureSpec::builder()
                            .project_ref("project".to_string())
                            .role("governor".to_string())
                            .workflow_ref("workflow".to_string())
                            .repositories(vec![RepositoryKey("github.com/flotilla-org/flotilla".to_string())])
                            .build(),
                    )
                    .await
                    .expect("create readmitted ensure");
            }
            "WorkflowTemplate" => {
                let templates = backend.clone().using::<WorkflowTemplate>(NAMESPACE);
                let created = templates
                    .create(&empty_meta("workflow"), &WorkflowTemplateSpec::builder().vessels(Vec::new()).build())
                    .await
                    .expect("create workflow template");
                watch.next().await.expect("watch should remain open").expect("template create event");
                // A spec edit: the engine keeps a definition's typed origin, not free annotations.
                let mut edited = created.spec.clone();
                edited.repository_refs = Some(vec![RepositoryKey("github.com/flotilla-org/flotilla".to_string())]);
                templates
                    .update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &edited)
                    .await
                    .expect("update workflow template");
            }
            _ => unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(1), watch.next())
            .await
            .unwrap_or_else(|_| panic!("a {kind} change should wake promptly"))
            .expect("watch should remain open")
            .expect("dependency event");
    }
}

/// #1413: pins the cross-controller race itself — the checkout is gone
/// before the convoy's vessels are released, and the vessel requester must
/// still release them instead of wedging forever on the deleted evidence.
#[tokio::test(start_paused = true)]
async fn convoy_reclaims_vessels_after_checkout_authority_collected_the_checkout_first() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"checkout-authority-collected-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let backend = daemon.resource_backend();
    let engine = engine(&daemon);
    // The convoy, admitted and landed.
    let convoy = admit_convoy(
        &daemon,
        flotilla_engine_convoys_testkit::convoy(
            NAMESPACE,
            "raced",
            ConvoySpec::builder().workflow_ref("wf-a".to_string()).project_ref(NAMESPACE.into()).build(),
            flotilla_engine_convoys_testkit::single_agent("coder"),
        ),
    )
    .await;
    let admitted = backend.clone().using::<Convoy>(NAMESPACE).get(&convoy).await.expect("admitted convoy");
    flotilla_engine_convoys_testkit::land(&daemon.convoy_engine_path(), &admitted).await;
    let host = daemon.local_host_id().expect("local host");
    let launch = engine.vessels.request(&convoy, "implement", host.as_str());

    // The checkout authority's `OwnerTerminal` cascade already collected
    // the managed checkout: the engine holds no checkout for this convoy.
    let requests = flotilla_admission::reconcilers::vessel_requests::VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(backend.clone(), NAMESPACE)),
        daemon.lifecycle_reads(),
        Arc::new(super::super::ports::DaemonVesselReleaseGate { daemon: Arc::clone(&daemon), namespace: NAMESPACE.to_owned() }),
        flotilla_protocol::CanonicalHostId::resolved(host.as_str()),
        [],
    );
    let pass = requests.sync_once(flotilla_resources::clock::now()).await.expect("vessel requests");

    assert_eq!(pass.released, vec![launch.clone()], "the convoy's vessels must still be released after the checkout was collected first");
    assert_eq!(engine.vessels.vessels.get(&launch, None).expect("read").expect("launch").reading, LifecycleReading::Released);
}

#[tokio::test(start_paused = true)]
async fn checkout_removals_queue_across_environments_and_release_capacity() {
    struct GatedRemovalRunner {
        started: tokio::sync::mpsc::UnboundedSender<()>,
        release: tokio::sync::Semaphore,
    }
    #[async_trait]
    impl CommandRunner for GatedRemovalRunner {
        async fn run(&self, cmd: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            if cmd == "rm" {
                self.started.send(()).expect("removal observer");
                self.release.acquire().await.expect("removal gate").forget();
                Ok(String::new())
            } else {
                Err(format!("command unavailable: {cmd}"))
            }
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(1) })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }
    for concurrency in [1, 2, 3] {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("daemon.toml"), "machine_id = \"removal-queue-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
        // The engine's readings, which a running daemon always has: a removal reads its clones.
        engine_environments(&daemon);
        let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(GatedRemovalRunner { started, release: tokio::sync::Semaphore::new(0) });
        for env in ["teardown-a", "teardown-b"] {
            daemon
                .register_direct_environment_for_test(
                    EnvironmentId::new(env),
                    runner.clone(),
                    EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
                    None,
                )
                .expect("register environment");
        }
        let state = ControllerRuntimeState::new(daemon, config, passthrough_registry(), None, "local".into(), "local-env".into());
        let state = Arc::new(if concurrency == 2 {
            state
        } else {
            state.with_checkout_removal_concurrency(NonZeroUsize::new(concurrency).expect("positive limit"))
        });
        let removal = CheckoutRemoval::OrphanedWorktree { target_path: "/checkouts/queued".into() };
        let runtime = RoutingCheckoutRuntime { state: Arc::clone(&state), change_requests: None };
        assert!(runtime.remove_checkout_in("missing-environment", &removal).await.is_err(), "failed removals release capacity");
        let mut tasks = Vec::new();
        for index in 0..=concurrency {
            let env = if index % 2 == 0 { "teardown-a" } else { "teardown-b" };
            let runtime = RoutingCheckoutRuntime { state: Arc::clone(&state), change_requests: None };
            let removal = removal.clone();
            tasks.push(tokio::spawn(async move { runtime.remove_checkout_in(env, &removal).await }));
        }
        for _ in 0..concurrency {
            tokio::time::timeout(Duration::from_secs(5), observed.recv()).await.expect("admitted removals start").expect("observer open");
        }
        assert!(tokio::time::timeout(Duration::from_millis(200), observed.recv()).await.is_err(), "excess removal must queue");
        runner.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), observed.recv())
            .await
            .expect("queued removal starts after capacity is released")
            .expect("observer open");
        runner.release.add_permits(concurrency);
        for task in tasks {
            assert_eq!(task.await.expect("removal task").expect("removal succeeds"), CheckoutRemovalOutcome::Removed);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn checkout_archive_catalog_keeps_custom_roots_after_checkout_resources_are_gone() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"archive-catalog-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let state = ControllerRuntimeState::new(
        daemon,
        Arc::clone(&config),
        passthrough_registry(),
        None,
        "test-host".to_string(),
        "host-direct-test-host".to_string(),
    );
    let custom_root = temp.path().join("custom/.flotilla-archives");
    let expired = custom_root.join("expired");
    let recent = custom_root.join("recent");
    std::fs::create_dir_all(&expired).expect("expired archive");
    std::fs::create_dir(&recent).expect("recent archive");
    let past = (chrono::Utc::now() - chrono::Duration::days(20)).format("%Y%m%d%H%M.%S").to_string();
    assert!(std::process::Command::new("touch").args(["-t", &past]).arg(&expired).status().expect("age archive").success());
    state
        .register_checkout_archive_roots(
            "host-direct-test-host",
            &CheckoutRemoval::ForcedWorktree {
                clone_path: temp.path().join("custom/base").display().to_string(),
                branch: "feature/work".to_string(),
                target_path: temp.path().join("checkouts/work").display().to_string(),
            },
        )
        .await
        .expect("record archive root before removal");
    drop(state);
    let roots = load_checkout_archive_roots(&config.state_dir().as_path().join("checkout-archive-roots.json"))
        .await
        .expect("restore archive roots after controller restart");
    assert!(roots.contains(&CheckoutArchiveRoot { env_ref: "host-direct-test-host".to_string(), path: custom_root.clone() }));
    for root in roots {
        flotilla_vcs::vcs::prune_checkout_archives(&ProcessCommandRunner, &root.path, 14).await.expect("retention sweep");
    }
    assert!(!expired.exists());
    assert!(recent.exists());
}

#[tokio::test(start_paused = true)]
async fn archive_sweep_continues_to_the_next_remote_root_after_a_timeout() {
    struct SweepRunner {
        roots: StdMutex<Vec<String>>,
        finished: Notify,
    }
    #[async_trait]
    impl CommandRunner for SweepRunner {
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            panic!("sweeps must use the deadline seam");
        }
        async fn run_with_timeout(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel, _: Duration) -> Result<String, String> {
            assert_eq!(cmd, "timeout");
            let root = args[args.len() - 2].to_string();
            let mut roots = self.roots.lock().expect("roots");
            roots.push(root);
            if roots.len() == 1 {
                Err("remote sweep timed out".into())
            } else {
                self.finished.notify_one();
                Ok(String::new())
            }
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            panic!("no unbounded probe before the deadline");
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"archive-sweep-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let runner = Arc::new(SweepRunner { roots: StdMutex::new(Vec::new()), finished: Notify::new() });
    daemon
        .register_direct_environment_for_test(EnvironmentId::new("remote"), runner.clone(), EnvironmentBag::new(), None)
        .expect("register remote sweep runner");
    let task = tokio::spawn(run_checkout_archive_gc(CheckoutArchiveSweep {
        daemon,
        catalog_path: temp.path().join("missing-catalog"),
        roots: vec![
            CheckoutArchiveRoot { env_ref: "remote".into(), path: "/archives/a".into() },
            CheckoutArchiveRoot { env_ref: "remote".into(), path: "/archives/b".into() },
        ],
        retention_days: 14,
    }));
    tokio::time::timeout(Duration::from_secs(5), runner.finished.notified()).await.expect("later root swept");
    assert_eq!(*runner.roots.lock().expect("roots"), ["/archives/a", "/archives/b"]);
    task.abort();
    let _ = task.await;
}

#[tokio::test(start_paused = true)]
async fn remotely_placed_host_direct_shared_clone_provisions_across_two_stores() {
    remote_shared_clone_placement_reaches_running(false).await;
}

#[tokio::test(start_paused = true)]
async fn daemon_restart_publishes_and_logs_agent_adapter_regression() {
    let temp = TempDir::new().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"daemon-restart-adapter-regression-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let mut first_profile = manual_profile(&host_id, false);
    first_profile.available_agent_adapters = BTreeSet::from(["claude-code".to_string(), "codex".to_string()]);
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi", None).await.expect("host registration");
    apply_host_heartbeat_with_credentials(
        &daemon,
        NAMESPACE,
        &first_profile,
        None,
        &DaemonHealthIdentity {
            generation: Some("generation-1".to_string()),
            version: "1.0.0".to_string(),
            started_at: flotilla_resources::clock::now() - chrono::Duration::minutes(1),
        },
        &RuntimeHealth::default(),
    )
    .await
    .expect("first generation heartbeat");
    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
    let first_host = hosts.get(&host_id).await.expect("first generation host");
    let mut legacy_status = first_host.status.expect("first generation status");
    legacy_status.agent_adapter_baseline = None;
    hosts
        .update_status(&host_id, &first_host.metadata.resource_version, &legacy_status)
        .await
        .expect("simulate status written before the baseline field existed");

    let mut restarted_profile = first_profile.clone();
    restarted_profile.available_agent_adapters.remove("claude-code");
    let runtime_health = RuntimeHealth::default();
    let second_health = DaemonHealthIdentity {
        generation: Some("generation-2".to_string()),
        version: "1.0.0".to_string(),
        started_at: flotilla_resources::clock::now(),
    };
    let (logging, logs) = flotilla_log_testkit::capture(tracing::Level::WARN);
    {
        let _guard = tracing::dispatcher::set_default(&logging);
        apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &restarted_profile, None, &second_health, &runtime_health)
            .await
            .expect("restarted generation heartbeat");
    }

    let status = daemon
        .resource_backend()
        .using::<Host>(NAMESPACE)
        .get(&host_id)
        .await
        .expect("host after restart")
        .status
        .expect("host status after restart");
    assert!(!status.ready, "capability regression should degrade the host");
    assert_eq!(status.conditions.len(), 1);
    assert_eq!(status.conditions[0].condition_type, "CapabilityRegression");
    assert_eq!(status.conditions[0].reason, "AgentAdaptersMissing");
    assert!(status.conditions[0].message.contains("claude-code"));
    assert_eq!(status.agent_adapter_baseline, Some(first_profile.available_agent_adapters.clone()));

    let logs = logs.text();
    assert!(logs.contains("host capabilities regressed across daemon restart"), "missing capability regression warning: {logs}");
    assert!(logs.contains("claude-code"), "warning should name the missing adapter: {logs}");

    let (logging, logs) = flotilla_log_testkit::capture(tracing::Level::WARN);
    {
        let _guard = tracing::dispatcher::set_default(&logging);
        apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &restarted_profile, None, &second_health, &runtime_health)
            .await
            .expect("same generation heartbeat");
    }
    let same_generation_logs = logs.text();
    assert!(
        !same_generation_logs.contains("host capabilities regressed across daemon restart"),
        "same generation heartbeat should not repeat the warning: {same_generation_logs}"
    );

    let third_runtime_health = RuntimeHealth::default();
    let (logging, logs) = flotilla_log_testkit::capture(tracing::Level::WARN);
    {
        let _guard = tracing::dispatcher::set_default(&logging);
        apply_host_heartbeat_with_credentials(
            &daemon,
            NAMESPACE,
            &restarted_profile,
            None,
            &DaemonHealthIdentity {
                generation: Some("generation-3".to_string()),
                version: "1.0.0".to_string(),
                started_at: flotilla_resources::clock::now() + chrono::Duration::minutes(1),
            },
            &third_runtime_health,
        )
        .await
        .expect("third generation heartbeat");
    }
    let third_status = daemon
        .resource_backend()
        .using::<Host>(NAMESPACE)
        .get(&host_id)
        .await
        .expect("host after repeated restart")
        .status
        .expect("host status after repeated restart");
    assert!(!third_status.ready, "a repeated restart must not absorb the capability regression into its baseline");
    assert_eq!(third_status.conditions.len(), 1);
    assert_eq!(third_status.conditions[0].reason, "AgentAdaptersMissing");
    assert!(third_status.conditions[0].message.contains("claude-code"));
    assert_eq!(third_status.agent_adapter_baseline, Some(first_profile.available_agent_adapters));
    let third_logs = logs.text();
    assert!(
        third_logs.contains("host capabilities regressed across daemon restart"),
        "each regressed daemon generation should warn: {third_logs}"
    );
}

#[tokio::test]
async fn checkout_authority_observes_replicated_landing_and_convoy_authority_settles_from_evidence() {
    let temp = TempDir::new().expect("tempdir");
    let authority_root = flotilla_protocol::NodeId::new("authority-root");
    let authority = flotilla_store_memory::engine_home_backend().with_local_root(authority_root);
    // Checkout host B is another home, its engine serving its own records.
    let checkout_home = flotilla_provisioning_testkit::EngineBackend::open_in("checkout-root", NAMESPACE);
    let checkout_host = checkout_home.backend.clone();
    let authority_config_base = temp.path().join("authority-config");
    fs::create_dir_all(&authority_config_base).expect("config directory");
    fs::write(authority_config_base.join("daemon.toml"), "machine_id = \"checkout-authority-evidence-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(authority_config_base));
    let authority_daemon = daemon_with_backend(Vec::new(), config, authority.clone()).await;
    let repository_key = flotilla_resources::RepositoryKey("repo-a".to_string());

    // A's engine readings, which read every home's records in its decided store.
    engine(&authority_daemon);
    // Checkout host B's log reaches A's decided store.
    let decided = authority_daemon.decided_store();
    let at_checkout_host = flotilla_provisioning_testkit::CheckoutFixture::over("checkout-root", checkout_home.service.store());

    // The convoy, admitted at authority A, its coder owing a decision ledger.
    let status = ConvoyStatus {
        workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(flotilla_resources::ExitDeclaration::standard_table()),
            turn_delivery: Default::default(),
            vessels: vec![VesselRequirement::builder()
                .name("work".to_string())
                .crew(vec![CrewSpec::builder()
                    .role("coder".to_string())
                    .promises(vec![flotilla_resources::TemplatePromise::decision_ledger("coder")])
                    .source(CrewSource::Tool { command: "true".to_string() })
                    .build()])
                .build()],
        }),
        observed_workflow_ref: Some("review-and-fix".to_string()),
        ..Default::default()
    };
    let spec = ConvoySpec::builder()
        .workflow_ref("review-and-fix".to_string())
        .project_ref(NAMESPACE.into())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(repository_key.clone())
            .source_ref("feature/cross-host".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .adopted_checkout_refs(BTreeMap::from([(repository_key.clone(), "checkout-b".to_string())]))
        .build();
    let convoy_name = admit_convoy(&authority_daemon, flotilla_engine_convoys_testkit::convoy(NAMESPACE, "cross-host", spec, status)).await;
    // Its branch's change request is linked and its work done: it lands once evidence settles it.
    let convoys = authority_daemon.resource_backend().using::<Convoy>(NAMESPACE);
    let engine_path = authority_daemon.convoy_engine_path();
    let engine = &engine_path;
    let admitted = convoys.get(&convoy_name).await.expect("admitted convoy");

    engine
        .link(
            &admitted,
            &flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: flotilla_protocol::provider_data::IssueSource {
                    service: "github.com".to_string(),
                    scope: "flotilla-org/flotilla".to_string(),
                },
                id: "1367".to_string(),
            },
            flotilla_resources::SubjectDiscoverySource::Branch,
            "operator",
        )
        .await
        .expect("link the branch's change request");
    engine
        .keep_by_verdict(
            &admitted,
            &flotilla_resources::convoy::engine::VerdictScope { vessel: "work".to_string(), role: None },
            "operator",
            "done",
        )
        .await
        .expect("work done");
    flotilla_store::note_convoy(
        &engine_path,
        &convoys,
        &convoy_name,
        &flotilla_resources::ConvoyStatusPatch::RecordBranchSubjectScan { at: flotilla_resources::clock::now() },
    )
    .await
    .expect("branch scanned");
    assert_eq!(convoys.get(&convoy_name).await.expect("landing convoy").status.expect("status").phase, ConvoyPhase::Landing);
    // A's launch reaches B, which holds its adopted checkout for it.
    flotilla_provisioning_testkit::replicate_home(&decided, decided.home(), &checkout_home.service);

    let git_repo = TestGitRepo::init(temp.path().join("checkout")).with_initial_commit();
    let spec = CheckoutSpec::Observed(ResourceObservedCheckoutSpec {
        r#ref: "feature/cross-host".to_string(),
        path: git_repo.path().display().to_string(),
        repo_ref: repository_key,
        host_ref: "checkout-host".to_string(),
        is_main: false,
    });
    let launch = at_checkout_host.put_ready(
        "checkout-b",
        &spec,
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(convoy_name.clone())),
    );

    // #3710: the PR observation is replicated before checkout inspection; gh
    // supplies only merge time/base metadata, never the landing state or SHA.
    publish_merged_change_request(&checkout_host, 1367, "checkout-root").await;
    let runner = Arc::new(MergedPrProcessRunner::new(1367));
    let vcs = test_controller_vcs(runner.clone(), &git_repo.path().to_string_lossy());
    let records = checkout_host.using::<flotilla_resources::ChangeRequest>(NAMESPACE);
    let record = records.list().await.expect("records").items.into_iter().find(|record| record.spec.number == 1367).expect("merged record");
    let mut status = record.status.expect("merged status");
    status.head_sha.value =
        Some(vcs.read_repository(git_repo.path(), flotilla_vcs::vcs::RepositoryRead::HeadRevision).await.expect("HEAD").trim().to_string());
    records.update_status(&record.metadata.name, &record.metadata.resource_version, &status).await.expect("replicated head");
    // Checkout host B inspects its checkout against the change request the convoy binds, as its
    // Checkout controller does, and records what it found.
    let inspector =
        CheckoutControllerRuntime::new(runner, Some(vcs), Some(ChangeRequestObservations::new(checkout_host.clone(), NAMESPACE)));
    let observed = inspector
        .inspect_at(
            &git_repo.path().display().to_string(),
            &spec,
            Some("https://github.com/flotilla-org/flotilla"),
            flotilla_vcs::checkout_integration::RecordedAssociation {
                change_request_id: Some("1367"),
                complete: true,
                observe_remote_ref: false,
            },
            None,
        )
        .await
        .expect("observe integration on checkout authority");
    assert_eq!(observed.landed.value, ConditionValue::True, "checkout authority B must observe the merged change request");
    {
        use flotilla_resources::checkout_records::CheckoutRecords as _;
        at_checkout_host
            .checkouts
            .inspected(&launch, flotilla_resources::checkout_records::Inspection::of_status(&observed))
            .expect("record authority-local observation");
    }

    // B's checkout and its CR observations, decided records homed at B, reach A as B's log does.
    flotilla_provisioning_testkit::replicate_home(&checkout_home.service, "checkout-root", &decided);
    let reconciler = ConvoyReconciler::new(
        authority_daemon.lifecycle_reads(),
        authority_daemon.host_records(),
        Arc::clone(&engine_path),
        authority_daemon.crew_message_path(),
    )
    .with_change_requests(
        authority.including_replicas::<flotilla_resources::ChangeRequest>(NAMESPACE),
        authority_daemon.change_request_stale_after(),
    );
    // Fresh replicated evidence settles it: the reconciler observes the branch's change request,
    // one edge a pass as the controller runs it, then delivers the launch.
    let mut current = convoys.get(&convoy_name).await.expect("get Landing convoy");
    for _ in 0..4 {
        reconciler.prepare(&current).await.expect("consume replicated checkout evidence");
        current = convoys.get(&convoy_name).await.expect("get convoy");
        if engine.delivered(&current).is_some() {
            break;
        }
    }
    if engine.delivered(&current).is_none() {
        let explained = authority_daemon
            .execute_query(
                Command::builder()
                    .action(CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.to_string()), name: convoy_name.clone() })
                    .build(),
                uuid::Uuid::new_v4(),
            )
            .await;
        panic!("fresh replicated evidence should deliver the convoy: {explained:?}");
    }
    assert_eq!(convoys.get(&convoy_name).await.expect("settled convoy").status.expect("convoy status").phase, ConvoyPhase::Landed);
    {
        assert!(
            authority_daemon
                .lifecycle_reads()
                .checkouts()
                .and_then(|checkouts| checkouts.here())
                .expect("authority-local checkouts")
                .is_empty(),
            "convoy authority A must not re-author or write the replicated checkout",
        );
    }
}

#[tokio::test]
async fn in_memory_stage4a_flow_reaches_running_and_completes_convoy() {
    let temp = TempDir::new().expect("tempdir");
    let repo_default_dir = temp.path().join("flotilla-repos");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"in-memory-stage4a-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    std::fs::create_dir_all(config.state_dir()).expect("state dir");
    let runner = Arc::new(Stage4aGitRunner { inner: Arc::new(NoPrProcessRunner), upstream: repo.clone() });
    let daemon = daemon_with_backend_runner_and_change_requests(
        vec![repo.clone()],
        Arc::clone(&config),
        flotilla_store_memory::engine_home_backend(),
        runner,
        Some(Arc::new(FakeChangeRequest::new())),
    )
    .await;
    run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, CompletionAction::Retain).await;
}

#[tokio::test]
async fn deleting_a_live_convoy_ends_it_and_releases_its_vessel_checkout_and_sessions() {
    let temp = TempDir::new().expect("tempdir");
    let repo_default_dir = temp.path().join("flotilla-repos");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"delete-live-convoy-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    let daemon = daemon_with_backend_runner_and_change_requests(
        vec![repo.clone()],
        Arc::clone(&config),
        flotilla_store_memory::engine_home_backend(),
        Arc::new(Stage4aGitRunner { inner: Arc::new(NoPrProcessRunner), upstream: repo.clone() }),
        Some(Arc::new(FakeChangeRequest::new())),
    )
    .await;
    run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, CompletionAction::DeleteLive).await;
}

#[tokio::test]
async fn passing_teardown_gate_removes_the_managed_worktree_path() {
    let temp = TempDir::new().expect("tempdir");
    let repo_default_dir = temp.path().join("flotilla-repos");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"passing-teardown-gate-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    let requests = Arc::new(FakeChangeRequest::new());
    requests
        .add_change_requests(vec![(
            "884".into(),
            ProviderChangeRequest {
                title: "Claimed result".into(),
                branch: "claim-only".into(),
                status: flotilla_protocol::ChangeRequestStatus::Merged,
                body: None,
                provider_name: "fake".into(),
                provider_display_name: "Fake".into(),
            },
        )])
        .await;
    let daemon = daemon_with_backend_runner_and_change_requests(
        vec![repo.clone()],
        Arc::clone(&config),
        flotilla_store_memory::engine_home_backend(),
        Arc::new(Stage4aGitRunner { inner: Arc::new(MergedPrProcessRunner::new(884)), upstream: repo.clone() }),
        Some(requests),
    )
    .await;

    run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, CompletionAction::Delete).await;
}

/// A convoy whose work completes naming change request 884, read by the forge observer in
/// `state`, its branch pushed when `pushed`: what became of its checkout.
async fn checkout_release_after_change_request(
    state: flotilla_resources::ObservedChangeRequestState,
    pushed: bool,
    untracked: bool,
    then: OnHold,
) -> ReleaseSeen {
    checkout_released(state, pushed, untracked, then, Release::Prompt).await
}

/// As [`checkout_release_after_change_request`], its release reaching the checkout as `release`.
async fn checkout_released(
    state: flotilla_resources::ObservedChangeRequestState,
    pushed: bool,
    untracked: bool,
    then: OnHold,
    release: Release,
) -> ReleaseSeen {
    let temp = TempDir::new().expect("tempdir");
    let repo_default_dir = temp.path().join("flotilla-repos");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"change-request-release-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    let daemon = daemon_with_backend_runner_and_change_requests(
        vec![repo.clone()],
        Arc::clone(&config),
        flotilla_store_memory::engine_home_backend(),
        Arc::new(Stage4aGitRunner { inner: Arc::new(MergedPrProcessRunner::new(884)), upstream: repo.clone() }),
        Some(Arc::new(FakeChangeRequest::new())),
    )
    .await;
    run_stage4a_flow_reaches_running_and_completes_convoy(
        daemon,
        config,
        repo_default_dir,
        CompletionAction::ChangeRequest { state, pushed, untracked, then, release },
    )
    .await
    .expect("what became of the checkout")
}

// F5 (run 3): the checkout is held for the convoy's launch, `<name>#<n>`, and its change request
// is found through that launch's convoy, so a merged one lands it. Its commits were never pushed,
// so without the change request it would be archived as forced removal.
#[tokio::test]
async fn a_merged_change_request_releases_its_checkout_as_landed() {
    let seen =
        checkout_release_after_change_request(flotilla_resources::ObservedChangeRequestState::Merged, false, false, OnHold::Nothing).await;
    assert_eq!(seen.convoy, Some(ConvoyPhase::Landed), "{seen:?}");
    assert_eq!(seen.landed, Some(true), "{seen:?}");
    assert!(matches!(seen.reading, LifecycleReading::Released), "{seen:?}");
    assert!(!seen.on_disk, "{seen:?}");
    assert_eq!(seen.archives, 0, "a landed checkout is removed, not archived: {seen:?}");
}

// The convoy ends, its change request's observer has its chance to collect the observation, and
// only then is the checkout released: the convoy's demand is held until the checkout it holds is
// released, so the merged change request is still observed and the checkout is removed as landed.
#[tokio::test]
async fn a_checkout_released_after_its_convoy_ended_is_still_judged_against_its_merged_change_request() {
    let seen =
        checkout_released(flotilla_resources::ObservedChangeRequestState::Merged, false, false, OnHold::Nothing, Release::AfterCollection)
            .await;
    assert_eq!(seen.convoy, Some(ConvoyPhase::Landed), "{seen:?}");
    assert_eq!(seen.landed, Some(true), "{seen:?}");
    assert!(matches!(seen.reading, LifecycleReading::Released), "{seen:?}");
    assert!(!seen.on_disk, "{seen:?}");
    assert_eq!(seen.archives, 0, "a landed checkout is removed, not archived: {seen:?}");
}

// A change request closed unmerged did not land the work: its checkout is kept as an archive,
// never removed as landed.
#[tokio::test]
async fn a_change_request_closed_unmerged_archives_its_checkout() {
    let seen =
        checkout_release_after_change_request(flotilla_resources::ObservedChangeRequestState::Closed, false, false, OnHold::Nothing).await;
    assert_eq!(seen.convoy, Some(ConvoyPhase::Landed), "{seen:?}");
    assert_eq!(seen.landed, Some(false), "{seen:?}");
    assert!(seen.landed_details.iter().any(|detail| detail.contains("#884")), "the change request was read: {seen:?}");
    assert_eq!(seen.pushed, Some(false), "{seen:?}");
    assert!(matches!(seen.reading, LifecycleReading::Released), "{seen:?}");
    assert!(!seen.on_disk, "{seen:?}");
    assert_eq!(seen.archives, 1, "{seen:?}");
}

// An unmerged branch pushed to origin under its own name is pushed, though it is ahead of the
// origin/main it was made from: its clean checkout is removed as clean and pushed, keeping the
// branch, and nothing is archived.
#[tokio::test]
async fn an_unmerged_pushed_clean_branch_is_removed_as_pushed_not_archived() {
    let seen =
        checkout_release_after_change_request(flotilla_resources::ObservedChangeRequestState::Closed, true, false, OnHold::Nothing).await;
    assert_eq!(seen.convoy, Some(ConvoyPhase::Landed), "{seen:?}");
    assert_eq!(seen.landed, Some(false), "{seen:?}");
    assert_eq!(seen.pushed, Some(true), "{seen:?}");
    assert!(matches!(seen.reading, LifecycleReading::Released), "{seen:?}");
    assert!(!seen.on_disk, "{seen:?}");
    assert!(seen.branch_kept, "{seen:?}");
    assert_eq!(seen.archives, 0, "a pushed checkout is not archived: {seen:?}");
}

/// A merged change request's checkout left with an untracked file is landed, but its landed
/// removal refuses a checkout that is not clean: a person is told, naming the file, and the
/// convoy's explanation shows the hold. Then `then` acts.
async fn held_landed_checkout(then: OnHold) -> ReleaseSeen {
    let seen = checkout_release_after_change_request(flotilla_resources::ObservedChangeRequestState::Merged, false, true, then).await;
    assert_eq!(seen.convoy, Some(ConvoyPhase::Landed), "{seen:?}");
    assert_eq!(seen.landed, Some(true), "{seen:?}");
    let told = seen.told.as_deref().unwrap_or_else(|| panic!("a person is told the release is held: {seen:?}"));
    assert!(told.contains("left-over.pyc"), "the reason names the file: {seen:?}");
    assert_eq!(seen.explained.as_deref(), Some(told), "the explanation shows the hold: {seen:?}");
    seen
}

// Cleaned up and rechecked, the held checkout is released as landed: removed, nothing archived,
// and the person's attention withdrawn.
#[tokio::test]
async fn a_held_landed_checkout_cleaned_and_rechecked_is_released() {
    let seen = held_landed_checkout(OnHold::CleanAndRecheck).await;
    assert!(matches!(seen.reading, LifecycleReading::Released), "{seen:?}");
    assert!(!seen.on_disk, "{seen:?}");
    assert_eq!(seen.archives, 0, "{seen:?}");
    assert_eq!(seen.still_told, 0, "the attention is withdrawn: {seen:?}");
}

// Ruled archived, the held checkout is archived and removed, and the person's attention withdrawn.
#[tokio::test]
async fn a_held_landed_checkout_ruled_archived_is_archived_and_released() {
    let seen = held_landed_checkout(OnHold::Archive).await;
    assert!(matches!(seen.reading, LifecycleReading::Released), "{seen:?}");
    assert!(!seen.on_disk, "{seen:?}");
    assert_eq!(seen.archives, 1, "{seen:?}");
    assert_eq!(seen.still_told, 0, "the attention is withdrawn: {seen:?}");
}

#[test]
fn crew_provisioning_recovers_lost_session_after_in_process_daemon_restart_and_runs_handoffs() {
    // This full provisioning scenario builds a large async state machine.
    // Give its test thread enough stack without changing daemon workers.
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("test runtime")
                .block_on(crew_provisioning_recovery_scenario());
        })
        .expect("test thread")
        .join()
        .expect("test thread completed");
}

// The daemon's Checkout controller judges a released checkout landed against its change request's
// replicated observation. On a Forgejo forge that record is stored under the forge's id, which the
// checkout's repository URL reaches only through the declared Forge.
#[tokio::test]
async fn the_checkout_controller_finds_a_forgejo_change_request_landed() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"forgejo-landing-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    engine_environments(&daemon);
    daemon
        .register_direct_environment_for_test(
            EnvironmentId::new("landing-env"),
            Arc::new(MergedPrProcessRunner::new(4)),
            EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
            None,
        )
        .expect("register environment");
    let backend = daemon.resource_backend();
    declare_mapping_forge(&backend, NAMESPACE).await;
    let git_repo = TestGitRepo::init(temp.path().join("checkout")).with_initial_commit();
    let head = ProcessCommand::new("git").arg("-C").arg(git_repo.path()).args(["rev-parse", "HEAD"]).output().expect("HEAD");
    let head = String::from_utf8(head.stdout).expect("utf-8 HEAD").trim().to_owned();

    // The merged change request, as the forge observer records it: under the forge id `lab`.
    let forges =
        backend.using::<Forge>(NAMESPACE).list().await.expect("forges").items.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_change_request_address("lab", "robert/engine-trial", 4, &forges).expect("address"),
    )
    .expect("record name");
    let records = backend.using::<flotilla_resources::ChangeRequest>(NAMESPACE);
    let spec = flotilla_resources::ChangeRequestSpec::builder()
        .service("lab".to_string())
        .scope("robert/engine-trial".to_string())
        .number(4)
        .observing_authority("test-host".to_string())
        .build();
    let created = records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.expect("change request");
    let observed_at = flotilla_resources::clock::now();
    let status = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
        head_sha: flotilla_resources::Observation::known(head, observed_at),
        checks: flotilla_resources::Observation::unknown(observed_at),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
        },
        mergeable: flotilla_resources::Observation::unknown(observed_at),
    };
    records.update_status(&name, &created.metadata.resource_version, &status).await.expect("merged");

    let state = Arc::new(ControllerRuntimeState::new(daemon, config, passthrough_registry(), None, "local".into(), "local-env".into()));
    let path = git_repo.path().display().to_string();
    let checkout = CheckoutSpec::Observed(ResourceObservedCheckoutSpec {
        r#ref: "main".to_string(),
        path: path.clone(),
        repo_ref: RepositoryKey(flotilla_resources::repo_key("https://lab.example/robert/engine-trial")),
        host_ref: "local".to_string(),
        is_main: false,
    });
    let integration = checkout_routing(&state)
        .runtime_for("landing-env", &path)
        .await
        .expect("checkout runtime")
        .inspect_at(
            &path,
            &checkout,
            Some("https://lab.example/robert/engine-trial"),
            flotilla_vcs::checkout_integration::RecordedAssociation {
                change_request_id: Some("4"),
                complete: true,
                observe_remote_ref: false,
            },
            None,
        )
        .await
        .expect("inspect");
    assert_eq!(integration.landed.value, ConditionValue::True, "{:?}", integration.landed);
}

use flotilla_orchestration_testkit::InProcessDiscoveryExt;
