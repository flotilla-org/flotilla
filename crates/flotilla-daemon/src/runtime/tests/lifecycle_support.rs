use super::*;

#[derive(Clone, Copy)]
pub(super) enum CompletionAction {
    Retain,
    Delete,
    /// An operator deletes the convoy while its work is still running.
    DeleteLive,
    /// The work commits on its branch without pushing, leaving an untracked file when `untracked`,
    /// and completes naming change request 884, which the forge observer then reads in `state` at
    /// the checkout's head.
    ChangeRequest {
        state: flotilla_resources::ObservedChangeRequestState,
        untracked: bool,
    },
}

/// What became of a convoy's checkout once its change request was read.
#[derive(Debug)]
pub(super) struct ReleaseSeen {
    /// The checkout launch's reading, last read.
    pub(super) reading: LifecycleReading,
    /// Its recorded inspection's `landed`, and why.
    pub(super) landed: Option<bool>,
    pub(super) landed_details: Vec<String>,
    /// The worktree is still on disk.
    pub(super) on_disk: bool,
    /// Archives saved under the test's directory.
    pub(super) archives: usize,
    /// The convoy's phase, last read.
    pub(super) convoy: Option<ConvoyPhase>,
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
                ["ls-remote", "--symref", _, "HEAD"] => 2,
                ["fetch", "origin", ..] => 1,
                _ => return None,
            };
        let mut local = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        local[remote_index] = self.upstream.to_string_lossy().into_owned();
        Some(local)
    }

    /// Git naming a remote URL this runner does not route locally would reach the network.
    fn refuse_network(cmd: &str, args: &[&str]) -> Result<(), String> {
        let remote = |arg: &&&str| ["https://", "http://", "ssh://", "git://", "git@"].iter().any(|scheme| arg.starts_with(scheme));
        match args.iter().find(remote) {
            Some(url) if cmd == "git" => Err(format!("test git runner refuses to reach {url}: git {}", args.join(" "))),
            _ => Ok(()),
        }
    }
}

// Git transport boundary: retain the declared GitHub identity, but enforce
// branch/ref behavior against a real local repository instead of the network.
#[async_trait]
impl CommandRunner for Stage4aGitRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        match self.local_transport_args(cmd, args) {
            Some(local) => ProcessCommandRunner.run(cmd, &local.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await,
            None => {
                Self::refuse_network(cmd, args)?;
                self.inner.run(cmd, args, cwd, label).await
            }
        }
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.local_transport_args(cmd, args) {
            Some(local) => ProcessCommandRunner.run_output(cmd, &local.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await,
            None => {
                Self::refuse_network(cmd, args)?;
                self.inner.run_output(cmd, args, cwd, label).await
            }
        }
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.inner.exists(cmd, args).await
    }
    // A forced removal writes its archive's manifest through the runner, on the local disk.
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        ProcessCommandRunner.write_file(path, content).await
    }
}

