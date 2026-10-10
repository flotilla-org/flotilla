use super::*;
use flotilla_core::forge_observation::{source_owner, ForgeReads};
use flotilla_discovery_testkit::FakeIssueProvider;
use flotilla_resources::{forge_read_name, ForgeRead, ForgeReadHeartbeat, ForgeReadRequest, IssueSourceBindingSpec, Project, ProjectSpec};

// A demand first authored on a nonowner crosses the production registration,
// routed watches and replica store. Only the owner calls the forge; its answer
// must become readable at the requester without invoking the requester's loader.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn nonowner_issue_demand_returns_owner_value() {
    let temp = tempfile::tempdir().expect("owner config");
    let provider = Arc::new(FakeIssueProvider::new()); // External forge boundary.
    provider.add_issues(vec![("327".into(), TestIssue::new("Dispatch this issue").id("327").build())]).await;
    let owner = InProcessDaemon::new(
        vec![],
        test_config_store(temp.path().join("config")),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_issue_tracker(provider.clone())),
        HostName::new("forge-owner"),
    )
    .await;
    let requester = empty_daemon_named("forge-requester").await;
    let source = IssueSource { service: "https://github.com".into(), scope: "org/shared".into() };
    owner
        .resource_backend()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("shared".into()).build(),
            &ProjectSpec::builder()
                .display_name("Shared".into())
                .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source.clone()).alias("shared".into()).build()])
                .build(),
        )
        .await
        .expect("owner declaration");
    let mesh = spawn_in_memory_request_mesh_with_replication_kinds(
        vec![owner.clone(), requester.clone()],
        Some(&[Project::API_PATHS.kind, Host::API_PATHS.kind, ForgeRead::API_PATHS.kind, ForgeReadHeartbeat::API_PATHS.kind]),
    )
    .await
    .expect("replication mesh");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "source owner visible", || async {
        source_owner(&requester.resource_backend(), "flotilla", &source).await == Ok(Some(owner.node_id().clone()))
    })
    .await;
    let reads = ForgeReads::new(requester.resource_backend(), "flotilla".into());
    let request = ForgeReadRequest::Issue { id: "327".into() };
    let read = || reads.read::<Issue, _, _>(&source, request.clone(), || async { panic!("nonowner contacted forge") });
    assert_eq!(read().await.unwrap_err(), "forge observation pending at source owner");
    let name = forge_read_name(&source, &request);
    eventually(Duration::from_secs(5), Duration::from_millis(10), "owner receives remote demand", || async {
        owner.resource_backend().including_replicas::<ForgeRead>("flotilla").get(&name).await.is_ok()
            && owner.resource_backend().including_replicas::<ForgeReadHeartbeat>("flotilla").get(&name).await.is_ok()
    })
    .await;
    owner.refresh_forge_read_demands().await.expect("service remote demand");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "answer returns to requester", || async {
        read().await.is_ok_and(|issue| issue.title == "Dispatch this issue" && issue.reference.id == "327")
    })
    .await;
    assert_eq!(*provider.fetched_by_id.lock().await, vec![vec!["327".to_string()]]);
    drop(mesh);
}
