use super::*;

pub(super) async fn crew_provisioning_recovery_scenario() {
    let temp = TempDir::new().expect("tempdir");
    let repo = TestGitRepo::init(temp.path().join("repo"))
        .with_initial_commit()
        .with_origin("git@github.com:flotilla-org/flotilla.git")
        .path()
        .to_path_buf();
    let config_path = temp.path().join("config");
    std::fs::create_dir_all(&config_path).expect("config dir");
    std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_path));
    let (daemon, pool) = crew_daemon(Arc::clone(&config)).await;
    let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
    assert!(local_registry.agent_adapters.get("codex").is_some());
    assert!(local_registry.agent_adapters.get("claude-code").is_some());
    let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
    let backend = daemon.resource_backend();

    register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("startup resources");
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("host heartbeat");
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        Arc::clone(&config),
        local_registry,
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let controller_handles = spawn_controller_loops(
        Arc::clone(&state),
        NAMESPACE,
        Duration::from_millis(20),
        ControllerSupervision::default(),
        RuntimeHealth::default(),
    );

    backend
        .clone()
        .using::<WorkflowTemplate>(NAMESPACE)
        .create(
            &empty_meta("crew-workflow"),
            &WorkflowTemplateSpec::builder()
                .exit(flotilla_resources::ExitDeclaration::standard_table())
                .inputs(Vec::new())
                .vessels(vec![VesselRequirement::builder()
                    .name("implement".to_string())
                    .crew(vec![
                        CrewSpec::builder()
                            .role("coder".to_string())
                            .completion_conditions(vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
                                "coder",
                                "decision-ledger",
                                flotilla_resources::ArtifactSubjectBinding::Convoy,
                            )])
                            .source(CrewSource::Agent {
                                selector: Selector::for_capability("coding"),
                                prompt: Some("Implement issue 668 without leaking this full brief into the launch command.".to_string()),
                                brief_template: None,
                            })
                            .build(),
                        CrewSpec::builder()
                            .role("reviewer".to_string())
                            .source(CrewSource::Agent {
                                selector: Selector { capability: "review".to_string(), adapter: Some("codex".to_string()), model: None },
                                prompt: Some("Review the coder's work.".to_string()),
                                brief_template: None,
                            })
                            .build(),
                        CrewSpec::builder()
                            .role("watcher".to_string())
                            .source(CrewSource::Tool { command: "cargo test --watch".to_string() })
                            .build(),
                    ])
                    .build()])
                .build(),
        )
        .await
        .expect("crew workflow");

    let mut rx = daemon.subscribe();
    let create_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "crew-convoy".to_string(),
                    workflow_ref: "crew-workflow".to_string(),
                    inputs: Vec::new(),
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".to_string()),
                    r#ref: Some("main".to_string()),
                    project_ref: None,
                    placement_policy: Some(profile.host_direct_policy_name()),
                    adopted_checkout: Some(Box::new(repo.clone())),
                })
                .build(),
        )
        .await
        .expect("create crew convoy");
    assert_eq!(wait_for_command_result(&mut rx, create_id).await, CommandValue::ConvoyCreated { name: "crew-convoy".to_string() });

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let crew_record = convoy_record_name(&backend, "crew-convoy").await;
    wait_until(|| {
        let convoys = convoys.clone();
        let crew_record = crew_record.clone();
        async move {
            matches!(
                convoys.get(&crew_record).await.ok().and_then(|convoy| convoy.status).as_ref(),
                Some(status)
                    if status.phase == ConvoyPhase::Active
                        && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
            )
        }
    })
    .await;

    let terminals = backend.clone().using::<TerminalSession>(NAMESPACE);
    let coder = terminals
        .list()
        .await
        .expect("terminal list")
        .items
        .into_iter()
        .find(|session| session.spec.role == "coder")
        .expect("coder session");
    let TerminalSessionSource::Agent { context, .. } = &coder.spec.source else { panic!("coder must be an agent") };
    let coder_id = coder.status.as_ref().and_then(|status| status.crew.as_ref()).expect("coder identity").id.clone();
    assert_eq!(coder.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.adapter.as_str()), Some("codex"));
    assert!(terminals.list().await.expect("terminal list").items.iter().any(|session| session.spec.role == "watcher"));
    assert!(terminals.list().await.expect("terminal list").items.iter().all(|session| session.spec.role != "reviewer"));
    let ensured = pool.ensured.lock().await;
    let coder_launch = ensured.iter().find(|launch| launch.session_name.ends_with("-coder")).expect("coder launch");
    assert!(coder_launch.command.contains("--dangerously-bypass-approvals-and-sandbox"));
    // #2821: the complete rendered brief now appears in the launch prompt.
    assert!(coder_launch.command.contains("without leaking this full brief"));
    assert!(coder_launch.command.contains("This is also at"));
    for (name, expected) in [
        ("FLOTILLA_CREW_ID", coder_id.as_str()),
        ("FLOTILLA_CONVOY", context.convoy.as_str()),
        ("FLOTILLA_VESSEL", context.vessel_ref.as_str()),
        ("FLOTILLA_CREW_ROLE", "coder"),
        ("FLOTILLA_NAMESPACE", NAMESPACE),
        ("FLOTILLA_TERMINAL_SESSION", coder_launch.session_name.as_str()),
        ("GIT_AUTHOR_NAME", "flotilla-crew[bot]"),
        ("GIT_AUTHOR_EMAIL", "309902803+flotilla-crew[bot]@users.noreply.github.com"),
        ("GIT_COMMITTER_NAME", "flotilla-crew[bot]"),
        ("GIT_COMMITTER_EMAIL", "309902803+flotilla-crew[bot]@users.noreply.github.com"),
    ] {
        assert!(coder_launch.env_vars.iter().any(|(key, value)| key == name && value == expected), "host-direct launch missing {name}");
    }
    assert!(
        !coder_launch.env_vars.iter().any(|(key, _)| matches!(key.as_str(), "GH_TOKEN" | "GITHUB_TOKEN_FILE" | "GIT_CONFIG_GLOBAL")),
        "ungranted host-direct crew must remain credential-less"
    );
    assert!(coder_launch.env_vars.iter().any(|(key, value)| key == "CARGO_INCREMENTAL" && value == "0"));
    assert!(coder_launch
        .env_vars
        .iter()
        .any(|(key, value)| key == "CARGO_BUILD_JOBS" && value.parse::<usize>().is_ok_and(|jobs| jobs >= 2)));
    assert!(coder_launch.env_vars.iter().any(|(key, value)| key == "RUSTC_WORKSPACE_WRAPPER" && Path::new(value).is_file()));
    let watcher_launch = ensured.iter().find(|launch| launch.session_name.ends_with("-watcher")).expect("watcher launch");
    assert!(watcher_launch.env_vars.iter().any(|(key, value)| key == "CARGO_INCREMENTAL" && value == "0"));
    assert!(watcher_launch
        .env_vars
        .iter()
        .any(|(key, value)| key == "CARGO_BUILD_JOBS" && value.parse::<usize>().is_ok_and(|jobs| jobs >= 2)));
    assert!(watcher_launch.env_vars.iter().any(|(key, value)| key == "RUSTC_WORKSPACE_WRAPPER" && Path::new(value).is_file()));
    drop(ensured);

    let crew_context = CrewCommandContext { crew_id: Some(coder_id.clone()), ..Default::default() };
    let crew_list = daemon
        .execute_query(
            Command::builder().action(CommandAction::QueryCrewList { context: crew_context.clone() }).build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("crew list");
    let CommandValue::CrewList(crew_list) = crew_list else { panic!("expected crew list") };
    assert_eq!(
        crew_list.members.iter().map(|member| (member.role.as_str(), member.state.as_str())).collect::<Vec<_>>(),
        vec![("coder", "active"), ("reviewer", "latent"), ("watcher", "active")]
    );
    let initial_status = convoys.get(&crew_record).await.expect("crew convoy").status.expect("convoy status");
    assert_eq!(initial_status.crew_work["implement"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
    // The reviewer is latent above and has no terminal session yet, so the
    // convoy status has to agree rather than report a crew member that was
    // never launched as working.
    assert_eq!(initial_status.crew_work["implement"]["reviewer"].phase, flotilla_resources::CrewWorkPhase::Pending);
    assert_eq!(initial_status.crew_work["implement"]["reviewer"].started_at, None);
    assert!(!initial_status.crew_work["implement"].contains_key("watcher"));

    let mut rx = daemon.subscribe();
    let refused_complete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: crew_context.clone(),
                    message: Some("delegate attempted completion".to_string()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: false,
                })
                .build(),
        )
        .await
        .expect("dispatch refused completion");
    assert!(matches!(wait_for_command_result(&mut rx, refused_complete_id).await,
            CommandValue::Error { message } if message.contains("decision-ledger") && message.contains(".exists")));
    let after_refusal = convoys.get(&crew_record).await.expect("crew convoy after refusal").status.expect("convoy status after refusal");
    assert_eq!(after_refusal.phase, initial_status.phase);
    assert_eq!(after_refusal.crew_work["implement"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
    assert!(after_refusal.crew_work["implement"]["coder"]
        .completion_refusal
        .as_ref()
        .is_some_and(|refusal| { refusal.consecutive_count == 1 && refusal.expectation.contains("decision-ledger") }));
    assert_eq!(
        terminals.get(&coder.metadata.name).await.expect("coder session after refusal").status.expect("coder session status").phase,
        TerminalSessionPhase::Running,
        "refused completion must leave the crew session alive"
    );

    let ledger_name = flotilla_resources::artifact_record_name(&crew_record, "coder", "decision-ledger", &crew_record);
    backend
        .using::<flotilla_resources::Artifact>(NAMESPACE)
        .create(
            &empty_meta(&ledger_name),
            &flotilla_resources::ArtifactSpec::builder()
                .convoy(crew_record.clone())
                .producer("coder".to_string())
                .kind("decision-ledger".to_string())
                .subject(crew_record.clone())
                .summary(BTreeMap::from([(
                    "comment_url".to_string(),
                    serde_json::json!("https://github.com/flotilla-org/flotilla/pull/1875#issuecomment-coder"),
                )]))
                .digest("test-ledger-digest".to_string())
                .size(1)
                .media_type("text/markdown".to_string())
                .expires_at(Utc::now() + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("publish coder ledger artifact");

    let mut rx = daemon.subscribe();
    let coder_complete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: crew_context.clone(),
                    message: Some("implementation ready".to_string()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: false,
                })
                .build(),
        )
        .await
        .expect("coder complete");
    assert_eq!(wait_for_command_result(&mut rx, coder_complete_id).await, CommandValue::Ok);
    assert!(
        convoys.get(&crew_record).await.expect("crew convoy").status.expect("convoy status").crew_work["implement"]["coder"]
            .completed_while_crew_active,
        "completion while the agent process is running must remain visible on convoy status"
    );

    let mut rx = daemon.subscribe();
    let handoff_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewHandoff {
                    carries: Vec::new(),
                    context: crew_context.clone(),
                    target: "reviewer".to_string(),
                    message: "Review commit abc123".to_string(),
                })
                .build(),
        )
        .await
        .expect("handoff reviewer");
    assert_eq!(wait_for_command_result(&mut rx, handoff_id).await, CommandValue::Ok);
    wait_until(|| {
        let terminals = terminals.clone();
        async move {
            terminals
                .list()
                .await
                .ok()
                .and_then(|list| list.items.into_iter().find(|session| session.spec.role == "reviewer"))
                .and_then(|session| session.status)
                .is_some_and(|status| status.phase == TerminalSessionPhase::Running)
        }
    })
    .await;
    let reviewer = terminals
        .list()
        .await
        .expect("terminal list")
        .items
        .into_iter()
        .find(|session| session.spec.role == "reviewer")
        .expect("reviewer session");
    let reviewer_id = reviewer.status.as_ref().and_then(|status| status.crew.as_ref()).expect("reviewer identity").id.clone();
    assert_eq!(reviewer.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.adapter.as_str()), Some("codex"));
    pool.set_captured_screen(&reviewer.metadata.name, "› Ask Codex to do anything").await;
    wait_until(|| {
        let pool = Arc::clone(&pool);
        async move {
            pool.delivered
                .lock()
                .await
                .iter()
                .any(|(session, text, submit)| session.ends_with("-reviewer") && text.contains("Review commit abc123") && *submit)
        }
    })
    .await;

    pool.set_captured_screen(&reviewer.metadata.name, "• Working (1s • esc to interrupt)\n› Ask Codex to do anything").await;
    let mut rx = daemon.subscribe();
    let hand_back_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewHandoff {
                    carries: Vec::new(),
                    context: CrewCommandContext { crew_id: Some(reviewer_id.clone()), ..Default::default() },
                    target: "coder".to_string(),
                    message: "Address the review findings".to_string(),
                })
                .build(),
        )
        .await
        .expect("hand back to coder");
    assert_eq!(wait_for_command_result(&mut rx, hand_back_id).await, CommandValue::Ok);
    let messages = backend.using::<flotilla_resources::Message>(NAMESPACE);
    let coder_delivery_id = messages
        .list()
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|message| message.spec.receiver.ends_with("/implement/coder") && message.spec.body.ends_with("Address the review findings"))
        .expect("running coder handoff should be accepted in its durable inbox")
        .metadata
        .name;
    pool.set_captured_screen(&coder.metadata.name, "› Ask Codex to do anything").await;
    wait_until(|| {
        let pool = Arc::clone(&pool);
        async move {
            pool.delivered
                .lock()
                .await
                .iter()
                .any(|(session, text, submit)| session.ends_with("-coder") && text.contains("Address the review findings") && *submit)
        }
    })
    .await;
    pool.set_captured_screen(&coder.metadata.name, "• Working (1s • esc to interrupt)\n› Ask Codex to do anything").await;
    wait_until_with_timeout(Duration::from_secs(5), || {
        let messages = messages.clone();
        let coder_delivery_id = coder_delivery_id.clone();
        async move {
            messages
                .get(&coder_delivery_id)
                .await
                .ok()
                .and_then(|message| message.status)
                .is_some_and(|status| status.phase.has_delivery_evidence())
        }
    })
    .await;
    wait_until(|| {
        let convoys = convoys.clone();
        let crew_record = crew_record.clone();
        async move {
            convoys.get(&crew_record).await.ok().and_then(|convoy| convoy.status).is_some_and(|status| {
                status.phase == ConvoyPhase::Active && status.work.get("implement").is_some_and(|work| work.phase == WorkPhase::Running)
            })
        }
    })
    .await;
    let reopened = convoys.get(&crew_record).await.expect("reopened convoy").status.expect("reopened status");
    assert_eq!(reopened.crew_work["implement"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
    assert_eq!(reopened.crew_work["implement"]["reviewer"].phase, flotilla_resources::CrewWorkPhase::HandedBack);

    for handle in controller_handles {
        handle.abort();
        let _ = handle.await;
    }
    pool.remove_session(&coder.metadata.name).await;
    let listed_before_restart = convoys.list().await.expect("convoys before daemon restart");
    let mut convoy_watch =
        convoys.watch(flotilla_resources::WatchStart::resuming_from(&listed_before_restart)).await.expect("watch convoy recovery");
    let (daemon, pool) = crew_daemon_with_backend(Arc::clone(&config), backend.clone()).await;
    let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry after restart");
    let profile = build_local_profile(&daemon, &local_registry).expect("local profile after restart");
    let surviving_sessions = terminals
        .list()
        .await
        .expect("persisted terminal resources")
        .items
        .into_iter()
        .filter(|session| session.metadata.name != coder.metadata.name)
        .map(|session| {
            flotilla_core::providers::terminal::TerminalSession::builder()
                .session_name(session.metadata.name)
                .status(TerminalStatus::Running)
                .command("persisted process".to_string())
                .working_directory(ExecutionEnvironmentPath::new(session.spec.cwd))
                .build()
        })
        .collect();
    pool.add_sessions(surviving_sessions).await;
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        Arc::clone(&config),
        local_registry,
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let controller_handles = spawn_controller_loops(
        Arc::clone(&state),
        NAMESPACE,
        Duration::from_millis(20),
        ControllerSupervision::default(),
        RuntimeHealth::default(),
    );
    wait_until(|| {
        let terminals = terminals.clone();
        let name = coder.metadata.name.clone();
        let old_id = coder_id.clone();
        async move {
            terminals
                .get(&name)
                .await
                .ok()
                .and_then(|session| session.status)
                .is_some_and(|status| status.phase == TerminalSessionPhase::Running && status.crew.is_some_and(|crew| crew.id != old_id))
        }
    })
    .await;
    wait_until(|| {
        let convoys = convoys.clone();
        let crew_record = crew_record.clone();
        async move {
            convoys.get(&crew_record).await.ok().and_then(|convoy| convoy.status).is_some_and(|status| {
                status.phase == ConvoyPhase::Active
                    && status.work["implement"].phase == WorkPhase::Running
                    && status.crew_work["implement"]["coder"].phase == flotilla_resources::CrewWorkPhase::Working
            })
        }
    })
    .await;
    let mut saw_interrupted_work = false;
    let mut saw_interrupted_convoy = false;
    while !(saw_interrupted_work && saw_interrupted_convoy) {
        let event = tokio::time::timeout(Duration::from_secs(1), convoy_watch.next())
            .await
            .expect("interruption should be durably observable")
            .expect("convoy watch should remain open")
            .expect("convoy watch event");
        let convoy = match event {
            flotilla_resources::WatchEvent::Added(convoy) | flotilla_resources::WatchEvent::Modified(convoy) => convoy,
            flotilla_resources::WatchEvent::Deleted(_) | flotilla_resources::WatchEvent::DeletedByName(_) => continue,
        };
        let Some(status) = convoy.status else { continue };
        saw_interrupted_work |= status.work.get("implement").is_some_and(|work| work.phase == WorkPhase::Interrupted);
        saw_interrupted_convoy |= status.phase == ConvoyPhase::Interrupted;
    }
    let revived_coder = terminals.get(&coder.metadata.name).await.expect("revived coder");
    let revived_coder_id = revived_coder.status.as_ref().and_then(|status| status.crew.as_ref()).expect("revived identity").id.clone();
    let reviewer = terminals
        .list()
        .await
        .expect("terminal list after restart")
        .items
        .into_iter()
        .find(|session| session.spec.role == "reviewer")
        .expect("reviewer session after restart");
    let reviewer_id = reviewer.status.as_ref().and_then(|status| status.crew.as_ref()).expect("revived reviewer identity").id.clone();

    let attach = daemon.resolve_attach_command_internal("crew-convoy/implement/coder").await.expect("attach coder");
    let [flotilla_protocol::ResolvedAttachAction::Command(args)] = attach.plan.0.as_slice() else {
        panic!("expected one local attach command, got {:?}", attach.plan);
    };
    assert!(flotilla_protocol::arg::flatten(args, 0).contains(&format!("attach --take {}", revived_coder.metadata.name)));

    let ledgers = backend.using::<flotilla_resources::Artifact>(NAMESPACE);
    let prior_ledger = ledgers.get(&ledger_name).await.expect("coder ledger");
    let mut revised_ledger = prior_ledger.spec.clone();
    revised_ledger.digest = "test-ledger-revision".to_string();
    revised_ledger
        .summary
        .insert("comment_url".to_string(), serde_json::json!("https://github.com/flotilla-org/flotilla/pull/1875#issuecomment-recomplete"));
    ledgers
        .update(&empty_meta(&ledger_name), &prior_ledger.metadata.resource_version, &revised_ledger)
        .await
        .expect("publish revised coder ledger");

    let mut rx = daemon.subscribe();
    let coder_recomplete_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some(revived_coder_id.clone()), ..Default::default() },
                    message: Some("review findings addressed".to_string()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: false,
                })
                .build(),
        )
        .await
        .expect("coder re-complete");
    assert_eq!(wait_for_command_result(&mut rx, coder_recomplete_id).await, CommandValue::Ok);

    let mut rx = daemon.subscribe();
    let return_to_reviewer_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewHandoff {
                    carries: Vec::new(),
                    context: CrewCommandContext { crew_id: Some(revived_coder_id), ..Default::default() },
                    target: "reviewer".to_string(),
                    message: "Please verify the fixes".to_string(),
                })
                .build(),
        )
        .await
        .expect("return to reviewer");
    assert_eq!(wait_for_command_result(&mut rx, return_to_reviewer_id).await, CommandValue::Ok);

    let mut rx = daemon.subscribe();
    let checkouts = backend.clone().using::<ResourceCheckout>(NAMESPACE);
    let checkout = checkouts
        .list()
        .await
        .expect("checkout list")
        .items
        .into_iter()
        .find(|checkout| checkout.metadata.lifecycle_authority().ok().flatten() == Some(LifecycleAuthority::Adopted))
        .expect("adopted checkout");
    let mut integration = checkout.status.expect("checkout status").integration;
    integration.landed = flotilla_resources::IntegrationCondition::builder()
        .value(flotilla_resources::ConditionValue::True)
        .details(vec!["no change request exists for branch".to_string()])
        .observed_at(Utc::now().to_rfc3339())
        .build();
    flotilla_resources::apply_status_patch(
        &checkouts,
        &checkout.metadata.name,
        &flotilla_resources::CheckoutStatusPatch::UpdateIntegration { integration: Box::new(integration) },
    )
    .await
    .expect("record observed absence of a change request");
    record_successful_empty_branch_scan(&backend, &crew_record).await;
    let final_review_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some(reviewer_id), ..Default::default() },
                    message: Some("changes accepted".to_string()),
                    disposition: None,
                    decision_ledger_ref: Some("https://github.com/flotilla-org/flotilla/pull/1875#issuecomment-reviewer".to_string()),
                    force: false,
                })
                .build(),
        )
        .await
        .expect("final reviewer completion");
    assert_eq!(wait_for_command_result(&mut rx, final_review_id).await, CommandValue::Ok);
    wait_until(|| {
        let convoys = convoys.clone();
        let crew_record = crew_record.clone();
        async move {
            convoys.get(&crew_record).await.ok().and_then(|convoy| convoy.status).is_some_and(|status| status.phase == ConvoyPhase::Landed)
        }
    })
    .await;
    let completed = convoys.get(&crew_record).await.expect("completed convoy").status.expect("completed status");
    assert_eq!(completed.work["implement"].phase, WorkPhase::Complete);
    assert!(completed.crew_work["implement"].values().all(|state| state.phase == flotilla_resources::CrewWorkPhase::Done));

    backend
        .clone()
        .using::<WorkflowTemplate>(NAMESPACE)
        .create(
            &empty_meta("unknown-capability"),
            &WorkflowTemplateSpec::builder()
                .exit(flotilla_resources::ExitDeclaration::standard_table())
                .inputs(Vec::new())
                .vessels(vec![VesselRequirement::builder()
                    .name("implement".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("architect".to_string())
                        .source(CrewSource::Agent { selector: Selector::for_capability("architect"), prompt: None, brief_template: None })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("unknown capability workflow");
    let mut rx = daemon.subscribe();
    let create_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "unknown-convoy".to_string(),
                    workflow_ref: "unknown-capability".to_string(),
                    inputs: Vec::new(),
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".to_string()),
                    r#ref: Some("main".to_string()),
                    project_ref: None,
                    placement_policy: Some(profile.host_direct_policy_name()),
                    adopted_checkout: Some(Box::new(repo)),
                })
                .build(),
        )
        .await
        .expect("create unknown convoy");
    assert_eq!(
        wait_for_command_result(&mut rx, create_id).await,
        CommandValue::Error { message: "unknown agent capability `architect`".to_string() }
    );
    assert!(convoys.get("unknown-convoy").await.is_err(), "rejected convoy should not be persisted");

    for handle in controller_handles {
        handle.abort();
        let _ = handle.await;
    }
}
