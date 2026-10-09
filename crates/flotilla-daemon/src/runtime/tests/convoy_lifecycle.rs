use super::*;

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

#[tokio::test]
async fn standing_ensure_dependency_changes_emit_wakes() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    for kind in ["Clone", "Convoy", "ConvoyEnsure", "WorkflowTemplate"] {
        let mut watch = if kind == "Clone" {
            watch_resource_kind(&backend, NAMESPACE, kind).await
        } else {
            watch_resource_kind_including_replicas(&backend, NAMESPACE, kind).await
        }
        .expect("dependency watch")
        .stream;
        match kind {
            "Clone" => {
                let clones = backend.clone().using::<Clone>(NAMESPACE);
                clones
                    .create(
                        &empty_meta("renewed-demand"),
                        &CloneSpec {
                            repo_ref: RepositoryKey("github.com/flotilla-org/flotilla".to_string()),
                            url: "https://github.com/flotilla-org/flotilla".to_string(),
                            env_ref: "host-direct".to_string(),
                            path: "/workspace".to_string(),
                        },
                    )
                    .await
                    .expect("create renewed clone demand");
                watch.next().await.expect("watch should remain open").expect("clone create event");
                flotilla_resources::apply_status_patch(&clones, "renewed-demand", &flotilla_resources::CloneStatusPatch::MarkCloning)
                    .await
                    .expect("renew clone demand");
            }
            "Convoy" => {
                let convoys = backend.clone().using::<Convoy>(NAMESPACE);
                convoys
                    .create(&empty_meta("standing"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
                    .await
                    .expect("create convoy");
                watch.next().await.expect("watch should remain open").expect("convoy create event");
                convoys.delete("standing").await.expect("delete convoy");
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
                let mut updated_meta = InputMeta::from(&created.metadata);
                updated_meta.annotations.insert("example.com/revision".to_string(), "2".to_string());
                templates.update(&updated_meta, &created.metadata.resource_version, &created.spec).await.expect("update workflow template");
            }
            _ => unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("dependency change should wake promptly")
            .expect("watch should remain open")
            .expect("dependency event");
    }
}

#[tokio::test]
async fn repeated_reclaim_refusal_with_changing_reason_raises_demand() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"reclaim-refusal-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(vec![], config, git_process_discovery(false), HostName::local()).await;
    let convoys = daemon.resource_backend().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name("stuck-reclaim".to_string()).build(),
            &ConvoySpec::builder()
                .workflow_ref("scratch".to_string())
                .adopted_checkout_refs(BTreeMap::from([(
                    flotilla_resources::RepositoryKey("repo".to_string()),
                    "checkout-orphan".to_string(),
                )]))
                .build(),
        )
        .await
        .expect("create terminal convoy");
    let runtime = DaemonConvoyTeardownRuntime::new(Arc::clone(&daemon));

    runtime.verify_reclaim(&convoy, &[]).await.expect_err("missing checkout must refuse reclaim");
    let mut changed_reason_convoy = convoy.clone();
    changed_reason_convoy
        .spec
        .adopted_checkout_refs
        .insert(flotilla_resources::RepositoryKey("repo".to_string()), "checkout-still-orphaned".to_string());
    runtime.verify_reclaim(&changed_reason_convoy, &[]).await.expect_err("changed missing checkout must still refuse reclaim");
    assert!(daemon.resource_backend().using::<Demand>(NAMESPACE).list().await.expect("list demands").items.is_empty());

    runtime.verify_reclaim(&changed_reason_convoy, &[]).await.expect_err("persistent missing checkout must refuse reclaim");

    let demands = daemon.resource_backend().using::<Demand>(NAMESPACE).list().await.expect("list demands").items;
    assert_eq!(demands.len(), 1);
    assert!(
        demands[0]
            .metadata
            .annotations
            .get(RECLAIM_REFUSAL_REASON_ANNOTATION)
            .is_some_and(|reason| reason.contains("checkout-still-orphaned") && reason.contains("missing checkout integration evidence")),
        "demand must preserve the checkout-specific refusal: {:?}",
        demands[0].metadata.annotations
    );
}