#[async_trait]
impl CommandRunner for NoPrProcessRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "gh" && args.ends_with(&["--jq", ".default_branch"]) {
            // The forge names its default branch.
            Ok("main\n".to_string())
        } else if cmd == "gh" {
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
) -> Option<ReleaseSeen> {
    std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = LocalProvisioningProfile { repo_default_dir: repo_default_dir.display().to_string(), ..manual_profile(&host_id, false) };
    let backend = daemon.resource_backend();

    let provisioning = engine_startup(&daemon, NAMESPACE, &profile).await.expect("startup registration should succeed");
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("host heartbeat should succeed");

    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        Arc::clone(&config),
        passthrough_registry(),
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let controller_handles =
        spawn_controller_loops(Arc::clone(&state), &provisioning, NAMESPACE, ControllerSupervision::default(), RuntimeHealth::default());
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
                        .promises(match completion_action {
                            // The coder owes its PR, as a deliverer does.
                            CompletionAction::ChangeRequest { .. } => vec![
                                flotilla_resources::TemplatePromise::decision_ledger("coder"),
                                flotilla_resources::TemplatePromise {
                                    kind: flotilla_resources::promises::PromiseKind::Pr,
                                    owner: "coder".to_string(),
                                },
                            ],
                            _ => vec![flotilla_resources::TemplatePromise::decision_ledger("coder")],
                        })
                        .source(CrewSource::Tool { command: "bash -lc 'echo stage4a'".to_string() })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow template create should succeed");
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("repository spec");
    flotilla_store_testkit::fixtures::project_on_repository(&backend, NAMESPACE, NAMESPACE, &repository_spec, "main").await;
    let mut rx = daemon.subscribe();
    let create_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "convoy-a".to_string(),
                    workflow_ref: "wf-a".to_string(),
                    inputs: Vec::new(),
                    r#ref: Some("stage4a-work".to_string()),
                    project_ref: NAMESPACE.to_string(),
                    placement_policy: Some(format!("host-direct-{host_id}")),
                    adopted_checkout: None,
                })
                .build(),
        )
        .await
        .expect("convoy create command should start");
    let created = wait_for_command_result(&mut rx, create_id).await;
    let convoy_name = convoy_record_name(&backend, "convoy-a").await;
    assert_eq!(created, created_launch(NAMESPACE, &convoy_name));

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let run_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            convoys.get(&convoy_name).await.ok().and_then(|convoy| convoy.status).as_ref(),
            Some(status)
                if status.phase == ConvoyPhase::Active
                    && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
        ) {
            break;
        }
        if tokio::time::Instant::now() >= run_deadline {
            let convoy = convoys.get(&convoy_name).await.expect("convoy should exist");
            let workspace = backend.clone().using::<Vessel>(NAMESPACE).list().await.expect("workspace list should succeed");
            panic!("convoy did not reach running state: convoy={convoy:?} vessels={workspace:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let host = backend.clone().using::<Host>(NAMESPACE).get(&host_id).await.expect("host should exist after startup");
    assert!(host.status.is_some(), "startup heartbeat should publish host status");

    let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
    // The work's vessel is its launch, named for it.
    let launch = daemon
        .lifecycle_reads()
        .vessels()
        .and_then(|vessels| vessels.of_convoy(&convoy_name))
        .expect("convoy launches")
        .into_iter()
        .find(|launch| launch.work() == "implement" && !launch.ended())
        .expect("the work's launch")
        .name;
    let sqlite_path = config.state_dir().as_path().join("resources.sqlite");
    let idle_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut previous_idle_sample = None;
    loop {
        let workspace = workspaces.get(&launch).await.expect("steady workspace should remain");
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

    if let CompletionAction::ChangeRequest { state: request_state, untracked } = completion_action {
        let seen = release_after_change_request(&daemon, &convoy_name, &launch, &repo_default_dir, request_state, untracked).await;
        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
        return Some(seen);
    }

    if matches!(completion_action, CompletionAction::DeleteLive) {
        // A delete ends the convoy, never deletes it (#3108); what it held is released.
        let holder = flotilla_resources::convoy::engine::checkout_holder(&convoys.get(&convoy_name).await.expect("convoy"))
            .expect("held for its launch");
        let reads = daemon.lifecycle_reads();
        assert!(
            reads
                .checkouts()
                .and_then(|checkouts| checkouts.held(&holder))
                .expect("held checkouts")
                .iter()
                .any(|checkout| !checkout.ended()),
            "the running convoy holds a live checkout"
        );
        let delete_id = daemon
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyDelete {
                        namespace: Some(NAMESPACE.to_string()),
                        name: format!("convoy-a@{NAMESPACE}"),
                        force: true,
                    })
                    .build(),
            )
            .await
            .expect("convoy delete command should start");
        assert_eq!(wait_for_command_result(&mut rx, delete_id).await, CommandValue::Ok);
        wait_until(|| {
            let convoys = convoys.clone();
            let convoy_name = convoy_name.clone();
            let terminals = backend.using::<TerminalSession>(NAMESPACE);
            let vessel_released = reads
                .vessels()
                .and_then(|vessels| vessels.get(&launch, None))
                .is_ok_and(|record| record.is_some_and(|record| record.ended()));
            let checkouts_released = reads
                .checkouts()
                .and_then(|checkouts| checkouts.held(&holder))
                .is_ok_and(|held| !held.is_empty() && held.iter().all(|checkout| checkout.ended()));
            async move {
                let abandoned = convoys
                    .get(&convoy_name)
                    .await
                    .ok()
                    .and_then(|convoy| convoy.status)
                    .is_some_and(|status| status.phase == ConvoyPhase::Abandoned);
                let sessions_stopped = terminals.list().await.is_ok_and(|list| {
                    list.items.iter().all(|session| {
                        session.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::TerminalSessionPhase::Stopped)
                    })
                });
                abandoned && vessel_released && checkouts_released && sessions_stopped
            }
        })
        .await;
        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
        return None;
    }

    let teardown_checkout_path = if matches!(completion_action, CompletionAction::Delete) {
        // The convoy's checkout, held for the launch its record names.
        let convoy = convoys.get(&convoy_name).await.expect("convoy");
        let holder = flotilla_resources::convoy::engine::checkout_holder(&convoy).expect("held for its launch");
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

    record_successful_empty_branch_scan(&daemon, &convoy_name).await;
    daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyWorkForceComplete {
                    convoy: format!("convoy-a@{NAMESPACE}"),
                    work: "implement".to_string(),
                    message: Some(match completion_action {
                        CompletionAction::Delete => "https://github.com/flotilla-org/flotilla/pull/884".to_string(),
                        CompletionAction::Retain | CompletionAction::DeleteLive | CompletionAction::ChangeRequest { .. } => {
                            "done".to_string()
                        }
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
        let convoy_name = convoy_name.clone();
        async move {
            matches!(
                convoys.get(&convoy_name).await.ok().and_then(|convoy| convoy.status).as_ref(),
                Some(status)
                    if status.phase == ConvoyPhase::Landed
                        && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Complete)
            )
        }
    })
    .await;

    if let Some(checkout_path) = teardown_checkout_path {
        // A released launch stays as history (#3108): the vessel is gone once its launch ends.
        let reads = daemon.lifecycle_reads();
        wait_until(|| {
            let convoys = convoys.clone();
            let checkout_path = checkout_path.clone();
            let convoy_name = convoy_name.clone();
            let released = reads
                .vessels()
                .and_then(|vessels| vessels.get(&launch, None))
                .is_ok_and(|record| record.is_some_and(|record| record.ended()));
            async move {
                convoys
                    .get(&convoy_name)
                    .await
                    .ok()
                    .and_then(|convoy| convoy.status)
                    .is_some_and(|status| status.phase == ConvoyPhase::Landed)
                    && released
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
    None
}

/// Runs the `ChangeRequest` completion of running convoy `convoy_name`, and reads what became of
/// its checkout once the convoy ended, or once its release had long enough to finish.
async fn release_after_change_request(
    daemon: &Arc<InProcessDaemon>,
    convoy_name: &str,
    vessel: &str,
    repo_default_dir: &Path,
    state: flotilla_resources::ObservedChangeRequestState,
    untracked: bool,
) -> ReleaseSeen {
    let backend = daemon.resource_backend();
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    // The checkout is held for the convoy's launch, `<name>#<n>`, as the holder names it.
    let holder =
        flotilla_resources::convoy::engine::checkout_holder(&convoys.get(convoy_name).await.expect("convoy")).expect("held for its launch");
    let reads = daemon.lifecycle_reads();
    let held = || reads.checkouts().and_then(|checkouts| checkouts.held(&holder)).expect("held checkouts");
    let checkout = held().into_iter().find(|checkout| !checkout.ended()).expect("convoy checkout should exist");
    let launch = checkout.name.clone();
    let checkout_path = checkout.ready_path().expect("checkout should be ready under its path").to_owned();

    // The work commits on its branch and does not push it.
    fs::write(Path::new(&checkout_path).join("work.txt"), "the work\n").expect("write work");
    let git = |args: &[&str]| {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(&checkout_path)
            .args(["-c", "user.name=Flotilla Test", "-c", "user.email=test@flotilla.invalid"])
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("utf-8 git output").trim().to_owned()
    };
    git(&["add", "work.txt"]);
    git(&["commit", "-m", "the work"]);
    let head = git(&["rev-parse", "HEAD"]);
    if untracked {
        fs::write(Path::new(&checkout_path).join("left-over.pyc"), "test run output\n").expect("write untracked file");
    }

    record_successful_empty_branch_scan(daemon, convoy_name).await;
    let engine_path = daemon.convoy_engine_path();
    let engine = engine_path.get().expect("the engine's convoys");
    let current = convoys.get(convoy_name).await.expect("convoy");
    engine.verdict(&current, "template/decision-ledger", true, "operator", "ledger seen").await.expect("ledger kept");
    // The coder submits its PR, as `crew promise submit` does.
    let mut rx = daemon.subscribe();
    let submit = daemon
        .execute(
            Command::builder()
                .action(CommandAction::CrewPromise {
                    context: CrewCommandContext {
                        crew_id: None,
                        namespace: Some(NAMESPACE.to_string()),
                        convoy: Some(convoy_name.to_string()),
                        vessel_ref: Some(vessel.to_string()),
                        role: Some("coder".to_string()),
                    },
                    operation: flotilla_protocol::commands::CrewPromiseOperation::Submit {
                        promise: Some("template/pr".to_string()),
                        kind: "pr".to_string(),
                        reference: "https://github.com/flotilla-org/flotilla/pull/884".to_string(),
                        metadata: BTreeMap::new(),
                    },
                })
                .build(),
        )
        .await
        .expect("submit the PR");
    assert_eq!(wait_for_command_result(&mut rx, submit).await, CommandValue::Ok);
    publish_change_request(&backend, 884, "test-host", state, &head).await;
    if state == flotilla_resources::ObservedChangeRequestState::Closed {
        // The close rejects the submission, and the operator then retracts the promise.
        let pr_open_again = || {
            let convoys = convoys.clone();
            async move {
                convoys.get(convoy_name).await.ok().and_then(|convoy| convoy.status).is_some_and(|status| {
                    flotilla_resources::promises::owned(&status, "implement", "coder").iter().any(|promise| {
                        promise.id == "template/pr"
                            && promise.state == flotilla_resources::promises::PromiseState::Open
                            && !promise.submissions.is_empty()
                    })
                })
            }
        };
        wait_until_with_timeout(Duration::from_secs(20), pr_open_again).await;
        let current = convoys.get(convoy_name).await.expect("convoy");
        engine.verdict(&current, "template/pr", false, "operator", "closed unmerged; retraction accepted").await.expect("retract");
    }

    // The release ends the launch; one the removal refuses is given the same time to end.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !held().iter().any(|checkout| checkout.name == launch && checkout.ended()) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let checkout = held().into_iter().find(|checkout| checkout.name == launch).expect("the checkout launch");
    let inspection = checkout.integration.as_ref().map(|integration| &integration.inspection);
    ReleaseSeen {
        reading: checkout.reading.clone(),
        landed: inspection.and_then(|inspection| inspection.landed),
        landed_details: inspection.map(|inspection| inspection.landed_details.clone()).unwrap_or_default(),
        on_disk: Path::new(&checkout_path).exists(),
        // Archives are saved beside the clone a worktree is of, which may sit beside the
        // repositories' directory rather than in it.
        archives: saved_archives(repo_default_dir.parent().unwrap_or(repo_default_dir)),
        convoy: convoys.get(convoy_name).await.ok().and_then(|convoy| convoy.status).map(|status| status.phase),
    }
}

/// The checkout archives saved under `root`, in any `.flotilla-archives` below it.
fn saved_archives(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else { return 0 };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            if entry.file_name() == ".flotilla-archives" {
                fs::read_dir(entry.path())
                    .map(|archives| {
                        archives.flatten().filter(|archive| !archive.file_name().to_string_lossy().starts_with(".partial-")).count()
                    })
                    .unwrap_or(0)
            } else {
                saved_archives(&entry.path())
            }
        })
        .sum()
}

pub(super) async fn record_successful_empty_branch_scan(daemon: &InProcessDaemon, convoy: &str) {
    flotilla_store::note_convoy(
        &daemon.convoy_engine_path(),
        &daemon.resource_backend().using::<Convoy>(NAMESPACE),
        convoy,
        &flotilla_resources::ConvoyStatusPatch::RecordBranchSubjectScan { at: chrono::Utc::now() },
    )
    .await
    .expect("record empty branch discovery for test provider");
}

/// What creation reports for `launch` admitted in `project`: the launch's address.
pub(super) fn created_launch(project: &str, launch: &str) -> CommandValue {
    let project = flotilla_address::Address::project(project).expect("project address");
    CommandValue::ConvoyCreated {
        name: flotilla_resources::convoy::engine::launch_address(&project, launch).expect("launch address").to_string(),
    }
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
