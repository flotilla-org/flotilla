use super::*;

// Tracker boundary: deliberately suspend native forge observations so the board
// scenario can prove interactive reads never wait for remote work.
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
