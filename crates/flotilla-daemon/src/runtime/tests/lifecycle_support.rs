use super::*;

#[derive(Clone, Copy)]
pub(super) enum CompletionAction {
    Retain,
    Delete,
}

pub(super) struct NoPrProcessRunner;

pub(super) struct Stage4aGitRunner {
    pub(super) inner: Arc<dyn CommandRunner>,
    pub(super) upstream: PathBuf,
}

impl Stage4aGitRunner {
    pub(super) fn local_transport_args(&self, cmd: &str, args: &[&str]) -> Option<Vec<String>> {
        if cmd != "git" {
            return None;
        }
        let mut command_start = if matches!(args, ["-C", _, ..]) { 2 } else { 0 };
        if args.get(command_start).is_some_and(|arg| arg.starts_with("--git-dir=")) {
            command_start += 1;
        }
        let remote_index = command_start
            + match &args[command_start..] {
                ["ls-remote", "--heads" | "--refs", "origin", ..] => 2,
                ["fetch", "origin", ..] => 1,
                _ => return None,
            };
        let mut local = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        local[remote_index] = self.upstream.to_string_lossy().into_owned();
        Some(local)
    }
}

// Git transport boundary: retain the declared GitHub identity, but enforce
// branch/ref behavior against a real local repository instead of the network.
#[async_trait]
impl CommandRunner for Stage4aGitRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        match self.local_transport_args(cmd, args) {
            Some(local) => ProcessCommandRunner.run(cmd, &local.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await,
            None => self.inner.run(cmd, args, cwd, label).await,
        }
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.local_transport_args(cmd, args) {
            Some(local) => ProcessCommandRunner.run_output(cmd, &local.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await,
            None => self.inner.run_output(cmd, args, cwd, label).await,
        }
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.inner.exists(cmd, args).await
    }
}

#[async_trait]
impl CommandRunner for NoPrProcessRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "gh" {
            Ok("[]".to_string())
        } else {
            ProcessCommandRunner.run(cmd, args, cwd, label).await
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        if cmd == "gh" {
            Ok(CommandOutput { stdout: "[]".to_string(), stderr: String::new(), exit_code: Some(0) })
        } else {
            ProcessCommandRunner.run_output(cmd, args, cwd, label).await
        }
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }
}