/// #1413 leg 1: once the convoy is Landed, the checkout authority's
/// `OwnerTerminal` cascade may collect the managed checkout before the
/// convoy's own reclaim pass runs. That sanctioned absence is evidence of
/// completed reclaim, never "missing checkout integration evidence" —
/// outside the sanction (Failed) absence must keep refusing.
#[tokio::test]
async fn landed_convoy_reclaim_accepts_sanctioned_checkout_absence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"landed-reclaim-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(vec![], config, git_process_discovery(false), HostName::local()).await;
    let convoys = daemon.resource_backend().using::<Convoy>(NAMESPACE);
    let created = convoys
        .create(&InputMeta::builder().name("raced".to_string()).build(), &ConvoySpec::builder().workflow_ref("wf-a".to_string()).build())
        .await
        .expect("create convoy");
    convoys
        .update_status("raced", &created.metadata.resource_version, &landed_status_with_placed_checkout("checkout-raced"))
        .await
        .expect("mark convoy Landed");
    let convoy = convoys.get("raced").await.expect("get convoy");
    let runtime = DaemonConvoyTeardownRuntime::new(Arc::clone(&daemon));

    runtime.verify_reclaim(&convoy, &[]).await.expect("absence of a collected checkout must pass the Landed reclaim gate");

    let checkouts = daemon.resource_backend().using::<ResourceCheckout>(NAMESPACE);
    checkouts
        .create(
            &InputMeta::builder()
                .name("checkout-raced".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "raced".to_string())]))
                .finalizers(vec!["checkout".to_string()])
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Managed),
            &ResourceCheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey("repo-a".to_string()),
                env_ref: "host-direct-test".to_string(),
                r#ref: "feature/raced".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/checkouts/raced".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("create managed checkout");
    checkouts.delete("checkout-raced").await.expect("request checkout deletion");
    let deleting = checkouts.get("checkout-raced").await.expect("deleting checkout persists under its finalizer");
    assert!(deleting.metadata.deletion_timestamp.is_some(), "finalizer must hold the deleting checkout");
    runtime.verify_reclaim(&convoy, &[deleting]).await.expect("a checkout already being collected must not refuse reclaim");

    let mut failed = convoy.clone();
    failed.status.as_mut().expect("status").phase = ConvoyPhase::Failed;
    let refusal = runtime.verify_reclaim(&failed, &[]).await.expect_err("unsanctioned absence must still refuse reclaim");
    assert!(refusal.contains("missing checkout integration evidence"), "refusal must keep naming the missing evidence: {refusal}");
}

/// #1413: pins the cross-controller race itself — the checkout is gone
/// before the convoy's reclaim pass, and the convoy must still reclaim its
/// vessels instead of wedging forever on the deleted evidence.
#[tokio::test]
async fn convoy_reclaims_vessels_after_checkout_authority_collected_the_checkout_first() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"checkout-authority-collected-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(vec![], config, git_process_discovery(false), HostName::local()).await;
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    let created = convoys
        .create(&InputMeta::builder().name("raced".to_string()).build(), &ConvoySpec::builder().workflow_ref("wf-a".to_string()).build())
        .await
        .expect("create convoy");
    convoys
        .update_status("raced", &created.metadata.resource_version, &landed_status_with_placed_checkout("checkout-raced"))
        .await
        .expect("mark convoy Landed");
    vessels
        .create(
            &InputMeta::builder()
                .name("raced-implement".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "raced".to_string())]))
                .build(),
            &VesselSpec {
                convoy_ref: "raced".to_string(),
                vessel_name: "implement".to_string(),
                placement_policy_ref: "host-direct-test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create vessel");

    // The checkout authority's `OwnerTerminal` cascade already collected
    // the managed checkout: no checkout record exists for this pass.
    let reconciler = ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(NAMESPACE))
        .with_vessels(vessels)
        .with_checkouts(backend.clone().using::<ResourceCheckout>(NAMESPACE))
        .with_teardown_runtime(Arc::new(DaemonConvoyTeardownRuntime::new(Arc::clone(&daemon))));
    let convoy = convoys.get("raced").await.expect("get convoy");
    let deps = reconciler.prepare(&convoy).await.expect("fetch dependencies");
    let outcome = reconciler.reconcile(&convoy, &deps, Utc::now());

    assert!(
        outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "raced-implement")),
        "convoy must still reclaim its vessels after the checkout was collected first: {:?}",
        outcome.actuations
    );
    assert_eq!(outcome.requeue_after, None, "a successful reclaim pass must not keep requeueing");
}

