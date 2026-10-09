use super::*;

#[tokio::test]
async fn sqlite_adopted_checkout_flow_reaches_running_and_preserves_checkout_on_complete() {
    let temp = TempDir::new().expect("tempdir");
    let repo_default_dir = temp.path().join("flotilla-repos");
    std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("config directory");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"sqlite-adopted-checkout-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    let daemon = sqlite_daemon(vec![repo.clone()], Arc::clone(&config)).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = LocalProvisioningProfile { repo_default_dir: repo_default_dir.display().to_string(), ..manual_profile(&host_id, false) };
    let backend = daemon.resource_backend();

    register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("startup registration should succeed");
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("host heartbeat should succeed");

    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        Arc::clone(&config),
        passthrough_registry(),
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let controller_handles = spawn_controller_loops(
        Arc::clone(&state),
        NAMESPACE,
        Duration::from_millis(25),
        ControllerSupervision::default(),
        RuntimeHealth::default(),
    );

    backend
        .clone()
        .using::<WorkflowTemplate>(NAMESPACE)
        .create(
            &empty_meta("wf-a"),
            &WorkflowTemplateSpec::builder()
                .exit(flotilla_resources::ExitDeclaration::standard_table())
                .inputs(Vec::new())
                .vessels(vec![VesselRequirement::builder()
                    .name("implement".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Tool { command: "bash -lc 'echo adopted-stage4a'".to_string() })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow template create should succeed");

    let mut rx = daemon.subscribe();
    let create_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "convoy-adopted".to_string(),
                    workflow_ref: "wf-a".to_string(),
                    inputs: Vec::new(),
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: Some(format!("host-direct-{host_id}")),
                    adopted_checkout: Some(Box::new(repo.clone())),
                })
                .build(),
        )
        .await
        .expect("convoy create command should start");
    assert_eq!(wait_for_command_result(&mut rx, create_id).await, CommandValue::ConvoyCreated { name: "convoy-adopted".to_string() });

    let checkouts = backend.clone().using::<ResourceCheckout>(NAMESPACE);
    let checkout_count = checkouts.list().await.expect("list adopted checkouts").items.len();
    let duplicate_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "convoy-adopted".to_string(),
                    workflow_ref: "wf-a".to_string(),
                    inputs: Vec::new(),
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: Some(format!("host-direct-{host_id}")),
                    adopted_checkout: Some(Box::new(repo.clone())),
                })
                .build(),
        )
        .await
        .expect("duplicate convoy create command should start");
    assert_eq!(
        wait_for_command_result(&mut rx, duplicate_id).await,
        CommandValue::Error { message: "live convoy convoy-adopted generation 1 already exists".to_string() }
    );
    assert_eq!(
        checkouts.list().await.expect("list adopted checkouts after duplicate").items.len(),
        checkout_count,
        "a rejected duplicate must not persist an orphan adopted checkout"
    );

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let adopted_record = convoy_record_name(&backend, "convoy-adopted").await;
    wait_until(|| {
        let convoys = convoys.clone();
        let adopted_record = adopted_record.clone();
        async move {
            matches!(
                convoys.get(&adopted_record).await.ok().and_then(|convoy| convoy.status).as_ref(),
                Some(status)
                    if status.phase == ConvoyPhase::Active
                        && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
            )
        }
    })
    .await;

    daemon.reconcile_adopted_checkouts(NAMESPACE).await.expect("adopted checkout integration observation should succeed");
    record_successful_empty_branch_scan(&backend, &adopted_record).await;
    let complete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-adopted".to_string(),
                    work: "implement".to_string(),
                    message: Some("done".to_string()),
                })
                .build(),
        )
        .await
        .expect("convoy completion command should start");
    assert_eq!(wait_for_command_result(&mut rx, complete_id).await, CommandValue::Ok);

    daemon.reconcile_adopted_checkouts(NAMESPACE).await.expect("Landing should refresh adopted checkout integration evidence");

    wait_until(|| {
        let convoys = convoys.clone();
        let adopted_record = adopted_record.clone();
        async move {
            matches!(
                convoys.get(&adopted_record).await.ok().and_then(|convoy| convoy.status).as_ref(),
                Some(status)
                    if status.phase == ConvoyPhase::Landed
                        && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Complete)
            )
        }
    })
    .await;

    let checkout = backend
        .clone()
        .using::<ResourceCheckout>(NAMESPACE)
        .list()
        .await
        .expect("list adopted checkouts")
        .items
        .into_iter()
        .find(|checkout| checkout.metadata.lifecycle_authority().ok().flatten() == Some(LifecycleAuthority::Adopted))
        .expect("adopted checkout should remain after completion");
    assert_eq!(checkout.metadata.lifecycle_authority().expect("authority should parse"), Some(LifecycleAuthority::Adopted));
    assert_eq!(backend.clone().using::<ResourceCheckout>(NAMESPACE).list().await.expect("checkout list").items.len(), 1);

    for handle in controller_handles {
        handle.abort();
        let _ = handle.await;
    }
}
