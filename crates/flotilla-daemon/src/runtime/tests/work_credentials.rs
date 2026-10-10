use super::*;

#[tokio::test]
async fn convoy_work_state_reconciles_credentials_after_resume_and_review_delivery() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"credential-work-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("test-host"),
    )
    .await;
    let backend = daemon.resource_backend();
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("work-token"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::GitHttpToken { host: "github.com".to_string(), username: "bot".to_string() },
                source: CredentialSource::Env { name: "TEST_WORK_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Issued,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("credential declaration");
    let runner = Arc::new(ProcessCommandRunner);
    let store = Arc::new(CredentialStore::new(
        backend.clone(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_WORK_TOKEN", "test-token")])),
        EnvironmentBag::new(),
        runner.clone(),
        temp.path().to_path_buf(),
    ));
    let env_id = EnvironmentId::new("env-work");
    daemon
        .register_provisioned_environment(
            env_id.clone(),
            Arc::new(TestInteriorEnvironment {
                id: env_id.clone(),
                image: ImageId::new("test-image"),
                runner: runner.clone(),
                env_vars: HashMap::new(),
                destroyed: Arc::new(AtomicBool::new(false)),
            }),
            EnvironmentBag::new(),
            Some(passthrough_registry()),
        )
        .expect("register test environment");
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("credential-work"), &ConvoySpec::builder().workflow_ref("test".to_string()).build())
        .await
        .expect("create convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .credential_refs(BTreeSet::from(["work-token".to_string()]))
                        .crew(Vec::new())
                        .build()],
                }),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Running).build())]),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("mark work running");
    let vessels = backend.using::<Vessel>(NAMESPACE);
    let vessel = vessels
        .create(
            &empty_meta("credential-work-vessel"),
            &VesselSpec {
                convoy_ref: "credential-work".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create vessel");
    vessels
        .update_status(
            &vessel.metadata.name,
            &vessel.metadata.resource_version,
            &VesselStatus {
                phase: flotilla_resources::VesselPhase::Ready,
                environment_ref: Some(env_id.as_str().to_string()),
                ..VesselStatus::default()
            },
        )
        .await
        .expect("place vessel");
    create_credential_test_session(&backend, "credential-work-session", "credential-work", "credential-work-vessel", env_id.as_str()).await;
    let state = ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        passthrough_registry(),
        None,
        "test-host".to_string(),
        "host-direct-test".to_string(),
    )
    .with_credential_store(store);
    let git_config = temp.path().join("credentials/gitconfig").to_string_lossy().into_owned();
    let can_fill_git_credential = || {
        let runner = runner.clone();
        let git_config = git_config.clone();
        async move {
            runner
                    .run(
                        "sh",
                        &[
                            "-c",
                            "export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0; printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill",
                            "credential-probe",
                            &git_config,
                        ],
                        Path::new("/"),
                        &ChannelLabel::Default,
                    )
                    .await
                    .is_ok()
        }
    };
    let settle = |phase| {
        let convoys = &convoys;
        async move {
            let current = convoys.get("credential-work").await.expect("read convoy");
            let mut status = current.status.expect("convoy status");
            status.phase = phase;
            status.work.get_mut("work").expect("work").phase = WorkPhase::Complete;
            convoys.update_status("credential-work", &current.metadata.resource_version, &status).await.expect("settle work");
        }
    };

    let unavailable_convoy = convoys
        .create(
            &empty_meta("unavailable-credential-work"),
            &ConvoySpec::builder().workflow_ref("test".to_string()).placement_policy("missing-policy".to_string()).build(),
        )
        .await
        .expect("create convoy with unavailable placement");
    convoys
        .update_status(
            &unavailable_convoy.metadata.name,
            &unavailable_convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("unavailable".to_string())
                        .credential_refs(BTreeSet::from(["work-token".to_string()]))
                        .credential_scopes(BTreeMap::from([(
                            "work-token".to_string(),
                            BTreeSet::from([RepositoryKey("repo".to_string())]),
                        )]))
                        .crew(Vec::new())
                        .build()],
                }),
                work: BTreeMap::from([("unavailable".to_string(), WorkState::builder().phase(WorkPhase::Running).build())]),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("mark unavailable work running");
    let unavailable_vessel = vessels
        .create(
            &empty_meta("unavailable-work-vessel"),
            &VesselSpec {
                convoy_ref: "unavailable-credential-work".to_string(),
                vessel_name: "unavailable".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create unavailable vessel");
    vessels
        .update_status(
            &unavailable_vessel.metadata.name,
            &unavailable_vessel.metadata.resource_version,
            &VesselStatus {
                phase: flotilla_resources::VesselPhase::Ready,
                environment_ref: Some("aaa-unavailable".to_string()),
                ..VesselStatus::default()
            },
        )
        .await
        .expect("place unavailable vessel");
    create_credential_test_session(
        &backend,
        "unavailable-work-session",
        "unavailable-credential-work",
        "unavailable-work-vessel",
        "aaa-unavailable",
    )
    .await;
    reconcile_work_credentials_for_environment(&state, NAMESPACE, env_id.as_str())
        .await
        .expect("target environment stages despite unrelated failure");
    assert!(can_fill_git_credential().await);
    let scoped_error =
        reconcile_work_credentials_for_environment(&state, NAMESPACE, "aaa-unavailable").await.expect_err("unavailable target still fails");
    assert!(scoped_error.contains("aaa-unavailable"), "{scoped_error}");
    assert!(reconcile_work_credentials(&state, NAMESPACE).await.is_err());
    assert!(can_fill_git_credential().await, "unavailable placement must not block another environment's delivery");
    vessels.delete("unavailable-work-vessel").await.expect("remove unavailable vessel");
    backend.clone().using::<TerminalSession>(NAMESPACE).delete("unavailable-work-session").await.expect("remove unavailable session");

    let environments = backend.clone().using::<Environment>(NAMESPACE);
    environments
        .create(
            &empty_meta(env_id.as_str()),
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "local".into(), repo_default_dir: "/tmp".into() }),
                docker: None,
            },
        )
        .await
        .expect("create durable shared environment");

    let conflicting_convoy = convoys
        .create(
            &empty_meta("conflicting-credential-work"),
            &ConvoySpec::builder().workflow_ref("test".to_string()).placement_policy("test".to_string()).build(),
        )
        .await
        .expect("create second crew convoy");
    convoys
        .update_status(
            &conflicting_convoy.metadata.name,
            &conflicting_convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("other-work".to_string())
                        .credential_refs(BTreeSet::from(["work-token".to_string()]))
                        .credential_permissions(BTreeMap::from([(
                            "work-token".to_string(),
                            BTreeMap::from([("contents".to_string(), "read".to_string())]),
                        )]))
                        .crew(Vec::new())
                        .build()],
                }),
                work: BTreeMap::from([("other-work".to_string(), WorkState::builder().phase(WorkPhase::Running).build())]),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("mark second crew running");
    let conflicting_vessel = vessels
        .create(
            &empty_meta("conflicting-work-vessel"),
            &VesselSpec {
                convoy_ref: "conflicting-credential-work".to_string(),
                vessel_name: "other-work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create second vessel");
    vessels
        .update_status(
            &conflicting_vessel.metadata.name,
            &conflicting_vessel.metadata.resource_version,
            &VesselStatus {
                phase: flotilla_resources::VesselPhase::Ready,
                environment_ref: Some(env_id.as_str().to_string()),
                ..VesselStatus::default()
            },
        )
        .await
        .expect("place second vessel in shared environment");
    create_credential_test_session(
        &backend,
        "conflicting-work-session",
        "conflicting-credential-work",
        "conflicting-work-vessel",
        env_id.as_str(),
    )
    .await;
    let error = reconcile_work_credentials(&state, NAMESPACE).await.expect_err("shared environment must reject differing permissions");
    assert!(error.contains("different minted permissions for `work-token`"), "{error}");
    let conflict_retry = environments
        .get(env_id.as_str())
        .await
        .expect("shared environment")
        .status
        .expect("status")
        .credential_delivery_retry
        .expect("durable conflict retry");
    assert!(matches!(conflict_retry.disposition, ControllerRetryDisposition::Retryable { .. }));
    let conflict_environment_version = environments.get(env_id.as_str()).await.expect("shared environment").metadata.resource_version;
    let conflict_vessel_version = vessels.get("credential-work-vessel").await.expect("first vessel").metadata.resource_version;
    let repeated_error = reconcile_work_credentials(&state, NAMESPACE).await.expect_err("persistent conflict remains retryable");
    assert!(repeated_error.contains("different minted permissions for `work-token`"), "{repeated_error}");
    assert_eq!(
        environments.get(env_id.as_str()).await.expect("shared environment").metadata.resource_version,
        conflict_environment_version
    );
    assert_eq!(vessels.get("credential-work-vessel").await.expect("first vessel").metadata.resource_version, conflict_vessel_version);
    vessels.delete("conflicting-work-vessel").await.expect("remove second vessel");
    backend.clone().using::<TerminalSession>(NAMESPACE).delete("conflicting-work-session").await.expect("remove second session");

    // Simulate the retry deadline elapsing while preserving the persisted
    // episode, then verify the normal pass can recover without an operator.
    let due_retry = ControllerRetry {
        disposition: ControllerRetryDisposition::Retryable { next_attempt_at: Utc::now() - chrono::Duration::seconds(1) },
        ..conflict_retry
    };
    flotilla_store::apply_status_patch(
        &environments,
        env_id.as_str(),
        &EnvironmentStatusPatch::CredentialDelivery { retry: Some(due_retry) },
    )
    .await
    .expect("advance retry deadline");

    reconcile_work_credentials(&state, NAMESPACE).await.expect("stage running credentials");
    assert!(
        environments.get(env_id.as_str()).await.expect("shared environment").status.expect("status").credential_delivery_retry.is_none(),
        "resolved conflict should clear durable retry"
    );
    assert!(can_fill_git_credential().await);
    let current = convoys.get("credential-work").await.expect("read running work");
    let mut stalled = current.status.expect("work status");
    stalled.work.get_mut("work").expect("work").phase = WorkPhase::Stalled;
    convoys.update_status("credential-work", &current.metadata.resource_version, &stalled).await.expect("stall work");
    reconcile_work_credentials(&state, NAMESPACE).await.expect("retain stalled credentials");
    assert!(can_fill_git_credential().await);
    settle(ConvoyPhase::Landing).await;
    reconcile_work_credentials(&state, NAMESPACE).await.expect("retain credentials after claim admission");
    assert!(can_fill_git_credential().await, "live crew can act after its claim is admitted");
    flotilla_store::apply_status_patch(
        &convoys,
        "credential-work",
        &flotilla_resources::external_patches::resume_crew_work(
            "work".to_string(),
            "coder".to_string(),
            Utc::now(),
            "follow-up".to_string(),
            None,
        ),
    )
    .await
    .expect("deliver resume brief");
    reconcile_work_credentials(&state, NAMESPACE).await.expect("retain credentials for resumed input");
    assert!(can_fill_git_credential().await);

    settle(ConvoyPhase::Landing).await;
    reconcile_work_credentials(&state, NAMESPACE).await.expect("retain credentials at review boundary");
    assert!(can_fill_git_credential().await);
    flotilla_store::apply_status_patch(
        &convoys,
        "credential-work",
        &flotilla_resources::external_patches::record_turn_delivery(
            "actionable-review".to_string(),
            flotilla_resources::TurnDeliveryEpisode::builder()
                .subject_revision("abc".to_string())
                .evidence_at(Utc::now())
                .judged_claim_at(Utc::now())
                .outcome(flotilla_resources::TurnDeliveryOutcome::Delivered {
                    rung: flotilla_resources::TurnDeliveryRung::WarmSession,
                    delivered_at: Utc::now(),
                })
                .build(),
            "work".to_string(),
            "coder".to_string(),
            "address review".to_string(),
        ),
    )
    .await
    .expect("deliver review turn");
    reconcile_work_credentials(&state, NAMESPACE).await.expect("retain credentials for review turn");
    assert!(can_fill_git_credential().await);

    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    let session = sessions.get("credential-work-session").await.expect("live crew session");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &TerminalSessionStatus { phase: TerminalSessionPhase::Stopped, ..TerminalSessionStatus::default() },
        )
        .await
        .expect("stop last crew session");
    reconcile_work_credentials(&state, NAMESPACE).await.expect("revoke when last session ends");
    assert!(!can_fill_git_credential().await);

    sessions.delete("credential-work-session").await.expect("tear down crew session");
    vessels.delete("credential-work-vessel").await.expect("tear down vessel");
    reconcile_work_credentials(&state, NAMESPACE).await.expect("revoke credentials after vessel teardown");
    assert!(!can_fill_git_credential().await);
}