#[tokio::test]
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

#[tokio::test]
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
        flotilla_core::vcs::prune_checkout_archives(&ProcessCommandRunner, &root.path, 14).await.expect("retention sweep");
    }
    assert!(!expired.exists());
    assert!(recent.exists());
}

#[tokio::test]
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
    let task = tokio::spawn(run_checkout_archive_gc(
        daemon.resource_backend(),
        NAMESPACE.into(),
        CheckoutArchiveSweep {
            daemon,
            catalog_path: temp.path().join("missing-catalog"),
            roots: vec![
                CheckoutArchiveRoot { env_ref: "remote".into(), path: "/archives/a".into() },
                CheckoutArchiveRoot { env_ref: "remote".into(), path: "/archives/b".into() },
            ],
            retention_days: 14,
        },
    ));
    tokio::time::timeout(Duration::from_secs(5), runner.finished.notified()).await.expect("later root swept");
    assert_eq!(*runner.roots.lock().expect("roots"), ["/archives/a", "/archives/b"]);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn remotely_placed_host_direct_shared_clone_provisions_across_two_stores() {
    remote_shared_clone_placement_reaches_running(false).await;
}

#[tokio::test]
async fn daemon_restart_publishes_and_logs_agent_adapter_regression() {
    let temp = TempDir::new().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"daemon-restart-adapter-regression-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let mut first_profile = manual_profile(&host_id, false);
    first_profile.available_agent_adapters = BTreeSet::from(["claude-code".to_string(), "codex".to_string()]);
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi").await.expect("host registration");
    apply_host_heartbeat_with_credentials(
        &daemon,
        NAMESPACE,
        &first_profile,
        None,
        &DaemonHealthIdentity {
            generation: Some("generation-1".to_string()),
            version: "1.0.0".to_string(),
            started_at: Utc::now() - chrono::Duration::minutes(1),
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
    let second_health =
        DaemonHealthIdentity { generation: Some("generation-2".to_string()), version: "1.0.0".to_string(), started_at: Utc::now() };
    let log_output = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let writer = LogCaptureWriter(Arc::clone(&log_output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
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

    let logs = String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be utf-8");
    assert!(logs.contains("host capabilities regressed across daemon restart"), "missing capability regression warning: {logs}");
    assert!(logs.contains("claude-code"), "warning should name the missing adapter: {logs}");

    log_output.lock().expect("log output lock should be healthy").clear();
    {
        let writer = LogCaptureWriter(Arc::clone(&log_output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &restarted_profile, None, &second_health, &runtime_health)
            .await
            .expect("same generation heartbeat");
    }
    let same_generation_logs =
        String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be utf-8");
    assert!(
        !same_generation_logs.contains("host capabilities regressed across daemon restart"),
        "same generation heartbeat should not repeat the warning: {same_generation_logs}"
    );

    let third_runtime_health = RuntimeHealth::default();
    log_output.lock().expect("log output lock should be healthy").clear();
    {
        let writer = LogCaptureWriter(Arc::clone(&log_output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        apply_host_heartbeat_with_credentials(
            &daemon,
            NAMESPACE,
            &restarted_profile,
            None,
            &DaemonHealthIdentity {
                generation: Some("generation-3".to_string()),
                version: "1.0.0".to_string(),
                started_at: Utc::now() + chrono::Duration::minutes(1),
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
    let third_logs =
        String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be utf-8");
    assert!(
        third_logs.contains("host capabilities regressed across daemon restart"),
        "each regressed daemon generation should warn: {third_logs}"
    );
}

#[tokio::test]
async fn checkout_authority_observes_replicated_landing_and_convoy_authority_settles_from_evidence() {
    let temp = TempDir::new().expect("tempdir");
    let authority_root = flotilla_protocol::NodeId::new("authority-root");
    let checkout_root = flotilla_protocol::NodeId::new("checkout-root");
    let authority = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default()).with_local_root(authority_root.clone());
    let checkout_host = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default()).with_local_root(checkout_root.clone());
    let authority_config_base = temp.path().join("authority-config");
    fs::create_dir_all(&authority_config_base).expect("config directory");
    fs::write(authority_config_base.join("daemon.toml"), "machine_id = \"checkout-authority-evidence-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(authority_config_base));
    let authority_daemon = daemon_with_backend(Vec::new(), config, authority.clone()).await;
    let repository_key = flotilla_resources::RepositoryKey("repo-a".to_string());

    let convoys = authority.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &flotilla_resources::InputMeta::builder().name("cross-host".to_string()).build(),
            &ConvoySpec::builder()
                .workflow_ref("review-and-fix".to_string())
                .repositories(vec![ConvoyRepositorySpec::builder()
                    .url("https://github.com/flotilla-org/flotilla".to_string())
                    .repo_ref(repository_key.clone())
                    .source_ref("feature/cross-host".to_string())
                    .target_ref("main".to_string())
                    .workspace_slug("flotilla".to_string())
                    .subpaths(Vec::new())
                    .build()])
                .adopted_checkout_refs(BTreeMap::from([(repository_key.clone(), "checkout-b".to_string())]))
                .build(),
        )
        .await
        .expect("create authority convoy");
    let mut landing_status = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(flotilla_resources::ExitDeclaration::standard_table()),
            turn_delivery: Default::default(),
            vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()],
        }),
        work: BTreeMap::from([("work".to_string(), flotilla_resources::WorkState::builder().phase(WorkPhase::Complete).build())]),
        observed_workflow_ref: Some("review-and-fix".to_string()),
        ..Default::default()
    };
    landing_status.branch_subject_scan_at = Some(chrono::Utc::now());
    landing_status.discover_subject(
        flotilla_protocol::Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::provider_data::IssueSource {
                service: "github.com".to_string(),
                scope: "flotilla-org/flotilla".to_string(),
            },
            id: "1367".to_string(),
        },
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Branch,
        chrono::Utc::now(),
    );
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &landing_status)
        .await
        .expect("mark authority convoy Landing");
    checkout_host
        .replica_writer::<Convoy>(authority_root, NAMESPACE)
        .replace(&convoys.list().await.expect("list authority convoys"), chrono::Utc::now())
        .await
        .expect("replicate Landing convoy to checkout host");

    let git_repo = TestGitRepo::init(temp.path().join("checkout")).with_initial_commit();
    let checkouts = checkout_host.clone().using::<Checkout>(NAMESPACE);
    let checkout = checkouts
        .create(
            &flotilla_resources::InputMeta::builder()
                .name("checkout-b".to_string())
                .labels(BTreeMap::from([
                    (flotilla_resources::CONVOY_LABEL.to_string(), "cross-host".to_string()),
                    (flotilla_resources::CHANGE_REQUEST_ID_LABEL.to_string(), "1367".to_string()),
                ]))
                .annotations(BTreeMap::from([(
                    flotilla_resources::ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(),
                    "authority-root".to_string(),
                )]))
                .build(),
            &CheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "feature/cross-host".to_string(),
                path: git_repo.path().display().to_string(),
                repo_ref: repository_key,
                host_ref: "checkout-host".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("create checkout on authority host B");
    checkouts
        .update_status(
            &checkout.metadata.name,
            &checkout.metadata.resource_version,
            &ResourceCheckoutStatus {
                phase: ResourceCheckoutPhase::Ready,
                path: Some(git_repo.path().display().to_string()),
                integration: CheckoutIntegrationStatus::default(),
                ..Default::default()
            },
        )
        .await
        .expect("mark checkout ready");

    let checkout = checkouts.get("checkout-b").await.expect("get checkout on authority host B");
    let observer = CheckoutReconciler::new(
        Arc::new(CheckoutControllerRuntime::new(
            Arc::new(MergedPrProcessRunner::new(1367)),
            Some(test_controller_vcs(Arc::new(MergedPrProcessRunner::new(1367)), &git_repo.path().to_string_lossy())),
            None,
            Vec::new(),
        )),
        checkout_host.clone(),
        NAMESPACE,
    )
    .with_federated_convoys(&checkout_host, NAMESPACE);
    let dependencies = observer.prepare(&checkout).await.expect("observe integration on checkout authority");
    let observation = observer.reconcile(&checkout, &dependencies, chrono::Utc::now());
    let patch = observation.patch.expect("Landing observation should update checkout evidence");
    flotilla_resources::apply_status_patch(&checkouts, "checkout-b", &patch).await.expect("persist authority-local observation");
    assert_eq!(
        checkouts.get("checkout-b").await.expect("observed checkout").status.expect("checkout status").integration.landed.value,
        ConditionValue::True,
        "checkout authority B must observe the merged change request",
    );
    publish_merged_change_request(&checkout_host, 1367, "checkout-root").await;

    authority
        .replica_writer::<Checkout>(checkout_root, NAMESPACE)
        .replace(&checkouts.list().await.expect("list checkout authority resources"), chrono::Utc::now())
        .await
        .expect("replicate fresh evidence to convoy authority A");
    authority
        .replica_writer::<flotilla_resources::ChangeRequest>(flotilla_protocol::NodeId::new("checkout-root"), NAMESPACE)
        .replace(
            &checkout_host
                .using::<flotilla_resources::ChangeRequest>(NAMESPACE)
                .list()
                .await
                .expect("list checkout authority CR observations"),
            chrono::Utc::now(),
        )
        .await
        .expect("replicate fresh CR evidence to convoy authority A");
    let current = convoys.get("cross-host").await.expect("get Landing convoy");
    let reconciler = ConvoyReconciler::new(authority.definitions::<WorkflowTemplate>(NAMESPACE))
        .with_federated_checkouts(authority.including_replicas::<Checkout>(NAMESPACE))
        .with_change_requests(
            authority.including_replicas::<flotilla_resources::ChangeRequest>(NAMESPACE),
            authority_daemon.change_request_stale_after(),
        )
        .with_teardown_runtime(Arc::new(DaemonConvoyTeardownRuntime::new(authority_daemon)));
    let dependencies = reconciler.prepare(&current).await.expect("consume replicated checkout evidence");
    let outcome = reconciler.reconcile(&current, &dependencies, chrono::Utc::now());
    let patch = outcome.patch.expect("fresh replicated evidence should settle the convoy");
    flotilla_resources::apply_status_patch(&convoys, "cross-host", &patch).await.expect("persist Landed phase");

    assert_eq!(convoys.get("cross-host").await.expect("settled convoy").status.expect("convoy status").phase, ConvoyPhase::Landed,);
    assert!(
        authority.clone().using::<Checkout>(NAMESPACE).list().await.expect("list authority-local checkouts").items.is_empty(),
        "convoy authority A must not re-author or write the replicated checkout",
    );
}

// A Landed convoy and its running orphan predate daemon startup. The
// convoy is a replica here: startup must reap the local session through
// the real reclaim gate, without any convoy or session change event.
#[tokio::test(start_paused = true)]
async fn daemon_startup_reaps_retained_landed_convoy_orphan() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"orphan-startup-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let backend = ResourceBackend::InMemory(Default::default());
    let (initial, _) = crew_daemon_with_backend(Arc::clone(&config), backend.clone()).await;
    let registry = probe_local_provider_registry(&initial, &config).await.expect("registry");
    let profile = build_local_profile(&initial, &registry).expect("profile");
    let coordinator = ResourceBackend::InMemory(Default::default());
    let convoys = coordinator.using::<Convoy>(NAMESPACE);
    let created =
        convoys.create(&empty_meta("old-landed"), &ConvoySpec::builder().workflow_ref("wf".to_string()).build()).await.expect("convoy");
    convoys
        .update_status("old-landed", &created.metadata.resource_version, &landed_status_with_placed_checkout("collected-checkout"))
        .await
        .expect("landed");
    backend
        .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("coordinator"), NAMESPACE)
        .replace(&convoys.list().await.expect("convoys"), Utc::now())
        .await
        .expect("replicate convoy");
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let created = sessions
        .create(
            &InputMeta::builder()
                .name("old-orphan".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "old-landed".to_string())]))
                .annotations(BTreeMap::from([(flotilla_resources::ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "coordinator".to_string())]))
                .finalizers(vec!["flotilla.work/terminal-teardown".to_string()])
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Managed),
            &TerminalSessionSpec {
                env_ref: profile.host_direct_environment_name(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: profile.host_direct_pool.clone(),
            },
        )
        .await
        .expect("session");
    sessions
        .update_status(
            "old-orphan",
            &created.metadata.resource_version,
            &TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                session_id: Some("old-process".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("running orphan");
    drop(initial);
    let (restarted, pool) = crew_daemon_with_backend(Arc::clone(&config), backend.clone()).await;
    pool.add_sessions(vec![ProviderTerminalSession::builder()
        .session_name("old-process".to_string())
        .status(TerminalStatus::Running)
        .command("cargo test".to_string())
        .working_directory(ExecutionEnvironmentPath::new("/workspace"))
        .build()])
        .await;
    let runtime = DaemonRuntime::start_with_options(
        Arc::clone(&restarted),
        config,
        None,
        RuntimeOptions { controller_resync_interval: Duration::from_secs(3600), ..RuntimeOptions::default() },
    )
    .await
    .expect("daemon startup");
    for _ in 0..1000 {
        tokio::task::yield_now().await;
        if matches!(sessions.get("old-orphan").await, Err(ResourceError::NotFound { .. })) {
            break;
        }
    }
    runtime.shutdown();
    assert!(matches!(sessions.get("old-orphan").await, Err(ResourceError::NotFound { .. })), "startup must reap the old orphan");
    assert!(pool.list_sessions().await.expect("pool").is_empty(), "terminal finalizer must kill the old process");
    assert_eq!(*pool.killed.lock().await, vec!["old-process"]);
    assert!(
        backend.using::<Environment>(NAMESPACE).get(&profile.host_direct_environment_name()).await.is_ok(),
        "the gate, rather than a missing environment shortcut, must authorize cleanup"
    );
    assert!(backend.using::<Convoy>(NAMESPACE).list().await.expect("local convoys").items.is_empty());
    assert_eq!(convoys.get("old-landed").await.expect("retained convoy").status.expect("status").phase, ConvoyPhase::Landed);
}

#[tokio::test(start_paused = true)]
async fn adopted_checkout_reconciliation_task_runs_after_interval() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"adopted-checkout-reconciliation-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let durable = daemon.resource_backend().using::<ResourceCheckout>(NAMESPACE);
    let created = durable
        .create(
            &InputMeta::builder()
                .name("adopted-checkout-periodic".to_string())
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Adopted),
            &ResourceCheckoutSpec::Observed(
                ResourceObservedCheckoutSpec::builder()
                    .r#ref("feature/periodic".to_string())
                    .path("/work/periodic".to_string())
                    .repo_ref(flotilla_resources::RepositoryKey("widgets-api".to_string()))
                    .host_ref("host-01".to_string())
                    .is_main(false)
                    .build(),
            ),
        )
        .await
        .expect("durable adopted checkout should be created");
    durable
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).path("/work/periodic".to_string()).build(),
        )
        .await
        .expect("durable checkout status should be stored");
    let interval = Duration::from_secs(60);
    let reconciliation = spawn_adopted_checkout_reconciliation_task(Arc::clone(&daemon), NAMESPACE.to_string(), interval);
    tokio::task::yield_now().await;
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>(NAMESPACE);
    assert!(
        matches!(observed.get("adopted-checkout-periodic").await, Err(ResourceError::NotFound { .. })),
        "the periodic task should wait for its first interval"
    );

    tokio::time::advance(interval).await;
    tokio::task::yield_now().await;

    observed.get("adopted-checkout-periodic").await.expect("periodic reconciliation should restore the observed checkout");
    reconciliation.abort();
}