pub(super) async fn run_stage4a_flow_reaches_running_and_completes_convoy(
    daemon: Arc<InProcessDaemon>,
    config: Arc<ConfigStore>,
    repo_default_dir: PathBuf,
    completion_action: CompletionAction,
) {
    std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
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
    let controller_handles = spawn_controller_loops(
        Arc::clone(&state),
        NAMESPACE,
        Duration::from_secs(3600),
        ControllerSupervision::default(),
        RuntimeHealth::default(),
    );
    // Startup realises the host's environment before work arrives. Nothing holds a host-direct
    // environment, so its realisation wakes no vessel: one placed before it waits for the resync.
    let host_environment = profile.host_direct_environment_name();
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !daemon
        .lifecycle_reads()
        .environments()
        .and_then(|environments| environments.find(&host_environment))
        .expect("host environment")
        .is_some_and(|environment| environment.is_ready())
    {
        assert!(tokio::time::Instant::now() < ready_deadline, "the host environment {host_environment} was never realised");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

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
                        .source(CrewSource::Tool { command: "bash -lc 'echo stage4a'".to_string() })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow template create should succeed");
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("convoy-a".to_string())
                .labels(BTreeMap::from([
                    (flotilla_resources::PROJECT_LABEL.to_string(), "test".to_string()),
                    (flotilla_resources::ROLE_LABEL.to_string(), "convoy-a".to_string()),
                    (flotilla_resources::GENERATION_LABEL.to_string(), "1".to_string()),
                ]))
                .build(),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: "convoy-a".to_string(),
                generation: 1,
                workflow_ref: "wf-a".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: Some(format!("host-direct-{host_id}")),
                repositories: vec![ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla.git".to_string(),
                    repo_ref: repository_key,
                    source_ref: "main".to_string(),
                    target_ref: "main".to_string(),
                    workspace_slug: repository_spec.leaf_slug(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("stage4a-work".to_string()),
                project_ref: Some("test".to_string()),
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy create should succeed");

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let run_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            convoys.get("convoy-a").await.ok().and_then(|convoy| convoy.status).as_ref(),
            Some(status)
                if status.phase == ConvoyPhase::Active
                    && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
        ) {
            break;
        }
        if tokio::time::Instant::now() >= run_deadline {
            let convoy = convoys.get("convoy-a").await.expect("convoy should exist");
            let workspace = backend.clone().using::<Vessel>(NAMESPACE).list().await.expect("workspace list should succeed");
            panic!("convoy did not reach running state: convoy={convoy:?} vessels={workspace:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let host = backend.clone().using::<Host>(NAMESPACE).get(&host_id).await.expect("host should exist after startup");
    assert!(host.status.is_some(), "startup heartbeat should publish host status");

    let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
    let sqlite_path = config.state_dir().as_path().join("resources.sqlite");
    let idle_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut previous_idle_sample = None;
    loop {
        let workspace = workspaces.get("convoy-a-implement").await.expect("steady workspace should remain");
        let sample = (
            workspace.metadata.resource_version,
            workspace.status.expect("steady workspace status").ready_at,
            sqlite_path.exists().then(|| sqlite_max_event_rowid(&sqlite_path)),
        );
        if previous_idle_sample.as_ref() == Some(&sample) {
            break;
        }
        assert!(tokio::time::Instant::now() < idle_deadline, "resource store did not reach an idle fixed point");
        previous_idle_sample = Some(sample);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let teardown_checkout_path = if matches!(completion_action, CompletionAction::Delete) {
        // The convoy's checkout, held for its engine Convoy.
        let holder = flotilla_resources::checkout_records::CheckoutHolder::Convoy("convoy-a".to_string());
        let checkout = daemon
            .lifecycle_reads()
            .checkouts()
            .and_then(|checkouts| checkouts.held(&holder))
            .expect("held checkouts")
            .into_iter()
            .find(|checkout| !checkout.ended())
            .expect("convoy checkout should exist");
        let checkout_path = checkout.ready_path().expect("checkout should be ready under its path").to_owned();
        for args in [
            ["update-ref", "refs/remotes/origin/main", "HEAD"].as_slice(),
            ["branch", "--set-upstream-to", "origin/main", "stage4a-work"].as_slice(),
        ] {
            let status = ProcessCommand::new("git").arg("-C").arg(&checkout_path).args(args).status().expect("prepare pushed state");
            assert!(status.success());
        }
        Some(checkout_path)
    } else {
        None
    };

    record_successful_empty_branch_scan(&backend, "convoy-a").await;
    daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-a@test".to_string(),
                    work: "implement".to_string(),
                    message: Some(match completion_action {
                        CompletionAction::Delete => "https://github.com/flotilla-org/flotilla/pull/884".to_string(),
                        CompletionAction::Retain => "done".to_string(),
                    }),
                })
                .build(),
        )
        .await
        .expect("convoy completion command should succeed");
    if matches!(completion_action, CompletionAction::Delete) {
        publish_merged_change_request(&backend, 884, "test-host").await;
    }

    wait_until(|| {
        let convoys = convoys.clone();
        async move {
            matches!(
                convoys.get("convoy-a").await.ok().and_then(|convoy| convoy.status).as_ref(),
                Some(status)
                    if status.phase == ConvoyPhase::Landed
                        && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Complete)
            )
        }
    })
    .await;

    if let Some(checkout_path) = teardown_checkout_path {
        wait_until(|| {
            let convoys = convoys.clone();
            let workspaces = workspaces.clone();
            let checkout_path = checkout_path.clone();
            async move {
                convoys
                    .get("convoy-a")
                    .await
                    .ok()
                    .and_then(|convoy| convoy.status)
                    .is_some_and(|status| status.phase == ConvoyPhase::Landed)
                    && workspaces.list().await.is_ok_and(|list| list.items.is_empty())
                    && !Path::new(&checkout_path).exists()
            }
        })
        .await;
    }

    if matches!(completion_action, CompletionAction::Delete) {
        wait_until(|| {
            let terminals = backend.using::<TerminalSession>(NAMESPACE);
            // Ended launches stay as history: each session's newest launch is released.
            async move {
                terminals.list().await.is_ok_and(|list| {
                    list.items.iter().all(|session| {
                        session.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::TerminalSessionPhase::Stopped)
                    })
                })
            }
        })
        .await;
    }

    for handle in controller_handles {
        handle.abort();
        let _ = handle.await;
    }
}

pub(super) async fn record_successful_empty_branch_scan(backend: &ResourceBackend, convoy: &str) {
    flotilla_store::apply_status_patch(
        &backend.clone().using::<Convoy>(NAMESPACE),
        convoy,
        &flotilla_resources::ConvoyStatusPatch::RecordBranchSubjectScan { at: chrono::Utc::now() },
    )
    .await
    .expect("record empty branch discovery for test provider");
}

pub(super) async fn convoy_record_name(backend: &ResourceBackend, role: &str) -> String {
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .list_matching_labels(&BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_string(), role.to_string())]))
        .await
        .expect("list convoy by role")
        .items
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("convoy role {role}"))
        .metadata
        .name
}