#[tokio::test]
async fn undecodable_credential_spec_parks_delivery_as_a_terminal_controller_failure() {
    let temp = TempDir::new().expect("tempdir");
    let sqlite_path = temp.path().join("resources.sqlite");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&sqlite_path).expect("sqlite store"));
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    environments
        .create(
            &empty_meta("credential-work"),
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "local".into(), repo_default_dir: "/tmp".into() }),
                docker: None,
            },
        )
        .await
        .expect("work environment");
    let connection = rusqlite::Connection::open(&sqlite_path).expect("raw sqlite store");
    insert_undecodable_resource::<CredentialSpec>(&connection, "broken-spec");
    drop(connection);
    flotilla_store::quarantine_undecodable_stored_objects(&backend, NAMESPACE).await.expect("quarantine undecodable spec");
    let granted = BTreeSet::from(["broken-spec".to_string()]);
    let need = credential_quarantine_need(&backend, NAMESPACE, &granted)
        .await
        .expect("inspect quarantine")
        .expect("undecodable spec needs operator");
    assert!(need.contains("broken-spec") && need.contains("does not decode"), "{need}");
    record_credential_delivery_retry(&backend, NAMESPACE, "credential-work", Some(need.clone()), true)
        .await
        .expect("persist terminal delivery disposition");
    let retry = environments
        .get("credential-work")
        .await
        .expect("environment")
        .status
        .expect("status")
        .credential_delivery_retry
        .expect("delivery retry");
    assert_eq!(retry.stall_reason(Utc::now(), flotilla_resources::RetryCeiling::default()), Some(need.clone()));
    assert!(matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. }));
    let resource_version = environments.get("credential-work").await.expect("environment").metadata.resource_version;
    record_credential_delivery_retry(&backend, NAMESPACE, "credential-work", Some(need), true).await.expect("repeat terminal disposition");
    // A repeated terminal failure does not churn durable status.
    let after_repeat = environments.get("credential-work").await.expect("environment");
    assert_eq!(after_repeat.metadata.resource_version, resource_version);
    record_credential_delivery_retry(&backend, NAMESPACE, "credential-work", None, false).await.expect("operator retry reset");
    assert!(environments.get("credential-work").await.expect("environment").status.expect("status").credential_delivery_retry.is_none());
    record_credential_delivery_retry(&backend, NAMESPACE, "credential-work", Some("provider unavailable".into()), false)
        .await
        .expect("retry after operator reset");
    assert_eq!(
        environments
            .get("credential-work")
            .await
            .expect("environment")
            .status
            .expect("status")
            .credential_delivery_retry
            .expect("new episode")
            .attempts,
        1
    );
}