#[tokio::test(start_paused = true)]
async fn codex_central_refresh_task_survives_repeated_local_failures_without_panicking() {
    struct FixedHomeEnv(PathBuf);
    impl EnvVars for FixedHomeEnv {
        fn get(&self, key: &str) -> Option<String> {
            (key == "HOME").then(|| self.0.to_string_lossy().into_owned())
        }
    }

    let home = TempDir::new().expect("tempdir");
    // Deliberately leave the central auth.json unprovisioned, mirroring a
    // host before an operator has dedicated a pool slot as the central
    // source. This must never reach the network: the task should fail
    // locally and keep ticking, not panic the daemon.
    let env: Arc<dyn EnvVars> = Arc::new(FixedHomeEnv(home.path().to_path_buf()));
    let interval = Duration::from_secs(60);
    let task = spawn_codex_central_refresh_task(env, interval);

    for _ in 0..3 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "a missing central auth.json must log a warning and retry, never panic the daemon");
    }

    task.abort();
    let _ = task.await;
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
        ResourceBackend::InMemory(Default::default()),
        runner,
        Some(Arc::new(FakeChangeRequest::new())),
    )
    .await;
    run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, CompletionAction::Retain).await;
}

#[tokio::test]
async fn sqlite_stage4a_flow_reaches_running_and_completes_convoy() {
    let temp = TempDir::new().expect("tempdir");
    let repo_default_dir = temp.path().join("flotilla-repos");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"sqlite-stage4a-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    std::fs::create_dir_all(config.state_dir()).expect("state dir");
    let runner = Arc::new(Stage4aGitRunner { inner: Arc::new(NoPrProcessRunner), upstream: repo.clone() });
    let daemon = daemon_with_backend_runner_and_change_requests(
        vec![repo.clone()],
        Arc::clone(&config),
        ResourceBackend::Sqlite(SqliteBackend::open(config.state_dir().join("resources.sqlite")).expect("sqlite backend")),
        runner,
        Some(Arc::new(FakeChangeRequest::new())),
    )
    .await;
    run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, CompletionAction::Retain).await;
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
        ResourceBackend::InMemory(Default::default()),
        Arc::new(Stage4aGitRunner { inner: Arc::new(MergedPrProcessRunner::new(884)), upstream: repo.clone() }),
        Some(requests),
    )
    .await;

    run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, CompletionAction::Delete).await;
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
                .build()
                .expect("test runtime")
                .block_on(crew_provisioning_recovery_scenario());
        })
        .expect("test thread")
        .join()
        .expect("test thread completed");
}
