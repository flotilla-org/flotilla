use super::*;
use crate::testkits::engine::{admit_convoy, engine};
use flotilla_engine_convoys_testkit::{agent, convoy};
use flotilla_resources::convoy::engine::note;
use flotilla_resources::vessel_records::VesselStep;
use flotilla_resources::ResumeIntent;
use tokio_util::task::AbortOnDropHandle;

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
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
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
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
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
    let launch = admit_convoy(
        &daemon,
        convoy(
            NAMESPACE,
            "credential-work",
            ConvoySpec::builder().project_ref(NAMESPACE.into()).workflow_ref("test".to_string()).build(),
            credential_work(
                VesselRequirement::builder()
                    .name("work".to_string())
                    .credential_refs(BTreeSet::from(["work-token".to_string()]))
                    .crew(vec![agent("coder")])
                    .build(),
            ),
        ),
    )
    .await;
    let fixture = engine(&daemon);
    let vessels = &fixture.vessels;
    let host = daemon.local_host_id().expect("local host").to_string();
    let vessel = vessels.request(&launch, "work", &host);
    vessels.ready(&vessel, env_id.as_str());
    let live = create_credential_test_session(&engine(&daemon).terminals, "credential-work-session", &vessel, env_id.as_str());
    engine_environments(&daemon);
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            "test-host".to_string(),
            "host-direct-test".to_string(),
        )
        .with_credential_store(store),
    );
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
    let unavailable_launch = admit_convoy(
        &daemon,
        convoy(
            NAMESPACE,
            "unavailable-credential-work",
            ConvoySpec::builder()
                .project_ref(NAMESPACE.into())
                .workflow_ref("test".to_string())
                .placement_policy("missing-policy".to_string())
                .build(),
            credential_work(
                VesselRequirement::builder()
                    .name("unavailable".to_string())
                    .credential_refs(BTreeSet::from(["work-token".to_string()]))
                    .credential_scopes(BTreeMap::from([("work-token".to_string(), BTreeSet::from([RepositoryKey("repo".to_string())]))]))
                    .crew(vec![agent("coder")])
                    .build(),
            ),
        ),
    )
    .await;
    let unavailable_vessel = vessels.request(&unavailable_launch, "unavailable", &host);
    vessels.ready(&unavailable_vessel, "aaa-unavailable");
    let unavailable =
        create_credential_test_session(&engine(&daemon).terminals, "unavailable-work-session", &unavailable_vessel, "aaa-unavailable");
    // The start pass stages the target environment despite the unrelated environment's failure.
    let mut delivery = CredentialDelivery::new(Arc::clone(&state)).expect("a credential store");
    let error = delivery.start().await.expect_err("the unavailable environment fails");
    assert!(error.contains("aaa-unavailable"), "{error}");
    assert!(can_fill_git_credential().await, "unavailable placement must not block another environment's delivery");
    let _controller = AbortOnDropHandle::new(tokio::spawn(delivery.run(state.controller_metrics.for_kind("CredentialDelivery"))));
    let deliver = |environment: &'static str| {
        let state = Arc::clone(&state);
        async move { state.credential_events.deliver(environment).await }
    };
    let env = env_id.as_str().to_owned().leak();
    let scoped_error = deliver("aaa-unavailable").await.expect_err("unavailable target still fails");
    assert!(scoped_error.contains("aaa-unavailable"), "{scoped_error}");
    deliver(env).await.expect("target environment stages despite unrelated failure");
    vessels.step(&unavailable_vessel, VesselStep::Released);
    engine(&daemon).terminals.release(&unavailable);

    engine_environments(&daemon).request(
        env_id.as_str(),
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "local".into(), repo_default_dir: "/tmp".into() }),
            ..Default::default()
        },
        None,
    );

    let conflicting_launch = admit_convoy(
        &daemon,
        convoy(
            NAMESPACE,
            "conflicting-credential-work",
            ConvoySpec::builder()
                .project_ref(NAMESPACE.into())
                .workflow_ref("test".to_string())
                .placement_policy("test".to_string())
                .build(),
            credential_work(
                VesselRequirement::builder()
                    .name("other-work".to_string())
                    .credential_refs(BTreeSet::from(["work-token".to_string()]))
                    .credential_permissions(BTreeMap::from([(
                        "work-token".to_string(),
                        BTreeMap::from([("contents".to_string(), "read".to_string())]),
                    )]))
                    .crew(vec![agent("coder")])
                    .build(),
            ),
        ),
    )
    .await;
    let conflicting_vessel = vessels.request(&conflicting_launch, "other-work", &host);
    vessels.ready(&conflicting_vessel, env_id.as_str());
    let conflicting =
        create_credential_test_session(&engine(&daemon).terminals, "conflicting-work-session", &conflicting_vessel, env_id.as_str());
    let error = deliver(env).await.expect_err("shared environment must reject differing permissions");
    assert!(error.contains("different minted permissions for `work-token`"), "{error}");
    let repeated_error = deliver(env).await.expect_err("a persistent conflict keeps failing");
    assert!(repeated_error.contains("different minted permissions for `work-token`"), "{repeated_error}");
    vessels.step(&conflicting_vessel, VesselStep::Released);
    engine(&daemon).terminals.release(&conflicting);

    deliver(env).await.expect("the resolved conflict stages the running crew's credentials");
    assert!(can_fill_git_credential().await);
    // The crew's work phases are derived on the engine, so only this home's notes on the launch
    // change under a live crew: a resume brief and a review turn leave its credentials in place.
    let path = daemon.convoy_engine_path();
    let noted = || async { convoys.get(&launch).await.expect("read the launch") };
    note(
        &path,
        &noted().await,
        &flotilla_resources::external_patches::resume_crew_work(
            "work".to_string(),
            "coder".to_string(),
            Utc::now(),
            "follow-up".to_string(),
            None,
            ResumeIntent::Reopen,
        ),
    )
    .expect("deliver resume brief");
    deliver(env).await.expect("retain credentials for resumed input");
    assert!(can_fill_git_credential().await);
    note(
        &path,
        &noted().await,
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
    .expect("deliver review turn");
    deliver(env).await.expect("retain credentials for review turn");
    assert!(can_fill_git_credential().await);

    // The last crew session stopping is the event that withdraws its credentials: no request.
    engine(&daemon).terminals.exited(&live, "the crew stopped");
    tokio::time::timeout(Duration::from_secs(10), async {
        while can_fill_git_credential().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the session's stop withdraws its credentials");

    vessels.step(&vessel, VesselStep::Released);
    deliver(env).await.expect("nothing to deliver after vessel teardown");
    assert!(!can_fill_git_credential().await);
}

// A grant whose declaration is missing fails its environment's delivery; the declaration
// arriving is the event that delivers it, with no request and no timer.
#[tokio::test]
async fn a_missing_credential_declaration_is_delivered_once_it_arrives() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"credential-declaration-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("test-host"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let backend = daemon.resource_backend();
    let runner = Arc::new(ProcessCommandRunner);
    let store = Arc::new(CredentialStore::new(
        backend.clone(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_LATE_TOKEN", "test-token")])),
        EnvironmentBag::new(),
        runner.clone(),
        temp.path().to_path_buf(),
    ));
    let env_id = EnvironmentId::new("env-late");
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
    let launch = admit_convoy(
        &daemon,
        convoy(
            NAMESPACE,
            "late-work",
            ConvoySpec::builder().project_ref(NAMESPACE.into()).workflow_ref("test".to_string()).build(),
            credential_work(
                VesselRequirement::builder()
                    .name("work".to_string())
                    .credential_refs(BTreeSet::from(["late-token".to_string()]))
                    .crew(vec![agent("coder")])
                    .build(),
            ),
        ),
    )
    .await;
    let fixture = engine(&daemon);
    let vessels = &fixture.vessels;
    let host = daemon.local_host_id().expect("local host").to_string();
    let vessel = vessels.request(&launch, "work", &host);
    vessels.ready(&vessel, env_id.as_str());
    create_credential_test_session(&engine(&daemon).terminals, "late-session", &vessel, env_id.as_str());
    engine_environments(&daemon);
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            "test-host".to_string(),
            "host-direct-test".to_string(),
        )
        .with_credential_store(store),
    );
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
    let mut delivery = CredentialDelivery::new(Arc::clone(&state)).expect("a credential store");
    let error = delivery.start().await.expect_err("an undeclared credential cannot be delivered");
    assert!(error.contains("late-token") && error.contains("declaration not found"), "{error}");
    assert!(!can_fill_git_credential().await);
    let _controller = AbortOnDropHandle::new(tokio::spawn(delivery.run(state.controller_metrics.for_kind("CredentialDelivery"))));
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("late-token"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::GitHttpToken { host: "github.com".to_string(), username: "bot".to_string() },
                source: CredentialSource::Env { name: "TEST_LATE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Issued,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("credential declaration");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !can_fill_git_credential().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the declaration's arrival delivers the credential");
}

/// A frozen workflow whose one vessel is `requirement`.
fn credential_work(requirement: VesselRequirement) -> ConvoyStatus {
    ConvoyStatus {
        workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: None,
            turn_delivery: Default::default(),
            vessels: vec![requirement],
        }),
        ..ConvoyStatus::default()
    }
}
