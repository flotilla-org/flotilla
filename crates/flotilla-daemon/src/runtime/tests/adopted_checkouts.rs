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

    let provisioning = engine_startup(&daemon, NAMESPACE, &profile).await.expect("startup registration should succeed");
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("host heartbeat should succeed");

    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            passthrough_registry(),
            None,
            profile.host_id.clone(),
            profile.host_direct_environment_name(),
        )
        .with_provisioning_lifecycles(Some(provisioning)),
    );
    let controller_handles =
        spawn_controller_loops(Arc::clone(&state), NAMESPACE, ControllerSupervision::default(), RuntimeHealth::default());

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
                        .promises(vec![flotilla_resources::TemplatePromise::decision_ledger("coder")])
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

    // The checkouts the engine holds here, and the ones this host observes.
    let checkout_count = || async {
        let reads = daemon.lifecycle_reads();
        let held = reads.checkouts().expect("checkouts").here().expect("held here").len();
        let observed = daemon.observed_resource_backend().using::<ResourceCheckout>(NAMESPACE).list().await.expect("observed").items.len();
        (held, observed)
    };
    let checkout_count_before = checkout_count().await;
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
    assert_eq!(checkout_count().await, checkout_count_before, "a rejected duplicate must not persist an orphan adopted checkout");

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

    // The adopted checkout is the engine's, held for the convoy; it is seen, and landing never
    // removes it (#3108).
    let convoy = convoys.get(&adopted_record).await.expect("landed convoy");
    let holder = flotilla_resources::convoy::engine::checkout_holder(&convoy).expect("held for its launch");
    let held = daemon.lifecycle_reads().checkouts().expect("checkouts").held(&holder).expect("held checkouts");
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(
        matches!(&held[0].spec, CheckoutSpec::Observed(spec) if std::path::Path::new(&spec.path) == repo.canonicalize().expect("repo"))
    );
    assert!(repo.join(".git").exists(), "adopted checkout should remain after completion");

    for handle in controller_handles {
        handle.abort();
        let _ = handle.await;
    }
}
